use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::id::AssignmentId;
use crate::pipeline::{grade, overrides};
use crate::spec::Spec;
use crate::store::Store;

/// Re-runs **only** the Grade stage from persisted `EvaluationResult`s (no
/// student code, no evaluator) -- applies `spec.scoring`/`overrides.toml`
/// fresh from disk every time, so editing either always reflects the
/// current policy, never one baked in at `grade` time.
pub fn run(assignment_id: AssignmentId, assignment: &Path, config: &Config) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let store = Store::new(&config.storage_dir);
    let evals = store.latest_evals(assignment_id)?;
    let overrides = overrides::Overrides::load_from_package(assignment)?;

    let mut grades = Vec::new();
    for eval in &evals {
        let grade = grade::grade(eval, &spec.scoring);
        let grade = overrides::apply(
            grade,
            &overrides,
            &spec.assignment.deadline,
            spec.scoring.late_penalty.as_ref(),
            eval.submission_date.as_ref(),
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

[limits]
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
            submission_date: None,
        }
    }

    #[test]
    fn regrade_recomputes_scores_from_a_changed_policy_without_reevaluating() {
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

        run(AssignmentId::new("hw3"), assignment_dir.path(), &config).unwrap();
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
        run(AssignmentId::new("hw3"), assignment_dir.path(), &config).unwrap();
        let grades = store.latest_grades(AssignmentId::new("hw3")).unwrap();
        assert_eq!(grades[0].score, 5.0);
        assert_eq!(grades[0].max, Some(10.0));
    }

    #[test]
    fn regrade_applies_overrides_toml_on_top_of_the_recomputed_score() {
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

        run(AssignmentId::new("hw3"), assignment_dir.path(), &config).unwrap();

        let grades = store.latest_grades(AssignmentId::new("hw3")).unwrap();
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].score, 25.0);
        assert_eq!(grades[0].status, "override");
        assert!(grades[0].override_reason.is_some());
    }
}
