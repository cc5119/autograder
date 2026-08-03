use std::path::Path;

use crate::error::Result;
use crate::model::{EvalStatus, EvaluationResult, TestOutcome};
use crate::pipeline;
use crate::spec::Spec;

use super::build_evaluator;

pub fn run(assignment_dir: &Path, submissions_dir: &Path, local_sandbox: bool) -> Result<()> {
    let spec = Spec::load(assignment_dir)?;

    let evaluator = build_evaluator(&spec, local_sandbox)?;

    let evals =
        pipeline::evaluate_batch(submissions_dir, evaluator.as_ref(), assignment_dir, &spec)?;

    print!("{}", render_summary(&evals));

    Ok(())
}

/// The final tally printed once a batch finishes -- per-submission outcomes
/// are already printed live as each one completes (see
/// `pipeline::evaluate_batch`), so this is deliberately just the count, not
/// a full recap.
fn render_summary(evals: &[EvaluationResult]) -> String {
    if evals.is_empty() {
        return "autograde: 0 submissions evaluated -- no submission directories found under \
                --submissions (run `autograder fetch` first, or check the path)\n"
            .to_string();
    }
    let ok = evals
        .iter()
        .filter(|eval| {
            matches!(
                eval.status,
                EvalStatus::Ran {
                    tests: TestOutcome::Tests(_),
                    ..
                }
            )
        })
        .count();
    format!(
        "{} submissions evaluated: {ok} ok, {} not ok\n",
        evals.len(),
        evals.len() - ok
    )
}
