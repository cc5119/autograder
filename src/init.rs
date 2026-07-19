//! Scaffolds a brand-new **private instructor package** from nothing --
//! `publish`'s inverse, which derives the public starter tree from an
//! existing private package.
//!
//! The layout to generate *is* a real directory tree, `templates/<kind>/`
//! (rendered via [`crate::template`]), not a Rust table describing one.
//! Adding or restructuring a generated file is an edit to that tree, never
//! a change to this module's code.
//!
//! What's generated is a template to edit, not a finished package:
//! `{deadline}` renders to "now + one week" and `[assignment].name` is a
//! literal `"TODO: ..."` marker, both meant to be replaced.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{Duration, Local, SubsecRound};

use crate::error::{Error, Result};
use crate::fs;
use crate::spec::AssignmentKind;

#[derive(Debug, Clone)]
pub struct InitOutcome {
    pub dir: PathBuf,
}

/// Writes a fresh private instructor package at `dir`, which must not
/// already exist or must be empty.
pub fn init(dir: &Path, id: &str, kind: AssignmentKind) -> Result<InitOutcome> {
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

    let kind_name = match kind {
        AssignmentKind::Library => "library",
        AssignmentKind::Binary => "binary",
    };

    // Truncated to whole seconds so it reads as an editable value, not
    // generated cruft.
    let deadline = (Local::now() + Duration::weeks(1))
        .trunc_subsecs(0)
        .to_rfc3339();
    let placeholders = HashMap::from([("id", id), ("deadline", deadline.as_str())]);

    for (rel_path, content) in crate::template::render_tree(kind_name, &placeholders)? {
        let dst = dir.join(&rel_path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dst, content)?;
    }

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
    use crate::spec::Spec;

    #[test]
    fn init_produces_a_loadable_library_package() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = init(dir.path(), "hw3", AssignmentKind::Library).unwrap();

        assert!(outcome.dir.join("autograder.toml").is_file());
        assert!(outcome.dir.join("Cargo.toml").is_file());
        assert!(outcome.dir.join("hw3/Cargo.toml").is_file());
        assert!(outcome.dir.join("hw3/src/lib.rs").is_file());
        assert!(outcome.dir.join("harness/Cargo.toml").is_file());
        assert!(outcome.dir.join("harness/src/main.rs").is_file());
        assert!(outcome.dir.join("harness/tests/judge.rs").is_file());

        let spec = Spec::load_file(&outcome.dir.join("autograder.toml")).unwrap();
        assert_eq!(spec.assignment.id, "hw3");
        assert_eq!(spec.assignment.kind, AssignmentKind::Library);
        assert!(spec.assignment.deadline > chrono::Local::now());
    }

    #[test]
    fn init_produces_a_loadable_binary_package_with_no_harness_dir() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = init(dir.path(), "wc", AssignmentKind::Binary).unwrap();

        assert!(outcome.dir.join("wc/src/main.rs").is_file());
        assert!(outcome.dir.join("wc/tests/judge.rs").is_file());
        assert!(!outcome.dir.join("harness").exists());

        let spec = Spec::load_file(&outcome.dir.join("autograder.toml")).unwrap();
        assert_eq!(spec.assignment.kind, AssignmentKind::Binary);
        assert_eq!(
            std::fs::read_to_string(outcome.dir.join("Cargo.toml")).unwrap(),
            "[workspace]\nresolver = \"3\"\nmembers = [\"wc\"]\n"
        );
    }

    #[test]
    fn init_rejects_an_id_that_would_be_an_invalid_package_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = init(dir.path(), "3-bad start", AssignmentKind::Library).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn init_refuses_to_write_into_a_nonempty_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "content").unwrap();

        let err = init(dir.path(), "hw3", AssignmentKind::Library).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn generated_solution_crate_builds() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = init(dir.path(), "hw3", AssignmentKind::Library).unwrap();

        let build = std::process::Command::new("cargo")
            .arg("build")
            .current_dir(outcome.dir.join("hw3"))
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "generated solution crate failed to build: {}",
            String::from_utf8_lossy(&build.stderr)
        );
    }
}
