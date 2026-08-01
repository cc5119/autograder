use crate::model::{
    EvalStatus, EvaluationResult, Grade, GradeOutcome, TestOutcome, TestResult, TestStatus,
};
use crate::spec::{Scoring, ScoringFormula};

/// Pure scoring: `EvaluationResult` + policy -> per-student `Grade`. No
/// untrusted code runs here, and no pre-declared test-name table is
/// consulted -- every test `eval` reports feeds the sum, whatever its
/// runtime name. A result with no trustworthy tests to score comes back
/// `GradeOutcome::Unscored` saying why.
pub fn grade(eval: &EvaluationResult, policy: &Scoring) -> Grade {
    let tests: &[TestResult] = match &eval.status {
        EvalStatus::Ran {
            tests: TestOutcome::Tests(tests),
            ..
        } => tests,
        status => {
            return Grade {
                student_id: eval.student_id,
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

    Grade {
        student_id: eval.student_id,
        outcome: GradeOutcome::Scored {
            score,
            passed: tests
                .iter()
                .filter(|t| t.status == TestStatus::Pass)
                .count(),
            total: tests.len(),
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
