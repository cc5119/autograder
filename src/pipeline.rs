use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::Result;
use crate::evaluator::Evaluator;
use crate::fetch::read_fetch_record;
use crate::id::RunId;
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus,
};
use crate::overlay::{self, Context, Rule};
use crate::overrides::{self, Overrides};
use crate::source::SubmissionsSource;
use crate::spec::Spec;
use crate::store::Store;

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

/// Stage orchestration for the authoritative-tier `grade` pipeline:
/// Prepare -> Evaluate -> persist -> Grade -> apply overrides, one student
/// at a time. Fetch is a separate stage (`crate::fetch::fetch_batch`, run
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

        let fetch_record = read_fetch_record(&job_root)?;

        let eval = if fetch_record
            .as_ref()
            .is_none_or(|r| r.status != StageStatus::Ok)
        {
            let message = match &fetch_record {
                Some(r) => r.message.clone(),
                None => Some(format!(
                    "no prior fetch found for {} -- run `autograder fetch` first, or pass \
                     --fetch to grade",
                    submission.student_id
                )),
            };
            terminal_eval(&ctx, StageStatus::FetchFailed, message)
        } else {
            let submitted_crate = checkout_dir.join(spec.assignment.id.as_str());
            if !submitted_crate.is_dir() {
                terminal_eval(
                    &ctx,
                    StageStatus::FetchFailed,
                    Some(format!(
                        "fetched checkout has no {:?} directory -- expected the student's own \
                         crate there, matching [assignment].id",
                        spec.assignment.id
                    )),
                )
            } else {
                let outcome: Result<EvaluationResult> = (|| {
                    let subs = HashMap::from([
                        ("id", spec.assignment.id.to_string()),
                        ("harness", spec.assignment.harness.clone()),
                    ]);
                    overlay::apply(
                        &Context {
                            source_root: checkout_dir.clone(),
                            substitutions: subs.clone(),
                        },
                        &build_dir,
                        &checkout_rules(),
                    )?;
                    overlay::apply(
                        &Context {
                            source_root: package_dir.to_path_buf(),
                            substitutions: subs,
                        },
                        &build_dir,
                        &package_rules(),
                    )?;
                    let prepared = crate::prepare::prepare(&workspace, package_dir, spec)?;
                    if !prepared.manifest_diagnostics.is_empty() {
                        let message = prepared
                            .manifest_diagnostics
                            .iter()
                            .map(|d| d.to_string())
                            .collect::<Vec<_>>()
                            .join("; ");
                        Ok(terminal_eval(
                            &ctx,
                            StageStatus::DisallowedDependency,
                            Some(message),
                        ))
                    } else {
                        evaluator.evaluate(&ctx)
                    }
                })();

                if let Err(io_err) = std::fs::remove_dir_all(&build_dir)
                    && io_err.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        path = %build_dir.display(),
                        error = %io_err,
                        "failed to clean up scratch build directory after grading"
                    );
                }

                outcome?
            }
        };

        store.save_eval(&eval)?;
        let grade = crate::grade::grade(&eval, &spec.scoring);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LocalPath;
    use crate::source::SubmissionsSource;

    struct FixedSource(Vec<crate::model::Submission<LocalPath>>);
    impl SubmissionsSource<LocalPath> for FixedSource {
        fn submissions(&self) -> Result<Vec<crate::model::Submission<LocalPath>>> {
            Ok(self.0.clone())
        }
    }

    /// `grade_batch` only reads what a prior fetch left behind, so every
    /// test needs one run first.
    fn fetch_first(source: &FixedSource, work_dir: &std::path::Path) {
        let deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
            .parse()
            .unwrap();
        crate::fetch::fetch_batch(source, work_dir, &deadline).unwrap();
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
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"


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
formula = "sum"
base = 0.0
"#;

    fn passing_test(name: &str) -> crate::model::TestResult {
        crate::model::TestResult {
            name: name.into(),
            status: crate::model::TestStatus::Pass,
            duration_ms: Some(1),
            message: None,
            reported_score: None,
        }
    }

    #[test]
    fn grade_batch_runs_end_to_end_over_a_directory_submission() {
        let package_dir = tempfile::tempdir().unwrap();
        let submission_src = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();

        write(
            &package_dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"hw3\"]\n",
        );
        write(
            &submission_src.path().join("hw3/src/lib.rs"),
            "// student code",
        );

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let source = FixedSource(vec![crate::model::Submission {
            student_id: "alice".into(),
            fetchable: LocalPath(submission_src.path().to_path_buf()),
            metadata: Default::default(),
        }]);
        let evaluator = crate::evaluator::StubEvaluator {
            tests: vec![passing_test("insert_basic")],
        };
        let store = Store::new(store_dir.path());

        fetch_first(&source, work_dir.path());
        let grades = grade_batch(
            &source,
            &evaluator,
            package_dir.path(),
            &spec,
            work_dir.path(),
            &store,
            &Overrides::default(),
        )
        .unwrap();

        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].student_id, "alice");
        assert_eq!(grades[0].score, 1.0);
        assert_eq!(grades[0].max, None);

        let persisted = store
            .latest_evals(crate::id::AssignmentId::new("hw3"))
            .unwrap();
        assert_eq!(persisted.len(), 1);
        let persisted_grades = store
            .latest_grades(crate::id::AssignmentId::new("hw3"))
            .unwrap();
        assert_eq!(persisted_grades.len(), 1);

        assert!(
            work_dir
                .path()
                .join("alice/checkout/hw3/src/lib.rs")
                .is_file()
        );
        assert!(!work_dir.path().join("alice/build").exists());
    }

    #[test]
    fn grade_batch_reports_fetch_failed_when_the_checkout_has_no_id_directory() {
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
            tests: vec![passing_test("insert_basic")],
        };
        let store = Store::new(store_dir.path());

        fetch_first(&source, work_dir.path());
        let grades = grade_batch(
            &source,
            &evaluator,
            package_dir.path(),
            &spec,
            work_dir.path(),
            &store,
            &Overrides::default(),
        )
        .unwrap();

        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].status, "FetchFailed");
        assert!(!work_dir.path().join("alice/build").exists());
    }

    /// Records the sorted file names in `harness/tests/` (a sibling of
    /// `ctx.workspace`) at evaluate-time, before `grade_batch` deletes the
    /// scratch dir.
    struct CapturingEvaluator<'a> {
        seen: &'a std::sync::Mutex<Option<Vec<String>>>,
    }

    impl Evaluator for CapturingEvaluator<'_> {
        fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
            let harness_tests = ctx.workspace.parent().unwrap().join("harness/tests");
            let mut names: Vec<String> = std::fs::read_dir(harness_tests)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            *self.seen.lock().unwrap() = Some(names);

            Ok(EvaluationResult {
                schema_version: 1,
                assignment_id: ctx.assignment_id,
                student_id: ctx.student_id,
                run_id: ctx.run_id,
                graded_commit: None,
                instructor_commit: None,
                public_harness_commit: None,
                stages: StageReports {
                    fetch: StageReport::ok(),
                    build: StageReport::ok(),
                    run: StageReport::ok(),
                },
                tests: Vec::new(),
                resource_usage: ResourceUsage::default(),
                diagnostics: Default::default(),
            })
        }
    }

    /// Confirms a submission's checkout can never reach `harness/`, even if
    /// it ships its own same-named `harness/` directory at its root, hoping
    /// to overwrite the trusted judge: `checkout_rules()` only ever globs
    /// `{id}/**`, so `harness/` in `build_dir` always comes from the
    /// trusted `package_dir`, never the student.
    #[test]
    fn grade_batch_never_lets_the_submission_checkout_reach_the_harness_package() {
        let package_dir = tempfile::tempdir().unwrap();
        let submission_src = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();

        write(
            &package_dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"harness\", \"wc\"]\n",
        );
        write(
            &package_dir.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(
            &package_dir.path().join("harness/tests/judge.rs"),
            "#[test]\nfn counts_words() {}\n",
        );

        write(
            &submission_src.path().join("wc/src/main.rs"),
            "fn main() {}\n",
        );
        // A malicious submission tries to ship its own `harness/`, hoping
        // to overwrite the trusted judge.
        write(
            &submission_src.path().join("harness/tests/decoy.rs"),
            "#[test]\nfn fake_pass() { assert!(true); }\n",
        );

        let toml = SPEC_TOML
            .replace("kind = \"library\"", "kind = \"binary\"")
            .replace("id = \"hw3\"", "id = \"wc\"");
        let spec: Spec = toml::from_str(&toml).unwrap();
        let source = FixedSource(vec![crate::model::Submission {
            student_id: "alice".into(),
            fetchable: LocalPath(submission_src.path().to_path_buf()),
            metadata: Default::default(),
        }]);
        let seen = std::sync::Mutex::new(None);
        let evaluator = CapturingEvaluator { seen: &seen };
        let store = Store::new(store_dir.path());

        fetch_first(&source, work_dir.path());
        grade_batch(
            &source,
            &evaluator,
            package_dir.path(),
            &spec,
            work_dir.path(),
            &store,
            &Overrides::default(),
        )
        .unwrap();

        assert_eq!(
            seen.lock().unwrap().take(),
            Some(vec!["judge.rs".to_string()])
        );
    }

    #[test]
    fn grade_batch_scores_zero_for_a_disallowed_dependency_without_running_the_evaluator() {
        let package_dir = tempfile::tempdir().unwrap();
        let submission_src = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();

        write(
            &package_dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"hw3\"]\n",
        );
        write(
            &submission_src.path().join("hw3/src/lib.rs"),
            "// student code",
        );
        write(
            &submission_src.path().join("hw3/Cargo.toml"),
            "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\ntokio = \"1\"\n",
        );

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let source = FixedSource(vec![crate::model::Submission {
            student_id: "alice".into(),
            fetchable: LocalPath(submission_src.path().to_path_buf()),
            metadata: Default::default(),
        }]);
        let evaluator = crate::evaluator::StubEvaluator {
            tests: vec![passing_test("insert_basic")],
        };
        let store = Store::new(store_dir.path());

        fetch_first(&source, work_dir.path());
        let grades = grade_batch(
            &source,
            &evaluator,
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

        let persisted = store
            .latest_evals(crate::id::AssignmentId::new("hw3"))
            .unwrap();
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
            tests: vec![passing_test("insert_basic")],
        };
        let store = Store::new(store_dir.path());

        fetch_first(&source, work_dir.path());
        let grades = grade_batch(
            &source,
            &evaluator,
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

        write(
            &package_dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"hw3\"]\n",
        );
        write(
            &submission_src.path().join("hw3/src/lib.rs"),
            "// student code",
        );

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let source = FixedSource(vec![crate::model::Submission {
            student_id: "alice".into(),
            fetchable: LocalPath(submission_src.path().to_path_buf()),
            metadata: Default::default(),
        }]);
        let evaluator = crate::evaluator::StubEvaluator {
            tests: vec![passing_test("insert_basic")],
        };
        let store = Store::new(store_dir.path());
        let overrides = Overrides {
            manual: std::collections::BTreeMap::from([(
                crate::id::StudentId::new("alice"),
                crate::overrides::ManualOverride {
                    score: 3.0,
                    status: Some("manual-review".into()),
                    reason: "Partial credit for a documented edge case".into(),
                },
            )]),
            late: Default::default(),
        };

        fetch_first(&source, work_dir.path());
        let grades = grade_batch(
            &source,
            &evaluator,
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

        let persisted = store
            .latest_evals(crate::id::AssignmentId::new("hw3"))
            .unwrap();
        assert_eq!(persisted[0].tests[0].status, crate::model::TestStatus::Pass);
    }
}
