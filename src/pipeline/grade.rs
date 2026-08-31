use crate::model::{
    EvalStatus, EvaluationResult, Grade, GradeOutcome, TestOutcome, TestResult, TestStatus,
};
use crate::spec::{LatePenalty, Scoring, ScoringFormula};

/// Pure scoring: `EvaluationResult` + policy -> per-student `Grade`. No
/// untrusted code runs here, and no pre-declared test-name table is
/// consulted -- every test `eval` reports feeds the sum, whatever its
/// runtime name. A result with no trustworthy tests to score comes back
/// `GradeOutcome::Unscored` saying why.
///
/// `late_by` is `Some` for any commit `FetchRecord::late_by` reports as
/// late -- including a blessed or overridden one, exactly like the
/// terminal summary already reports lateness for those. Blessing only
/// exempts a commit from the deadline *gate*, not from what being late
/// costs once it's graded.
pub fn grade(
    eval: &EvaluationResult,
    policy: &Scoring,
    late_penalty: Option<&LatePenalty>,
    late_by: Option<jiff::SignedDuration>,
) -> Grade {
    let tests: &[TestResult] = match &eval.status {
        EvalStatus::Ran {
            tests: TestOutcome::Tests(tests),
            ..
        } => tests,
        status => {
            return Grade {
                github_user: eval.github_user,
                outcome: GradeOutcome::Unscored {
                    reason: unscored_reason(status),
                },
            };
        }
    };

    let reported_sum: f64 = tests.iter().map(contribution).sum();

    let score = match &policy.formula {
        ScoringFormula::Sum { base } => base + reported_sum,
        ScoringFormula::Affine {
            max_sum,
            scale_min,
            scale_max,
        } => {
            let frac = (reported_sum / max_sum).clamp(0.0, 1.0);
            scale_min + frac * (scale_max - scale_min)
        }
    };

    let penalty = late_penalty
        .zip(late_by)
        .map(|(policy, by)| policy.fraction_off(by))
        .unwrap_or(0.0);

    Grade {
        github_user: eval.github_user,
        outcome: GradeOutcome::Scored {
            score: score * (1.0 - penalty),
            points: reported_sum,
            passed: tests
                .iter()
                .filter(|t| t.status == TestStatus::Pass)
                .count(),
            total: tests.len(),
            penalty,
        },
    }
}

/// Why a result wasn't scorable, in the same words `evaluate` and `show`
/// already use for it. The process status stays in: "timed out" and
/// "exited (0) but produced nothing" are different problems.
fn unscored_reason(status: &EvalStatus) -> String {
    match status {
        EvalStatus::BuildFailed(build) => format!("build failed: {}", build.label()),
        EvalStatus::Ran { process, tests } => format!("{} ({})", process.describe(), tests.label()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::sandbox::ProcessStatus;
    use crate::model::InputHash;
    use crate::model::{Diagnostics, EvalStatus, TestOutcome, TestResult, TestStatus};
    use crate::spec::LatePenaltyFormula;

    fn eval(passed: bool) -> EvaluationResult {
        EvaluationResult {
            assignment_id: "hw3".into(),
            github_user: "alice".into(),
            run_id: "run-1".into(),
            input_hash: InputHash::new("test"),
            status: EvalStatus::Ran {
                process: ProcessStatus::Exited(0),
                tests: TestOutcome::Tests(vec![TestResult {
                    name: "insert_basic".into(),
                    status: if passed {
                        TestStatus::Pass
                    } else {
                        TestStatus::Fail
                    },
                    duration_ms: 0,
                    message: None,
                    reported_score: None,
                }]),
            },
            wall_clock_ms: None,
            diagnostics: Diagnostics::default(),
        }
    }

    fn sum_policy() -> Scoring {
        Scoring {
            formula: ScoringFormula::Sum { base: 0.0 },
        }
    }

    fn linear_penalty(rate: f64, max_penalty: f64) -> LatePenalty {
        LatePenalty {
            formula: LatePenaltyFormula::Linear {
                rate,
                grace: jiff::SignedDuration::ZERO,
                max_penalty,
            },
        }
    }

    #[test]
    fn no_late_penalty_policy_leaves_the_score_untouched() {
        let grade = grade(
            &eval(true),
            &sum_policy(),
            None,
            Some(jiff::SignedDuration::from_hours(48)),
        );
        assert_eq!(grade.score(), Some(1.0));
    }

    #[test]
    fn a_penalty_policy_with_no_lateness_leaves_the_score_untouched() {
        let policy = linear_penalty(0.1, 1.0);
        let grade = grade(&eval(true), &sum_policy(), Some(&policy), None);
        assert_eq!(grade.score(), Some(1.0));
    }

    #[test]
    fn a_late_submission_is_docked_by_the_penalty_fraction_but_points_are_untouched() {
        let policy = linear_penalty(0.1, 1.0);
        let grade = grade(
            &eval(true),
            &sum_policy(),
            Some(&policy),
            Some(jiff::SignedDuration::from_hours(48)),
        );
        // 2 days late * 10%/day = 20% off a score of 1.0.
        assert_eq!(grade.score(), Some(0.8));
        match grade.outcome {
            GradeOutcome::Scored {
                points, penalty, ..
            } => {
                assert_eq!(points, 1.0);
                assert_eq!(penalty, 0.2);
            }
            GradeOutcome::Unscored { .. } => panic!("expected a scored outcome"),
        }
    }

    #[test]
    fn an_unscored_result_is_never_penalized() {
        let mut failed = eval(false);
        failed.status = EvalStatus::BuildFailed(crate::model::BuildStatus::Failed(
            ProcessStatus::Exited(101),
        ));

        let policy = linear_penalty(0.1, 1.0);
        let grade = grade(
            &failed,
            &sum_policy(),
            Some(&policy),
            Some(jiff::SignedDuration::from_hours(48)),
        );
        assert!(matches!(grade.outcome, GradeOutcome::Unscored { .. }));
    }
}
