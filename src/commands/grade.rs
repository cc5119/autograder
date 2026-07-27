use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::id::AssignmentId;
use crate::pipeline::{grade, overrides};
use crate::spec::Spec;
use crate::store::Store;
use crate::submissions::read_fetch_record;

/// Computes scores from persisted `EvaluationResult`s (no student code, no
/// evaluator) -- applies `spec.scoring`/`overrides.toml` fresh from disk
/// every time, so editing either always reflects the current policy, never
/// one baked in earlier. `fetched`, if given, is a directory previously
/// produced by `autograder fetch --out`: each student's `.meta/<student_id>.json`
/// record is read fresh from there for late-penalty submission dates,
/// instead of trusting a copy baked into the eval (there is none -- eval's
/// `EvaluationResult` never carries a submission date).
pub fn run(
    assignment_id: AssignmentId,
    assignment: &Path,
    fetched: Option<&Path>,
    config: &Config,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let store = Store::new(&config.storage_dir);
    let evals = store.latest_evals(assignment_id)?;
    let overrides = overrides::Overrides::load_from_package(assignment)?;

    let mut grades = Vec::new();
    for eval in &evals {
        let submission_date = match fetched {
            Some(dir) => read_fetch_record(dir, &eval.student_id)?.and_then(|r| r.submission_date),
            None => None,
        };
        let grade = grade::grade(eval, &spec.scoring);
        let grade = overrides::apply(
            grade,
            &overrides,
            &spec.assignment.deadline,
            spec.scoring.late_penalty.as_ref(),
            submission_date.as_ref(),
        );
        store.save_grade(eval.assignment_id, eval.run_id, &grade)?;
        grades.push(grade);
    }

    super::report::write_reports(assignment_id, &grades, config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Diagnostics, EvaluationResult, ResourceUsage, StageReport, StageReports, TestResult,
        TestStatus,
    };
    use crate::model::StageStatus;
    use crate::submissions::{CommitTimestamp, FetchRecord, SubmissionDate, fetch_record_path};

    fn write(path: &std::path::Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn spec_toml(scoring_block: &str) -> String {
        format!(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{sha}"

[sandbox]
image = "autograder-base:1.86.0"

[build-limits]
wall-clock = "30s"
cpus = 1
memory = "512MiB"
pids = 64
max-output-bytes = "64KiB"

{scoring_block}
"#,
            sha = "0".repeat(64)
        )
    }

    fn persisted_eval() -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            graded_commit: None,
            instructor_commit: None,
            public_harness_commit: None,
            stages: StageReports {
                fetch: StageReport::ok(),
                build: StageReport::ok(),
                run: StageReport::ok(),
            },
            tests: vec![
                TestResult {
                    name: "insert_basic".into(),
                    status: TestStatus::Pass,
                    duration_ms: None,
                    message: None,
                    reported_score: None,
                },
                TestResult {
                    name: "balance_adversarial".into(),
                    status: TestStatus::Fail,
                    duration_ms: None,
                    message: None,
                    reported_score: None,
                },
            ],
            resource_usage: ResourceUsage::default(),
            diagnostics: Diagnostics::default(),
        }
    }

    #[test]
    fn grade_recomputes_scores_from_a_changed_policy_without_reevaluating() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage_dir: store_dir.path().to_path_buf(),
        };

        write(
            &assignment_dir.path().join(crate::spec::SPEC_FILE),
            &spec_toml("[scoring]\nformula = \"sum\"\nbase = 0.0"),
        );
        let store = Store::new(&config.storage_dir);
        store.save_eval(&persisted_eval()).unwrap();

        run(AssignmentId::new("hw3"), assignment_dir.path(), None, &config).unwrap();
        let grades = store.latest_grades(AssignmentId::new("hw3")).unwrap();
        assert_eq!(grades.len(), 1);
        // insert_basic passes (1.0 default), balance_adversarial fails (0.0).
        assert_eq!(grades[0].score, 1.0);
        assert_eq!(grades[0].max, None);

        write(
            &assignment_dir.path().join(crate::spec::SPEC_FILE),
            &spec_toml(
                "[scoring]\nformula = \"affine\"\nmax-sum = 2.0\nscale-min = 0.0\nscale-max = 10.0",
            ),
        );
        run(AssignmentId::new("hw3"), assignment_dir.path(), None, &config).unwrap();
        let grades = store.latest_grades(AssignmentId::new("hw3")).unwrap();
        assert_eq!(grades[0].score, 5.0);
        assert_eq!(grades[0].max, Some(10.0));
    }

    #[test]
    fn grade_applies_overrides_toml_on_top_of_the_recomputed_score() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage_dir: store_dir.path().to_path_buf(),
        };

        write(
            &assignment_dir.path().join(crate::spec::SPEC_FILE),
            &spec_toml("[scoring]\nformula = \"sum\"\nbase = 0.0"),
        );
        write(
            &assignment_dir.path().join(overrides::OVERRIDES_FILE),
            "[manual.alice]\nscore = 25.0\nreason = \"Appeal granted for balance_adversarial\"\n",
        );
        let store = Store::new(&config.storage_dir);
        store.save_eval(&persisted_eval()).unwrap();

        run(AssignmentId::new("hw3"), assignment_dir.path(), None, &config).unwrap();

        let grades = store.latest_grades(AssignmentId::new("hw3")).unwrap();
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].score, 25.0);
        assert_eq!(grades[0].status, "override");
        assert!(grades[0].override_reason.is_some());
    }

    #[test]
    fn grade_sources_submission_date_from_the_fetched_dir_for_late_penalty() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let fetched_dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage_dir: store_dir.path().to_path_buf(),
        };

        write(
            &assignment_dir.path().join(crate::spec::SPEC_FILE),
            &spec_toml(
                "[scoring]\nformula = \"sum\"\nbase = 0.0\n\n[scoring.late-penalty]\n\
                 grace = \"0s\"\nper-day-percent = 10.0\nmax-percent = 50.0",
            ),
        );
        let store = Store::new(&config.storage_dir);
        store.save_eval(&persisted_eval()).unwrap();

        // ~3 days late -> 30% docked.
        let record = FetchRecord {
            status: StageStatus::Ok,
            graded_commit: Some("abc123".into()),
            message: None,
            fetched_at: "2026-02-17T00:00:00Z".parse().unwrap(),
            submission_date: Some(SubmissionDate::Unblessed(CommitTimestamp {
                push_event: Some("2026-02-17T23:59:59Z".parse().unwrap()),
                commit_date: "2026-02-17T23:59:59Z".parse().unwrap(),
            })),
        };
        write(
            &fetch_record_path(fetched_dir.path(), &"alice".into()),
            &serde_json::to_string(&record).unwrap(),
        );

        run(
            AssignmentId::new("hw3"),
            assignment_dir.path(),
            Some(fetched_dir.path()),
            &config,
        )
        .unwrap();

        let grades = store.latest_grades(AssignmentId::new("hw3")).unwrap();
        assert_eq!(grades[0].late_penalty_percent, Some(30.0));
        assert_eq!(grades[0].score, 0.7);
    }
}
