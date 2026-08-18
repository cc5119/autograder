//! `autograder open`: opens a submission's fork on GitHub. Takes the same
//! single argument `show` does -- the checkout dir -- and reads the same
//! `.fetch/<user>.json` record beside it.
//!
//! The record is the only reliable source for which repo a checkout came
//! from: `fetch --detach` removes the checkout's `.git`, so for the layout
//! that mode exists to serve there's no remote left on disk to read.
//!
//! Opening goes through `gh browse`, already a hard dependency of every
//! roster-facing stage. That keeps host resolution and auth `gh`'s
//! business rather than something this command hardcodes, so a GitHub
//! Enterprise host works with no changes here.

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};
use crate::github::GH_BIN;
use crate::submissions::read_fetch_record;

use super::submission_parts;

pub fn run(submission: &Path) -> Result<()> {
    let (github_user, submissions_dir) = submission_parts(submission)?;

    let record = read_fetch_record(submissions_dir, &github_user)?.ok_or_else(|| {
        Error::Other(format!(
            "no fetch record for {github_user} under {} -- run `autograder fetch` first",
            submissions_dir.display()
        ))
    })?;

    // `forks[0]` is the one that was fetched; a longer list is a recorded
    // ambiguity (see `FetchRecord::forks`), and opening anything but the
    // repo actually graded would be misleading.
    let fork = record.forks.first().ok_or_else(|| {
        Error::Other(format!(
            "the fetch record for {github_user} names no fork -- nothing to open"
        ))
    })?;

    let nwo = fork.nwo();
    let status = Command::new(GH_BIN)
        .args(["browse", "--repo", &nwo])
        .status()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if !status.success() {
        return Err(Error::Other(format!(
            "`{GH_BIN} browse --repo {nwo}` failed"
        )));
    }
    Ok(())
}
