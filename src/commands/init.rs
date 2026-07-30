use std::path::Path;

use crate::error::Result;
use crate::package::init;

pub fn run(dir: &Path, id: &str) -> Result<()> {
    let outcome = init::init(dir, id)?;
    tracing::info!(dir = %outcome.dir.display(), "init complete");
    Ok(())
}
