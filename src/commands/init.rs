use std::path::Path;

use crate::error::Result;
use crate::package::init;
use crate::spec::AssignmentKind;

pub fn run(dir: &Path, id: &str, kind: AssignmentKind) -> Result<()> {
    let outcome = init::init(dir, id, kind)?;
    tracing::info!(dir = %outcome.dir.display(), "init complete");
    Ok(())
}
