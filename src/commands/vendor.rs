use std::path::Path;

use crate::deps::vendor;
use crate::error::Result;
use crate::spec::Spec;

pub fn run(assignment: &Path) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let outcome = vendor::vendor(assignment, &spec)?;
    tracing::info!(
        vendor_dir = %outcome.vendor_dir.display(),
        cargo_config = %outcome.cargo_config_path.display(),
        "vendor complete"
    );
    Ok(())
}
