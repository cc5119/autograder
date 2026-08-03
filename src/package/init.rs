//! Scaffolds a **private instructor workspace** by copying a template.

use std::path::{Path, PathBuf};

use jiff::{ToSpan, Unit, Zoned, ZonedRound};

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::package::template;
use crate::str_map;

#[derive(Debug, Clone)]
pub struct InitOutcome {
    pub dir: PathBuf,
}

/// Writes a fresh private instructor package at `dir`, which must not
/// already exist or must be empty.
pub fn init(dir: &Path, id: &str) -> Result<InitOutcome> {
    if !is_valid_id(id) {
        return Err(Error::InvalidSpec(format!(
            "{id:?} is not a valid [assignment].id -- use only letters, digits, `_`, and `-`, \
             starting with a letter (it doubles as a Cargo package name)"
        )));
    }
    if dir.is_dir() && !fs::is_empty_dir(dir)? {
        return Err(Error::InvalidSpec(format!(
            "init requires an empty or nonexistent directory, but {} already has contents",
            dir.display()
        )));
    }

    // Truncated to whole seconds so it reads as an editable value. Uses
    // the host's own time zone, so the placeholder is meaningful.
    let deadline = (&Zoned::now() + 1.week())
        .round(ZonedRound::new().smallest(Unit::Second))
        .expect("rounding to the nearest second never fails")
        .to_string();
    let placeholders = str_map! {"id" => id, "deadline" => deadline};

    for (rel_path, content) in template::render_tree("library", &placeholders)? {
        let dst = dir.join(&rel_path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dst, content)?;
    }

    // Resolves and records `Cargo.lock`'s hash so the freshly scaffolded
    // workspace is immediately loadable
    crate::deps::lock::lock(dir)?;

    Ok(InitOutcome {
        dir: dir.to_path_buf(),
    })
}

fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_id_requires_a_leading_letter_and_only_alphanumerics_underscore_or_hyphen() {
        assert!(is_valid_id("hw3"));
        assert!(is_valid_id("a"));
        assert!(is_valid_id("hw_3-final"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("3-bad-start"));
        assert!(!is_valid_id("has space"));
        assert!(!is_valid_id("has.dot"));
    }
}
