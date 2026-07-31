pub mod evaluator;
pub mod grade;
pub mod manifest_check;
pub mod prepare;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use indicatif::{ProgressBar, ProgressStyle};

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::json::write_json;
use crate::exec::overlay::{self, Context, Rule};
use crate::exec::sandbox::ProcessStatus;
use crate::id::{RunId, StudentId};
use crate::model::{BuildStatus, Diagnostics, EvalStatus, EvaluationResult, JobContext};
use crate::pipeline::evaluator::Evaluator;
use crate::spec::Spec;
use crate::str_map;

static RUN_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Where `evaluate_batch` persists each `EvaluationResult`, alongside (not
/// inside) the submission checkouts under `submissions_dir`.
const EVAL_DIR: &str = ".eval";

/// One entry per submission under `submissions_dir`: every direct
/// subdirectory whose name doesn't start with `.` -- the fetch/eval/grade
/// stages each keep their own dot-prefixed directory alongside the actual
/// submission checkouts, and skipping by that one rule means evaluate never
/// needs to know their names. Evaluate reads nothing else about how a
/// checkout got there -- no fetch record required or consulted -- so a
/// directory dropped in by hand works exactly like one `autograder fetch`
/// produced. Sorted by name.
fn list_submissions(submissions_dir: &Path) -> Result<Vec<StudentId>> {
    if !submissions_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir_entries(submissions_dir)? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        ids.push(StudentId::new(name));
    }
    ids.sort();
    Ok(ids)
}

/// Everything copied from the student's own fetched checkout: the untrusted submission.
pub(crate) fn checkout_rules() -> Vec<Rule> {
    vec![Rule::Glob("{id}/**", None)]
}

/// Everything copied from the instructor's private package: the trusted judge.
pub(crate) fn package_rules() -> Vec<Rule> {
    vec![
        Rule::File("Cargo.toml", None),
        Rule::File("Cargo.lock", None),
        Rule::Glob("{harness}/**", None),
    ]
}

/// Builds an `EvaluationResult` for a build-stage failure that happened
/// before Evaluate ever ran (missing crate dir, disallowed dependency), so
/// a non-fatal per-submission problem still produces a well-formed,
/// gradeable result instead of aborting the batch.
fn terminal_eval(
    ctx: &JobContext,
    status: BuildStatus,
    message: Option<String>,
) -> EvaluationResult {
    EvaluationResult {
        assignment_id: ctx.assignment_id,
        student_id: ctx.student_id,
        run_id: ctx.run_id,
        graded_commit: None,
        instructor_commit: None,
        status: EvalStatus::BuildFailed(status),
        wall_clock_ms: None,
        diagnostics: Diagnostics {
            compiler_errors: None,
            stderr_excerpt: message,
        },
    }
}

pub(crate) fn generate_run_id() -> RunId {
    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    RunId::new(format!(
        "{}-{:04x}",
        jiff::Timestamp::now().strftime("%Y-%m-%dT%H-%M-%SZ"),
        n
    ))
}

/// Runs Prepare -> Evaluate for a single submission checkout on disk, into
/// `ctx.workspace` (see `JobContext`'s doc comment for the layout).
/// Early-returns a `terminal_eval` at the first stage that didn't succeed
/// (missing package, disallowed dependency).
pub(crate) fn evaluate_submission(
    ctx: &JobContext,
    checkout_dir: &Path,
    assignment_dir: &Path,
    spec: &Spec,
    evaluator: &dyn Evaluator,
) -> Result<EvaluationResult> {
    let submitted_package = checkout_dir.join(spec.assignment.id.as_str());
    if !submitted_package.is_dir() {
        return Ok(terminal_eval(
            ctx,
            BuildStatus::Failed(ProcessStatus::Unknown),
            Some(format!(
                "submission has no {:?} directory -- expected the student's own package \
                 there, matching [assignment].id",
                spec.assignment.id
            )),
        ));
    }

    let subs = str_map! {"id" => spec.assignment.id, "harness" => spec.assignment.harness};
    overlay::apply(
        &Context::new(checkout_dir, subs.clone()),
        &ctx.workspace,
        &checkout_rules(),
    )?;
    overlay::apply(
        &Context::new(assignment_dir, subs),
        &ctx.workspace,
        &package_rules(),
    )?;

    // The sandboxed container process runs as an unprivileged,
    // rootless-podman-remapped uid, so we must grant "other" write
    // access to `ctx.workspace` so it can create new entries (Cargo.lock, target/).
    let mut perms = crate::exec::fs::metadata(&ctx.workspace)?.permissions();
    perms.set_mode(perms.mode() | 0o002);
    crate::exec::fs::set_permissions(&ctx.workspace, perms)?;

    let prepared = crate::pipeline::prepare::prepare(ctx, assignment_dir, spec)?;
    if !prepared.manifest_diagnostics.is_empty() {
        let message = prepared
            .manifest_diagnostics
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Ok(terminal_eval(
            ctx,
            BuildStatus::DisallowedDependency,
            Some(message),
        ));
    }
    evaluator.evaluate(ctx)
}

