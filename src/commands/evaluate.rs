use std::path::Path;

use crate::error::Result;
use crate::pipeline;
use crate::report::evaluate::render_evaluate_summary;
use crate::spec::Spec;

pub fn run(assignment: &Path, submissions: &Path, local_sandbox: bool) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let evaluator = super::build_evaluator(&spec, assignment, local_sandbox)?;

    let evals = pipeline::evaluate_batch(submissions, evaluator.as_ref(), assignment, &spec)?;

    print!("{}", render_evaluate_summary(&evals));

    Ok(())
}
