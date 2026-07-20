use std::path::Path;

use crate::deps::lock;
use crate::error::Result;

pub fn run(assignment: &Path) -> Result<()> {
    let outcome = lock::lock(assignment)?;
    tracing::info!(
        lock_path = %outcome.lock_path.display(),
        sha256 = %outcome.sha256,
        "lock complete"
    );
    Ok(())
}
