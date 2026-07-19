use crate::model::{EvaluationResult, Grade, StageStatus, TestStatus};
use crate::spec::{ScoredTest, Scoring, ScoringModel};

/// Pure scoring: `EvaluationResult` + policy -> per-student `Grade`. No
/// untrusted code runs here.
pub fn grade(eval: &EvaluationResult, policy: &Scoring) -> Grade {
    // A stage-level failure means every scored test is treated as failing,
    // even if student code dodged a specific assertion by crashing the
    // driver instead.
    let stage_failed = eval.stages.fetch.status != StageStatus::Ok
        || eval.stages.build.status != StageStatus::Ok
        || eval.stages.run.status != StageStatus::Ok;

    let passing = |test: &ScoredTest| -> bool {
        if stage_failed {
            return false;
        }
        eval.tests
            .iter()
            .find(|t| t.name == test.name)
            .is_some_and(|t| t.status == TestStatus::Pass)
    };

    let failing_tests: Vec<String> = policy
        .tests
        .iter()
        .filter(|t| !passing(t))
        .map(|t| t.name.clone())
        .collect();

    let (score, max) = match policy.model {
        ScoringModel::Weighted => {
            let max: f64 = policy.tests.iter().filter_map(|t| t.points).sum();
            let score: f64 = policy
                .tests
                .iter()
                .filter(|t| passing(t))
                .filter_map(|t| t.points)
                .sum();
            (score, max)
        }
        ScoringModel::PassCount => {
            let max = policy.tests.len() as f64;
            let score = policy.tests.iter().filter(|t| passing(t)).count() as f64;
            (score, max)
        }
        ScoringModel::PassFail => {
            let all_pass = !policy.tests.is_empty() && policy.tests.iter().all(passing);
            (if all_pass { 1.0 } else { 0.0 }, 1.0)
        }
    };

    let status = if stage_failed {
        stage_status_label(eval)
    } else if failing_tests.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    Grade {
        student_id: eval.student_id.clone(),
        score,
        max,
        status,
        failing_tests,
        override_reason: None,
        late_penalty_percent: None,
    }
}

fn stage_status_label(eval: &EvaluationResult) -> String {
    for status in [
        eval.stages.fetch.status,
        eval.stages.build.status,
        eval.stages.run.status,
    ] {
        if status != StageStatus::Ok {
            return format!("{status:?}");
        }
    }
    "ok".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ResourceUsage, StageReport, StageReports};

    fn eval_with(stages: StageReports, tests: Vec<crate::model::TestResult>) -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run1".into(),
            graded_commit: None,
            instructor_commit: None,
            public_harness_commit: None,
            stages,
            tests,
            resource_usage: ResourceUsage::default(),
            diagnostics: Default::default(),
        }
    }

    fn ok_stages() -> StageReports {
        StageReports {
            fetch: StageReport::ok(),
            build: StageReport::ok(),
            run: StageReport::ok(),
        }
    }

    fn test_result(name: &str, status: TestStatus) -> crate::model::TestResult {
        crate::model::TestResult {
            name: name.into(),
            visibility: crate::model::TestVisibility::Public,
            status,
            duration_ms: None,
            message: None,
        }
    }

    fn policy(model: ScoringModel, tests: Vec<(&str, Option<f64>)>) -> Scoring {
        Scoring {
            model,
            tests: tests
                .into_iter()
                .map(|(name, points)| ScoredTest {
                    name: name.into(),
                    visibility: crate::model::TestVisibility::Public,
                    points,
                })
                .collect(),
            late_penalty: None,
        }
    }

    #[test]
    fn weighted_sums_points_of_passing_tests() {
        let eval = eval_with(
            ok_stages(),
            vec![
                test_result("a", TestStatus::Pass),
                test_result("b", TestStatus::Fail),
            ],
        );
        let policy = policy(
            ScoringModel::Weighted,
            vec![("a", Some(10.0)), ("b", Some(20.0))],
        );
        let grade = grade(&eval, &policy);
        assert_eq!(grade.score, 10.0);
        assert_eq!(grade.max, 30.0);
        assert_eq!(grade.failing_tests, vec!["b".to_string()]);
    }

    #[test]
    fn pass_count_counts_passing_tests() {
        let eval = eval_with(
            ok_stages(),
            vec![
                test_result("a", TestStatus::Pass),
                test_result("b", TestStatus::Pass),
                test_result("c", TestStatus::Fail),
            ],
        );
        let policy = policy(
            ScoringModel::PassCount,
            vec![("a", None), ("b", None), ("c", None)],
        );
        let grade = grade(&eval, &policy);
        assert_eq!(grade.score, 2.0);
        assert_eq!(grade.max, 3.0);
    }

    #[test]
    fn pass_fail_requires_all_scored_tests_to_pass() {
        let eval = eval_with(
            ok_stages(),
            vec![
                test_result("a", TestStatus::Pass),
                test_result("b", TestStatus::Fail),
            ],
        );
        let policy = policy(ScoringModel::PassFail, vec![("a", None), ("b", None)]);
        let grade = grade(&eval, &policy);
        assert_eq!(grade.score, 0.0);
    }

    #[test]
    fn harness_error_scores_no_better_than_a_plain_fail() {
        // Student code crashes the driver on the very test it would have
        // failed anyway. The adversarial case: harness_error must not
        // score higher than an honest fail.
        let honest_fail_eval = eval_with(
            ok_stages(),
            vec![
                test_result("a", TestStatus::Pass),
                test_result("b", TestStatus::Fail),
            ],
        );
        let harness_error_eval = eval_with(
            StageReports {
                fetch: StageReport::ok(),
                build: StageReport::ok(),
                run: StageReport {
                    status: StageStatus::HarnessError,
                    duration_ms: None,
                    warnings: None,
                },
            },
            vec![test_result("a", TestStatus::Pass)],
        );
        let policy = policy(
            ScoringModel::Weighted,
            vec![("a", Some(10.0)), ("b", Some(20.0))],
        );

        let honest_grade = grade(&honest_fail_eval, &policy);
        let harness_error_grade = grade(&harness_error_eval, &policy);

        assert_eq!(honest_grade.score, 10.0);
        // Despite reporting "a" as passing, a harness_error zeroes everything.
        assert_eq!(harness_error_grade.score, 0.0);
        assert!(harness_error_grade.score <= honest_grade.score);
    }

    #[test]
    fn build_failed_scores_zero() {
        let eval = eval_with(
            StageReports {
                fetch: StageReport::ok(),
                build: StageReport {
                    status: StageStatus::BuildFailed,
                    duration_ms: None,
                    warnings: None,
                },
                run: StageReport::ok(),
            },
            vec![],
        );
        let policy = policy(ScoringModel::Weighted, vec![("a", Some(10.0))]);
        let grade = grade(&eval, &policy);
        assert_eq!(grade.score, 0.0);
        assert_eq!(grade.status, "BuildFailed");
    }
}
