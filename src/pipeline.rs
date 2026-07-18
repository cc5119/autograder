use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::Result;
use crate::evaluator::Evaluator;
use crate::fetch::Fetchable;
use crate::grade::Grader;
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus, Tier,
};
use crate::overrides::{self, Overrides};
use crate::source::SubmissionsSource;
use crate::spec::Spec;
use crate::store::Store;

static RUN_COUNTER: AtomicU32 = AtomicU32::new(0);

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
        tier: ctx.tier,
        assignment_id: ctx.assignment_id.clone(),
        student_id: ctx.student_id.clone(),
        run_id: ctx.run_id.clone(),
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

pub(crate) fn generate_run_id() -> String {
    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{:04x}",
        chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ"),
        n
    )
}

/// Stage orchestration for the authoritative-tier `grade` pipeline:
/// Fetch -> Prepare -> Evaluate -> persist -> Grade -> apply overrides, one
/// student at a time.
///
/// Generic over the fetchable type `F: Fetchable`: `source` yields
/// `Submission<F>`s and each one fetches itself, so the compiler rejects
/// ever pairing e.g. a `CsvRoster` (`SubmissionsSource<GitRepo>`) with a
/// workspace that only makes sense for a `LocalPath`.
///
/// `overrides` (design §14, §18.2 -- M5 step 24) is applied to the `Grade`
/// after `grader.grade` runs, never touching the persisted `eval` -- see
/// `overrides::apply`'s doc comment for why a manual override or late
/// penalty is recomputed here rather than baked into the raw result.
#[allow(clippy::too_many_arguments)]
pub fn grade_batch<F: Fetchable>(
    source: &dyn SubmissionsSource<F>,
    evaluator: &dyn Evaluator,
    grader: &dyn Grader,
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
        // `workspace` and `driver_dir` are siblings under one per-job root,
        // never nested inside each other -- see `JobContext`'s doc comment.
        let job_root = work_dir.join(&submission.student_id);
        let workspace = job_root.join("student");
        let driver_dir = job_root.join("driver");
        let ctx = JobContext {
            assignment_id: spec.assignment.id.clone(),
            student_id: submission.student_id.clone(),
            run_id: run_id.clone(),
            tier: Tier::Authoritative,
            workspace: workspace.clone(),
            driver_dir: driver_dir.clone(),
        };

        let fetch_outcome = submission.fetch(&workspace)?;

        let eval = if fetch_outcome.status != StageStatus::Ok {
            terminal_eval(
                &ctx,
                StageStatus::FetchFailed,
                fetch_outcome.message.clone(),
            )
        } else {
            let prepared = crate::prepare::prepare(
                &workspace,
                &driver_dir,
                package_dir,
                spec,
                Tier::Authoritative,
            )?;
            if !prepared.manifest_diagnostics.is_empty() {
                let message = prepared
                    .manifest_diagnostics
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                terminal_eval(&ctx, StageStatus::DisallowedDependency, Some(message))
            } else {
                evaluator.evaluate(&ctx)?
            }
        };

        store.save_eval(&eval)?;
        let grade = grader.grade(&eval, &spec.scoring);
        let grade = overrides::apply(
            grade,
            overrides,
            spec.assignment.deadline,
            spec.scoring.late_penalty.as_ref(),
        );
        store.save_grade(&ctx.assignment_id, &ctx.run_id, &grade)?;
        grades.push(grade);
    }

    Ok(grades)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grade::DefaultGrader;
    use crate::model::LocalPath;
    use crate::source::SubmissionsSource;

    struct FixedSource(Vec<crate::model::Submission<LocalPath>>);
    impl SubmissionsSource<LocalPath> for FixedSource {
        fn submissions(&self) -> Result<Vec<crate::model::Submission<LocalPath>>> {
            Ok(self.0.clone())
        }
    }

    fn write(path: &std::path::Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const SPEC_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00"


[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]

[limits.build]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256

[limits.run]
cpu-time = "5s"
wall-clock = "10s"
cpus = 1
memory = "512MiB"
pids = 128
max-output-bytes = "1MiB"

[scoring]
model = "weighted"

[[scoring.tests]]
name = "insert_basic"
points = 10
visibility = "public"
"#;

    #[test]
    fn grade_batch_runs_end_to_end_over_a_directory_submission() {
        let package_dir = tempfile::tempdir().unwrap();
        let submission_src = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();

        write(&submission_src.path().join("src/lib.rs"), "// student code");

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let source = FixedSource(vec![crate::model::Submission {
            student_id: "alice".into(),
            fetchable: LocalPath(submission_src.path().to_path_buf()),
            metadata: Default::default(),
        }]);
        let evaluator = crate::evaluator::StubEvaluator {
            tests: spec.scoring.tests.clone(),
        };
        let store = Store::new(store_dir.path());

        let grades = grade_batch(
            &source,
            &evaluator,
            &DefaultGrader,
            package_dir.path(),
            &spec,
            work_dir.path(),
            &store,
            &Overrides::default(),
        )
        .unwrap();

        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].student_id, "alice");
        assert_eq!(grades[0].score, 10.0);
        assert_eq!(grades[0].max, 10.0);

        let persisted = store.latest_evals("hw3").unwrap();
        assert_eq!(persisted.len(), 1);
        let persisted_grades = store.latest_grades("hw3").unwrap();
        assert_eq!(persisted_grades.len(), 1);
    }

    #[test]
    fn grade_batch_scores_zero_for_a_disallowed_dependency_without_running_the_evaluator() {
        let package_dir = tempfile::tempdir().unwrap();
        let submission_src = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();

        write(&submission_src.path().join("src/lib.rs"), "// student code");
        write(
            &submission_src.path().join("Cargo.toml"),
            "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\ntokio = \"1\"\n",
        );

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let source = FixedSource(vec![crate::model::Submission {
            student_id: "alice".into(),
            fetchable: LocalPath(submission_src.path().to_path_buf()),
            metadata: Default::default(),
        }]);
        // A StubEvaluator that would score everything as passing if it ever
        // ran — the assertion below only holds if the pipeline actually
        // short-circuits before reaching it.
        let evaluator = crate::evaluator::StubEvaluator {
            tests: spec.scoring.tests.clone(),
        };
        let store = Store::new(store_dir.path());

        let grades = grade_batch(
            &source,
            &evaluator,
            &DefaultGrader,
            package_dir.path(),
            &spec,
            work_dir.path(),
            &store,
            &Overrides::default(),
        )
        .unwrap();

        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].score, 0.0);
        assert_eq!(grades[0].status, "DisallowedDependency");

        let persisted = store.latest_evals("hw3").unwrap();
        assert_eq!(persisted.len(), 1);
        assert!(
            persisted[0]
                .diagnostics
                .stderr_excerpt
                .as_deref()
                .unwrap()
                .contains("tokio")
        );
    }

    #[test]
    fn grade_batch_handles_fetch_failure_without_aborting_the_batch() {
        let package_dir = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let source = FixedSource(vec![crate::model::Submission {
            student_id: "ghost".into(),
            fetchable: LocalPath("/nonexistent/path".into()),
            metadata: Default::default(),
        }]);
        let evaluator = crate::evaluator::StubEvaluator {
            tests: spec.scoring.tests.clone(),
        };
        let store = Store::new(store_dir.path());

        let grades = grade_batch(
            &source,
            &evaluator,
            &DefaultGrader,
            package_dir.path(),
            &spec,
            work_dir.path(),
            &store,
            &Overrides::default(),
        )
        .unwrap();

        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].score, 0.0);
        assert_eq!(grades[0].status, "FetchFailed");
    }

    #[test]
    fn grade_batch_applies_a_manual_override_after_grading() {
        let package_dir = tempfile::tempdir().unwrap();
        let submission_src = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();

        write(&submission_src.path().join("src/lib.rs"), "// student code");

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let source = FixedSource(vec![crate::model::Submission {
            student_id: "alice".into(),
            fetchable: LocalPath(submission_src.path().to_path_buf()),
            metadata: Default::default(),
        }]);
        let evaluator = crate::evaluator::StubEvaluator {
            tests: spec.scoring.tests.clone(),
        };
        let store = Store::new(store_dir.path());
        let overrides = Overrides {
            manual: std::collections::BTreeMap::from([(
                "alice".to_string(),
                crate::overrides::ManualOverride {
                    score: 3.0,
                    status: Some("manual-review".into()),
                    reason: "Partial credit for a documented edge case".into(),
                },
            )]),
            late: Default::default(),
        };

        let grades = grade_batch(
            &source,
            &evaluator,
            &DefaultGrader,
            package_dir.path(),
            &spec,
            work_dir.path(),
            &store,
            &overrides,
        )
        .unwrap();

        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].score, 3.0);
        assert_eq!(grades[0].status, "manual-review");
        assert!(grades[0].override_reason.is_some());

        // The raw persisted eval is untouched by the override -- only the
        // derived Grade reflects it.
        let persisted = store.latest_evals("hw3").unwrap();
        assert_eq!(persisted[0].tests[0].status, crate::model::TestStatus::Pass);
    }
}
