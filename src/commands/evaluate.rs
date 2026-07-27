use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::pipeline;
use crate::spec::Spec;
use crate::store::Store;

pub fn run(
    assignment: &Path,
    submissions: &Path,
    local_sandbox: bool,
    config: &Config,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let evaluator = super::build_evaluator(&spec, assignment, local_sandbox)?;
    let store = Store::new(&config.storage_dir);
    let work_dir = config.storage_dir.join(".work");

    pipeline::evaluate_batch(
        submissions,
        evaluator.as_ref(),
        assignment,
        &spec,
        &work_dir,
        &store,
    )?;

    Ok(())
}
