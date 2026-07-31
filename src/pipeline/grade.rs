use crate::model::{EvalStatus, EvaluationResult, Grade, TestOutcome, TestResult, TestStatus};
use crate::spec::{Scoring, ScoringFormula};

/// Pure scoring: `EvaluationResult` + policy -> per-student `Grade`. No
/// untrusted code runs here, and no pre-declared test-name table is
/// consulted -- every test `eval` reports feeds the sum, whatever its
/// runtime name. `Grade.score` is `None` when the build failed or the run
/// left no readable results -- neither leaves a trustworthy set of tests
/// to score.
pub fn grade(eval: &EvaluationResult, policy: &Scoring) -> Grade {
    let tests: &[TestResult] = match &eval.status {
        EvalStatus::Ran {
            tests: TestOutcome::Tests(tests),
            ..
        } => tests,
        _ => {
            return Grade {
                student_id: eval.student_id,
                score: None,
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
        score: Some(score),
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
