//! Scaffolds a brand-new **private instructor package** from nothing --
//! `publish`'s inverse, which derives the public starter tree from an
//! existing private package.
//!
//! The layout to generate *is* a real directory tree, `templates/library/`
//! (rendered via [`crate::package::template`]), not a Rust table describing one.
//! Adding or restructuring a generated file is an edit to that tree, never
//! a change to this module's code.
//!
//! What's generated is a template to edit, not a finished package:
//! `{deadline}` renders to "now + one week" and `[assignment].name` is a
//! literal `"TODO: ..."` marker, both meant to be replaced.

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

    // Truncated to whole seconds so it reads as an editable value, not
    // generated cruft. Uses the host's own time zone (via `Zoned::now`), so
    // the placeholder is meaningful as-is rather than needing a manual
    // zone edit before it's even a plausible value.
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
    // package is immediately loadable (`cargo-lock-sha256` has no default,
    // like `id` and `harness` -- see `spec::Assignment`'s doc comment).
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
