//! `autograder override`: record which commit to grade for one student.
//! Writes the exception and stops -- applying it is `autograder fetch`'s
//! job, so the same rule runs on every later re-fetch instead of being a
//! one-off edit to a checkout that the next fetch would overwrite.

use std::path::Path;

use console::style;
use jiff::Timestamp;

use crate::error::Result;
use crate::submissions::overrides::{self, Override};

use super::submission_parts;

pub fn run(submission: &Path, commit: &str, reason: Option<String>) -> Result<()> {
    let (github_user, submissions) = submission_parts(submission)?;

    let entry = Override {
        github_user,
        commit: commit.to_string(),
        reason,
        recorded_at: Timestamp::now(),
    };

    let replaced = overrides::record(submissions, entry)?;

    if let Some(replaced) = replaced {
        println!(
            "  {} replaces the previous override for {github_user} ({})",
            style("!").yellow(),
            replaced.commit
        );
    }
    println!(
        "  recorded override for {github_user} -> {commit}\n  {}",
        style(format!(
            "applied by the next `autograder fetch --out {}`",
            submissions.display()
        ))
        .dim()
    );
    Ok(())
}
