pub mod evaluator;
pub mod grade;
pub mod manifest_check;
pub mod prepare;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::json::write_json;
use crate::exec::overlay::{self, Context, Rule};
use crate::id::{RunId, SubmissionId};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus,
};
use crate::pipeline::evaluator::Evaluator;
use crate::spec::Spec;

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
fn list_submissions(submissions_dir: &Path) -> Result<Vec<SubmissionId>> {
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
        ids.push(SubmissionId::new(name));
    }
    ids.sort();
    Ok(ids)
}

/// Everything copied from the student's own fetched checkout: the untrusted submission.
fn checkout_rules() -> Vec<Rule> {
    vec![Rule::Glob("{id}/**", None)]
}

/// Everything copied from the instructor's private package: the trusted judge.
fn package_rules() -> Vec<Rule> {
    vec![
        Rule::File("Cargo.toml", None),
        Rule::Glob("{harness}/**", None),
    ]
}

/// Builds an `EvaluationResult` for a build-stage failure that happened
/// before Evaluate ever ran (missing crate dir, disallowed dependency), so
/// a non-fatal per-submission problem still produces a well-formed,
/// gradeable result instead of aborting the batch.
fn terminal_eval(
    ctx: &JobContext,
    status: StageStatus,
    message: Option<String>,
) -> EvaluationResult {
    EvaluationResult {
        schema_version: 1,
        assignment_id: ctx.assignment_id,
        submission_id: ctx.submission_id,
        run_id: ctx.run_id,
        graded_commit: None,
        instructor_commit: None,
        public_harness_commit: None,
        stages: StageReports {
            build: StageReport {
                status,
                duration_ms: None,
                warnings: None,
            },
            run: StageReport::ok(),
        },
        tests: Vec::new(),
        resource_usage: ResourceUsage::default(),
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

/// Runs Prepare -> Evaluate for a single submission checkout on disk.
/// Early-returns a `terminal_eval` at the first stage that didn't succeed
/// (missing crate, disallowed dependency); only a fully prepared workspace
/// reaches `evaluator.evaluate`.
fn evaluate_submission(
    ctx: &JobContext,
    checkout_dir: &Path,
    build_dir: &Path,
    package_dir: &Path,
    workspace: &Path,
    spec: &Spec,
    evaluator: &dyn Evaluator,
) -> Result<EvaluationResult> {
    let submitted_crate = checkout_dir.join(spec.assignment.id.as_str());
    if !submitted_crate.is_dir() {
        return Ok(terminal_eval(
            ctx,
            StageStatus::BuildFailed,
            Some(format!(
                "submission has no {:?} directory -- expected the student's own crate \
                 there, matching [assignment].id",
                spec.assignment.id
            )),
        ));
    }

    let subs = HashMap::from([
        ("id", spec.assignment.id.to_string()),
        ("harness", spec.assignment.harness.clone()),
    ]);
    overlay::apply(
        &Context {
            source_root: checkout_dir.to_path_buf(),
            substitutions: subs.clone(),
        },
        build_dir,
        &checkout_rules(),
    )?;
    overlay::apply(
        &Context {
            source_root: package_dir.to_path_buf(),
            substitutions: subs,
        },
        build_dir,
        &package_rules(),
    )?;

    // The sandboxed container process runs as an unprivileged,
    // rootless-podman-remapped uid, so we must grant "other" write
    // access to `build_dir` so it can create new entries (Cargo.lock, target/).
    let mut perms = crate::exec::fs::metadata(build_dir)?.permissions();
    perms.set_mode(perms.mode() | 0o002);
    crate::exec::fs::set_permissions(build_dir, perms)?;

    let prepared = crate::pipeline::prepare::prepare(workspace, package_dir, spec)?;
    if !prepared.manifest_diagnostics.is_empty() {
        let message = prepared
            .manifest_diagnostics
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Ok(terminal_eval(
            ctx,
            StageStatus::DisallowedDependency,
            Some(message),
        ));
    }
    evaluator.evaluate(ctx)
}

fn save_eval(submissions_dir: &Path, eval: &EvaluationResult) -> Result<()> {
    let path = submissions_dir
        .join(EVAL_DIR)
        .join(eval.submission_id.as_str())
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
/// persisted at `submissions_dir/.eval/<submission_id>/<run_id>.eval.json`.
/// No scoring happens here -- run `autograder grade` afterwards for that.
pub fn evaluate_batch(
    submissions_dir: &Path,
    evaluator: &dyn Evaluator,
    package_dir: &Path,
    spec: &Spec,
) -> Result<Vec<EvaluationResult>> {
    let submission_ids = list_submissions(submissions_dir)?;
    let mut evals = Vec::new();

    for submission_id in submission_ids {
        let run_id = generate_run_id();
        let checkout_dir = submissions_dir.join(submission_id.as_str());
        // A fresh OS temp dir per submission -- dropped (and cleaned up)
        // automatically at the end of this iteration, no manual cleanup.
        let build_scratch = tempfile::tempdir().map_err(|source| {
            Error::Other(format!("failed to create a scratch build dir: {source}"))
        })?;
        let build_dir = build_scratch.path();
        // `workspace` is named after `[assignment].id`, not e.g. "student":
        // the harness's checked-in Cargo.toml depends on that exact sibling
        // name (see `evaluator::library`'s module doc comment).
        let workspace = build_dir.join(spec.assignment.id.as_str());
        let ctx = JobContext {
            assignment_id: spec.assignment.id,
            submission_id,
            run_id,
            workspace: workspace.clone(),
        };

        let eval = evaluate_submission(
            &ctx,
            &checkout_dir,
            build_dir,
            package_dir,
            &workspace,
            spec,
            evaluator,
        )?;

        save_eval(submissions_dir, &eval)?;
        evals.push(eval);
    }

    Ok(evals)
}

// This module's private helpers have no dedicated tests of their own --
// their behavior lives in `tests/pipeline.rs` as an integration test instead
// (see that file's doc comment).
