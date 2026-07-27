use std::path::Path;

use crate::error::Result;
use crate::package::publish::{self, PublishMode};

pub fn run(assignment: &Path, out: &Path, mode: PublishMode) -> Result<()> {
    let outcome = publish::publish(assignment, out, mode)?;
    tracing::info!(out_dir = %outcome.out_dir.display(), "publish complete");
    Ok(())
}
