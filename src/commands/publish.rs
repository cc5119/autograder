use std::path::Path;

use crate::error::Result;
use crate::package::publish;

pub fn run(assignment: &Path, out: &Path) -> Result<()> {
    let outcome = publish::publish(assignment, out)?;
    tracing::info!(out_dir = %outcome.out_dir.display(), "publish complete");
    Ok(())
}
