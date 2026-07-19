use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::{Error, Result};
use crate::evaluator::Evaluator;
use crate::fetch::read_fetch_record;
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus,
};
use crate::overrides::{self, Overrides};
use crate::source::SubmissionsSource;
use crate::spec::{AssignmentKind, Spec};
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
/// Prepare -> Evaluate -> persist -> Grade -> apply overrides, one student
/// at a time. Fetch is deliberately *not* one of these stages -- it runs
/// separately, either via `autograder fetch` or `autograder grade --fetch`
/// (see `crate::fetch::fetch_batch`), so re-running just this part (e.g.
/// after fixing a harness bug) never needs network access. This function
/// only ever *reads* what a prior fetch left behind: `job_root/checkout/`
/// and the [`crate::fetch::FetchRecord`] describing it. A student with no
/// such record (fetch never ran for them) gets a `FetchFailed` result, the
/// same as one whose fetch itself failed -- the batch never aborts for one
/// student either way.
///
/// Not generic over a fetchable type -- `source` only needs to yield the
/// roster (`student_id`/`metadata`), since this function never fetches.
///
/// The `checkout/` a prior fetch left behind mirrors `publish`'s starter
/// layout (the crate under `[assignment].id`, possibly a `harness/`
/// alongside it that this pipeline never trusts or reads) and is kept on
/// disk indefinitely as the record of what was actually submitted --
/// never written into by anything past this point. This function extracts
/// just the `<id>/` subdirectory into an ephemeral `job_root/build/`
/// alongside a fresh copy of the *private* harness (see
/// `prepare::prepare`), which is what Prepare/Evaluate actually operate
/// on. `build/` is deleted after every run regardless of outcome, so the
/// private harness's test source never lingers on disk.
///
/// `overrides` (design §14, §18.2 -- M5 step 24) is applied to the `Grade`
/// after `grade::grade` runs, never touching the persisted `eval` -- see
/// `overrides::apply`'s doc comment for why a manual override or late
/// penalty is recomputed here rather than baked into the raw result.
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
        let job_root = work_dir.join(&submission.student_id);
        // The raw fetched checkout a prior `fetch_batch` run left behind.
        // Never written into by anything past this point.
        let checkout_dir = job_root.join("checkout");
        // The ephemeral combined copy actually used to build/test: just
        // the extracted `<id>/` crate plus a fresh copy of the *private*
        // harness (never the student's own, even if their checkout has
        // one -- see `prepare::prepare`). Deleted after every run (success
        // or failure) so the private harness's test source never lingers
        // on disk. `workspace` is named after `[assignment].id`, not e.g.
        // "student": the harness's checked-in `Cargo.toml` depends on that
        // exact sibling name (see `evaluator::library`'s module doc
        // comment), so this naming is what lets that dependency resolve
        // correctly with no patch/`--config` override needed.
        let build_dir = job_root.join("build");
        let workspace = build_dir.join(&spec.assignment.id);
        let driver_dir = build_dir.join("harness");
        let ctx = JobContext {
            assignment_id: spec.assignment.id.clone(),
            student_id: submission.student_id.clone(),
            run_id: run_id.clone(),
            workspace: workspace.clone(),
            driver_dir: driver_dir.clone(),
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
            let submitted_crate = checkout_dir.join(&spec.assignment.id);
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
                // Extract just the submitted crate into the scratch build
                // dir, then Prepare/Evaluate exactly as before -- but
                // always clean up `build_dir` afterward, whether this
                // closure succeeded or hit a hard error, before letting
                // that error propagate.
                let outcome: Result<EvaluationResult> = (|| {
                    crate::prepare::copy_dir_into(&workspace, &submitted_crate)?;
                    // Both kinds need the *private* judge overlaid onto
                    // this submission -- `package_dir`'s own `<id>/` is
                    // the reference solution, not this submission, and
                    // `Authoritative` never shares one copy across
                    // students -- but the shape differs (see
                    // `evaluator::library`'s and `evaluator::binary`'s
                    // module doc comments): `library`'s harness is a
                    // separate crate copied to a sibling `driver_dir`;
                    // `binary` has no separate crate at all, so its judge
                    // tests get merged directly into `workspace`'s own
                    // `tests/`. That `tests/` dir is wiped first, not just
                    // overlaid onto: `copy_dir_into` only overwrites paths
                    // that exist in its source, so simply copying on top
                    // would leave any *other* file the submission already
                    // had there (e.g. the public judge `publish` baked
                    // into the starter, which a normal submission still
                    // has) -- and since `grade::grade` matches a
                    // `TestResult` to a scored test by name alone, with no
                    // regard for which file/binary it came from
                    // (`grade.rs`), a stray leftover file defining a
                    // same-named, trivially-passing test would be a real
                    // way to fake a hidden test's result. Wiping first
                    // guarantees the private judge is the *only* thing
                    // `cargo nextest` ever finds in `tests/`.
                    match spec.assignment.kind {
                        AssignmentKind::Library => {
                            let harness_dir = package_dir.join("harness");
                            if harness_dir.is_dir() {
                                crate::prepare::copy_dir_into(&driver_dir, &harness_dir)?;
                            }
                        }
                        AssignmentKind::Binary => {
                            let tests_dir = package_dir.join(&spec.assignment.id).join("tests");
                            let workspace_tests_dir = workspace.join("tests");
                            if workspace_tests_dir.is_dir() {
                                std::fs::remove_dir_all(&workspace_tests_dir).map_err(
                                    |source| Error::Io {
                                        path: workspace_tests_dir.clone(),
                                        source,
                                    },
                                )?;
                            }
                            if tests_dir.is_dir() {
                                crate::prepare::copy_dir_into(&workspace_tests_dir, &tests_dir)?;
                            }
                        }
                    }
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
    use crate::model::LocalPath;
    use crate::source::SubmissionsSource;

    struct FixedSource(Vec<crate::model::Submission<LocalPath>>);
    impl SubmissionsSource<LocalPath> for FixedSource {
        fn submissions(&self) -> Result<Vec<crate::model::Submission<LocalPath>>> {
            Ok(self.0.clone())
        }
    }

    /// `grade_batch` no longer fetches -- it only reads what a prior
    /// `fetch_batch` run left at `work_dir/<student_id>/checkout/` plus
    /// its `FetchRecord`. Every test below needs that populated first,
    /// exactly as a real `grade --fetch` (or a separate `autograder
    /// fetch`) would.
    fn fetch_first(source: &FixedSource, work_dir: &std::path::Path) {
        let deadline = "2026-02-14T23:59:59-08:00".parse().unwrap();
        crate::fetch::fetch_batch(source, work_dir, deadline).unwrap();
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

        // The submission source is a whole checkout, matching `publish`'s
        // starter layout: the crate lives under a subdirectory named after
        // `[assignment].id`, not flattened at the source root.
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
            tests: spec.scoring.tests.clone(),
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
        assert_eq!(grades[0].score, 10.0);
        assert_eq!(grades[0].max, 10.0);

        let persisted = store.latest_evals("hw3").unwrap();
        assert_eq!(persisted.len(), 1);
        let persisted_grades = store.latest_grades("hw3").unwrap();
        assert_eq!(persisted_grades.len(), 1);

        // The raw checkout survives as the record of what was submitted;
        // the ephemeral combined build dir (submission + private harness
        // copy) is cleaned up after grading.
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

        // A checkout with no `hw3/` subdirectory at all -- e.g. a
        // flattened submission, no longer the expected shape.
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

    /// An `Evaluator` double that, instead of scoring anything, just
    /// records the sorted file names it finds in `ctx.workspace/tests/` at
    /// evaluate-time -- lets a test observe exactly what `cargo nextest`
    /// would have seen, before `grade_batch` deletes the scratch `build/`
    /// dir afterward.
    struct CapturingEvaluator<'a> {
        seen: &'a std::sync::Mutex<Option<Vec<String>>>,
    }

    impl Evaluator for CapturingEvaluator<'_> {
        fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
            let mut names: Vec<String> = std::fs::read_dir(ctx.workspace.join("tests"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            *self.seen.lock().unwrap() = Some(names);

            Ok(EvaluationResult {
                schema_version: 1,
                assignment_id: ctx.assignment_id.clone(),
                student_id: ctx.student_id.clone(),
                run_id: ctx.run_id.clone(),
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

    /// A submission that still has the publish-baked public judge (same
    /// filename the private judge will overlay onto) *and* a decoy file
    /// with a same-named, trivially-passing stand-in for the hidden test
    /// -- confirms `workspace/tests/` is wiped before the private judge is
    /// copied in, so the decoy never reaches `cargo nextest` at all (see
    /// the `AssignmentKind::Binary` arm's comment in `grade_batch` for why
    /// this matters for grading integrity, not just tidiness).
    #[test]
    fn grade_batch_wipes_binary_workspace_tests_before_overlaying_the_private_judge() {
        let package_dir = tempfile::tempdir().unwrap();
        let submission_src = tempfile::tempdir().unwrap();
        let work_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();

        write(
            &package_dir.path().join("wc/tests/judge.rs"),
            "#[test]\nfn counts_words() {}\n",
        );
        write(
            &submission_src.path().join("wc/tests/judge.rs"),
            "#[test]\nfn counts_words() {}\n",
        );
        write(
            &submission_src.path().join("wc/tests/decoy.rs"),
            "#[test]\nfn counts_zero_for_empty_input() { assert!(true); }\n",
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
        // A StubEvaluator that would score everything as passing if it ever
        // ran — the assertion below only holds if the pipeline actually
        // short-circuits before reaching it.
        let evaluator = crate::evaluator::StubEvaluator {
            tests: spec.scoring.tests.clone(),
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

        // The raw persisted eval is untouched by the override -- only the
        // derived Grade reflects it.
        let persisted = store.latest_evals("hw3").unwrap();
        assert_eq!(persisted[0].tests[0].status, crate::model::TestStatus::Pass);
    }
}