fn save_eval(submissions_dir: &Path, eval: &EvaluationResult) -> Result<()> {
    let path = submissions_dir
        .join(EVAL_DIR)
        .join(eval.student_id.as_str())
        .join(format!("{}.eval.json", eval.run_id));
    write_json(&path, eval)
}

/// Stage orchestration for `autograder evaluate`: Prepare -> Evaluate ->
/// persist, one submission at a time. Evaluate never touches the Fetch
/// stage's own records at all -- it just enumerates the submission
/// checkouts already sitting under `submissions_dir` (see
/// `list_submissions`) -- so a submission never aborts the batch, and
/// which student (if any) a submission maps to is entirely `autograder
/// fetch`/`autograder grade`'s concern, not evaluate's. Each result is
/// persisted at `submissions_dir/.eval/<student_id>/<run_id>.eval.json`.
/// No scoring happens here -- run `autograder grade` afterwards for that.
///
/// Refuses up front, before touching any submission, if `assignment_dir`
/// hasn't been vendored or was vendored from a since-changed `Cargo.lock`
/// (`crate::deps::vendor::verify`) -- every sandboxed build runs
/// `--offline`, so an unvendored/stale dependency set would otherwise
/// surface as an opaque per-submission `build_failed` for every single
/// submission in the batch instead of one clear, actionable error.
pub fn evaluate_batch(
    submissions_dir: &Path,
    evaluator: &dyn Evaluator,
    assignment_dir: &Path,
    spec: &Spec,
) -> Result<Vec<EvaluationResult>> {
    if let Some(message) = crate::deps::vendor::verify(assignment_dir, spec) {
        return Err(Error::InvalidSpec(message));
    }

    let student_ids = list_submissions(submissions_dir)?;
    let mut evals = Vec::new();

    // A single reused spinner, not one `ProgressBar` per submission: each
    // iteration just changes its message/finishes it, so the terminal shows
    // one live "evaluating <id>..." line that's overwritten in place by the
    // result, rather than a scrolling log of everything printed so far.
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{spinner} {msg}").expect("static template is valid"),
    );
    progress.enable_steady_tick(std::time::Duration::from_millis(100));

    for student_id in student_ids {
        progress.set_message(format!("evaluating {student_id}..."));

        let run_id = generate_run_id();
        let checkout_dir = submissions_dir.join(student_id.as_str());
        // A fresh OS temp dir per submission, becoming `ctx.workspace`
        // (see `JobContext`'s doc comment for the layout) -- dropped and
        // cleaned up automatically at the end of this iteration.
        let build_scratch = fs::temp_dir()?;
        let ctx = JobContext {
            assignment_id: spec.assignment.id,
            student_id,
            run_id,
            workspace: build_scratch.path().to_path_buf(),
        };

        let eval = evaluate_submission(&ctx, &checkout_dir, assignment_dir, spec, evaluator)?;

        save_eval(submissions_dir, &eval)?;
        progress.suspend(|| println!("{}", eval.describe()));
        evals.push(eval);
    }
    progress.finish_and_clear();

    Ok(evals)
}

// This module's private helpers have no dedicated tests of their own --
// their behavior lives in `tests/pipeline.rs` as an integration test instead
// (see that file's doc comment).
