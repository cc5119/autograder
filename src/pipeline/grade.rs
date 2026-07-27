use crate::id::StudentId;
use crate::model::{EvalStatus, EvaluationResult, Grade, RunStatus, TestResult, TestStatus};
use crate::spec::{Scoring, ScoringFormula};

/// Pure scoring: `EvaluationResult` + policy -> per-student `Grade`. No
/// untrusted code runs here, and no pre-declared test-name table is
/// consulted -- every test `eval` reports feeds the sum, whatever its
/// runtime name. `eval.submission_id` names a submission, not a student --
/// `autograder fetch` always chooses that name to be the student's own id
/// (see `submissions` module doc comment), so it's reused verbatim as the
/// `Grade`'s `student_id` here, the one place submission identity turns
/// back into student identity.
pub fn grade(eval: &EvaluationResult, policy: &Scoring) -> Grade {
    // A stage-level failure means the run never produced a trustworthy set
    // of test results at all, even if a few tests happened to report
    // before the crash -- so it floors the score at each formula's own
    // floor value instead of scoring the partial results.
    let stage_failed = !matches!(eval.status, EvalStatus::Ran(RunStatus::Ok));

    let failing_tests: Vec<String> = eval
        .tests
        .iter()
        .filter(|t| t.status != TestStatus::Pass)
        .map(|t| t.name.clone())
        .collect();

    let reported_sum: f64 = eval.tests.iter().map(contribution).sum();

    let (score, max) = match &policy.formula {
        ScoringFormula::Sum { base } => {
            let score = if stage_failed {
                *base
            } else {
                base + reported_sum
            };
            (score, None)
        }
        ScoringFormula::Affine {
            max_sum,
            scale_min,
            scale_max,
        } => {
            let score = if stage_failed {
                *scale_min
            } else {
                let frac = (reported_sum / max_sum).clamp(0.0, 1.0);
                scale_min + frac * (scale_max - scale_min)
            };
            (score, Some(*scale_max))
        }
    };

    let status = if stage_failed {
        stage_status_label(&eval.status)
    } else if failing_tests.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    Grade {
        student_id: StudentId::new(eval.submission_id.as_str()),
        score,
        max,
        status,
        failing_tests,
    }
}

/// A test's contribution to the sum: whatever it self-reported, or the
/// 1.0/0.0 pass/fail default when it reported nothing.
fn contribution(test: &TestResult) -> f64 {
    match test.reported_score {
        Some(v) => v,
        None if test.status == TestStatus::Pass => 1.0,
        None => 0.0,
    }
}

fn stage_status_label(stages: &EvalStatus) -> String {
    match stages {
        EvalStatus::BuildFailed(status) => format!("{status:?}"),
        EvalStatus::Ran(RunStatus::Ok) => "ok".to_string(),
        EvalStatus::Ran(status) => format!("{status:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BuildStatus;

    fn eval_with(stages: EvalStatus, tests: Vec<TestResult>) -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            assignment_id: "hw3".into(),
            submission_id: "alice".into(),
            run_id: "run1".into(),
            graded_commit: None,
            instructor_commit: None,
            status: stages,
            tests,
            cpu_ms: None,
            diagnostics: Default::default(),
        }
    }

    fn ok_stages() -> EvalStatus {
        EvalStatus::Ran(RunStatus::Ok)
    }

    fn test_result(name: &str, status: TestStatus) -> TestResult {
        TestResult {
            name: name.into(),
            status,
            duration_ms: 0,
            message: None,
            reported_score: None,
        }
    }

    fn test_result_with_score(name: &str, status: TestStatus, score: f64) -> TestResult {
        TestResult {
            reported_score: Some(score),
            ..test_result(name, status)
        }
    }

    fn sum_policy(base: f64) -> Scoring {
        Scoring {
            formula: ScoringFormula::Sum { base },
        }
    }

    fn affine_policy(max_sum: f64, scale_min: f64, scale_max: f64) -> Scoring {
        Scoring {
            formula: ScoringFormula::Affine {
                max_sum,
                scale_min,
                scale_max,
            },
        }
    }

    #[test]
    fn sum_formula_adds_base_to_the_default_pass_fail_contributions() {
        let eval = eval_with(
            ok_stages(),
            vec![
                test_result("a", TestStatus::Pass),
                test_result("b", TestStatus::Fail),
            ],
        );
        let grade = grade(&eval, &sum_policy(1.0));
        assert_eq!(grade.score, 2.0);
        assert_eq!(grade.max, None);
        assert_eq!(grade.failing_tests, vec!["b".to_string()]);
    }

    #[test]
    fn sum_formula_sums_reported_scores_over_the_pass_fail_default() {
        let eval = eval_with(
            ok_stages(),
            vec![
                test_result_with_score("a", TestStatus::Pass, 0.5),
                test_result_with_score("b", TestStatus::Fail, 0.25),
            ],
        );
        let grade = grade(&eval, &sum_policy(0.0));
        assert_eq!(grade.score, 0.75);
    }

    #[test]
    fn affine_formula_scales_the_reported_sum_into_the_configured_range() {
        let eval = eval_with(
            ok_stages(),
            vec![
                test_result_with_score("a", TestStatus::Pass, 10.0),
                test_result_with_score("b", TestStatus::Pass, 10.0),
            ],
        );
        let grade = grade(&eval, &affine_policy(20.0, 1.0, 7.0));
        assert_eq!(grade.score, 7.0);
        assert_eq!(grade.max, Some(7.0));
    }

    #[test]
    fn affine_formula_clamps_above_max_sum() {
        let eval = eval_with(
            ok_stages(),
            vec![test_result_with_score("a", TestStatus::Pass, 100.0)],
        );
        let grade = grade(&eval, &affine_policy(20.0, 1.0, 7.0));
        assert_eq!(grade.score, 7.0);
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
            EvalStatus::Ran(RunStatus::HarnessError),
            vec![test_result("a", TestStatus::Pass)],
        );
        let policy = sum_policy(0.0);

        let honest_grade = grade(&honest_fail_eval, &policy);
        let harness_error_grade = grade(&harness_error_eval, &policy);

        assert_eq!(honest_grade.score, 1.0);
        // Despite reporting "a" as passing, a harness_error floors at base.
        assert_eq!(harness_error_grade.score, 0.0);
        assert!(harness_error_grade.score <= honest_grade.score);
    }

    #[test]
    fn build_failed_floors_the_sum_formula_at_base() {
        let eval = eval_with(EvalStatus::BuildFailed(BuildStatus::Failed), vec![]);
        let grade = grade(&eval, &sum_policy(1.0));
        assert_eq!(grade.score, 1.0);
        assert_eq!(grade.status, "Failed");
    }

    #[test]
    fn build_failed_floors_the_affine_formula_at_scale_min() {
        let eval = eval_with(EvalStatus::BuildFailed(BuildStatus::Failed), vec![]);
        let grade = grade(&eval, &affine_policy(20.0, 1.0, 7.0));
        assert_eq!(grade.score, 1.0);
        assert_eq!(grade.status, "Failed");
    }
}
