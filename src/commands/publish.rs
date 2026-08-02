use std::path::Path;

use dialoguer::Confirm;

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::package::PublishMode;
use crate::package::publish;

/// `mode` is `None` when `--mode` wasn't given, which is the ordinary
/// interactive case: it gets asked for rather than assumed.
pub fn run(assignment: &Path, out: &Path, mode: Option<PublishMode>) -> Result<()> {
    let mode = match mode {
        Some(mode) => mode,
        None => PublishMode::prompt("Publish as")?,
    };
    if out.is_dir() && !fs::is_empty_dir(out)? && !confirm_overwrite(out)? {
        println!("aborted");
        return Ok(());
    }

    let outcome = publish::publish(assignment, out, mode)?;
    tracing::info!(out_dir = %outcome.out_dir.display(), "publish complete");
    Ok(())
}

fn confirm_overwrite(out: &Path) -> Result<bool> {
    Confirm::new()
        .with_prompt(format!(
            "{} already exists and is not empty, overwrite?",
            out.display()
        ))
        .default(false)
        .interact()
        .map_err(|source| Error::Other(format!("failed to read confirmation: {source}")))
}
