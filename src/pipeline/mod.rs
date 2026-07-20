pub mod evaluator;
pub mod grade;
pub mod manifest_check;
pub mod overrides;
pub mod prepare;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::Result;
use crate::exec::overlay::{self, Context, Rule};
use crate::id::RunId;
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus,
};
use crate::pipeline::evaluator::Evaluator;
use crate::pipeline::overrides::Overrides;
use crate::spec::Spec;
use crate::store::Store;
use crate::submissions::read_fetch_record;
use crate::submissions::source::SubmissionsSource;

static RUN_COUNTER: AtomicU32 = AtomicU32::new(0);

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

/// Builds an `EvaluationResult` for a stage that failed before Evaluate
/// ever ran (fetch failure, disallowed dependency), so a non-fatal
/// per-student problem still produces a well-formed, gradeable result
/// instead of aborting the batch.
fn terminal_eval(
    ctx: &JobContext,
    status: StageStatus,
    message: Option<String>,
) -> EvaluationResult {
    EvaluationResult {
        schema_version: 1,
        assignment_id: ctx.assignment_id,
        student_id: ctx.student_id,
        run_id: ctx.run_id,
        graded_commit: None,
        instructor_commit: None,
        public_harness_commit: None,
        stages: StageReports {
            fetch: if status == StageStatus::FetchFailed {
                StageReport {
                    status,
                    duration_ms: None,
                    warnings: None,
                }
            } else {
                StageReport::ok()
            },
            build: if status == StageStatus::FetchFailed {
                StageReport::ok()
            } else {
                StageReport {
                    status,
                    duration_ms: None,
                    warnings: None,
                }
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

/// Runs Prepare -> Evaluate for a single submission, given a prior fetch's
/// output on disk. Early-returns a `terminal_eval` at the first stage that
/// didn't succeed (no fetch record, failed fetch, missing crate, disallowed
/// dependency); only a fully prepared workspace reaches `evaluator.evaluate`.
#[allow(clippy::too_many_arguments)]
fn evaluate_submission(
    ctx: &JobContext,
    job_root: &Path,
    checkout_dir: &Path,
    build_dir: &Path,
    package_dir: &Path,
    workspace: &Path,
    spec: &Spec,
    evaluator: &dyn Evaluator,
) -> Result<EvaluationResult> {
    let Some(fetch_record) = read_fetch_record(job_root)? else {
        return Ok(terminal_eval(
            ctx,
            StageStatus::FetchFailed,
            Some(format!(
                "no prior fetch found for {} -- run `autograder fetch` first, or pass \
                 --fetch to grade",
                ctx.student_id
            )),
        ));
    };
    if fetch_record.status != StageStatus::Ok {
        return Ok(terminal_eval(
            ctx,
            StageStatus::FetchFailed,
            fetch_record.message,
        ));
    }

    let submitted_crate = checkout_dir.join(spec.assignment.id.as_str());
    if !submitted_crate.is_dir() {
        return Ok(terminal_eval(
            ctx,
            StageStatus::FetchFailed,
            Some(format!(
                "fetched checkout has no {:?} directory -- expected the student's own \
                 crate there, matching [assignment].id",
                spec.assignment.id
            )),
        ));
    }

    let outcome: Result<EvaluationResult> = (|| {
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
    })();

    if let Err(io_err) = std::fs::remove_dir_all(build_dir)
        && io_err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %build_dir.display(),
            error = %io_err,
            "failed to clean up scratch build directory after grading"
        );
    }

    outcome
}

/// Stage orchestration for the authoritative-tier `grade` pipeline:
/// Prepare -> Evaluate -> persist -> Grade -> apply overrides, one student
/// at a time. Fetch is a separate stage (`crate::submissions::fetch_batch`, run
/// via `autograder fetch` or `grade --fetch`); this function only *reads*
/// what a prior fetch left behind (`job_root/checkout/` and its
/// `FetchRecord`) -- a student with no record gets the same `FetchFailed`
/// result as one whose fetch itself failed, and the batch never aborts for
/// one student either way.
#[allow(clippy::too_many_arguments)]
pub fn grade_batch<F>(
    source: &dyn SubmissionsSource<F>,
    evaluator: &dyn Evaluator,
    package_dir: &Path,
    spec: &Spec,
    work_dir: &Path,
    store: &Store,
    overrides: &Overrides,
) -> Result<Vec<crate::model::Grade>> {
    let submissions = source.submissions()?;
    let mut grades = Vec::new();

    for submission in submissions {
        let run_id = generate_run_id();
        let job_root = work_dir.join(submission.student_id.as_str());
        let checkout_dir = job_root.join("checkout");
        // `workspace` is named after `[assignment].id`, not e.g. "student":
        // the harness's checked-in Cargo.toml depends on that exact sibling
        // name (see `evaluator::library`'s module doc comment).
        let build_dir = job_root.join("build");
        let workspace = build_dir.join(spec.assignment.id.as_str());
        let ctx = JobContext {
            assignment_id: spec.assignment.id,
            student_id: submission.student_id,
            run_id,
            workspace: workspace.clone(),
        };

        let eval = evaluate_submission(
            &ctx,
            &job_root,
            &checkout_dir,
            &build_dir,
            package_dir,
            &workspace,
            spec,
            evaluator,
        )?;

        store.save_eval(&eval)?;
        let grade = crate::pipeline::grade::grade(&eval, &spec.scoring);
        let grade = overrides::apply(
            grade,
            overrides,
            &spec.assignment.deadline,
            spec.scoring.late_penalty.as_ref(),
        );
        store.save_grade(ctx.assignment_id, ctx.run_id, &grade)?;
        grades.push(grade);
    }

    Ok(grades)
}

// This module's private helpers have no dedicated tests of their own --
// their behavior lives in `tests/pipeline.rs` as an integration test instead
// (see that file's doc comment).
