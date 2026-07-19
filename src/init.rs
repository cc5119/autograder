//! Scaffolds a brand-new **private instructor package** from nothing --
//! `publish`'s inverse: that module derives the public starter tree from
//! an existing private package (design §5.1); this one originates the
//! private package itself, so there's something for `publish` to derive
//! from in the first place.
//!
//! Declarative in the most literal sense: the layout to generate *is* a
//! real directory tree, `templates/<kind>/` (embedded into the binary at
//! compile time via [`include_dir`]), not a Rust table describing one.
//! Adding, renaming, or restructuring a generated file is an edit to that
//! tree -- real TOML/Rust source with normal syntax highlighting, not a
//! Rust string literal -- never a change to this module's code. `init`
//! just walks whichever kind's subtree recursively, substitutes
//! placeholders (see [`substitute`]) into every file's path and contents,
//! strips the `.tmpl` suffix, and writes the result.
//!
//! What's generated is a *template* to edit, not a finished package:
//! `{deadline}` renders to "now + one week" (a plausible, obviously-
//! provisional starting point, not a permanent-looking artifact like a
//! hardcoded far-future date) and `[assignment].name` is a literal
//! `"TODO: ..."` marker -- both meant to be replaced. `[sandbox].image`,
//! `[limits]`, and `[scoring].model` are ordinary, reusable defaults that
//! don't need editing to be correct, mirroring the values already used
//! throughout this crate's own test fixtures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{Duration, Local, SubsecRound};
use include_dir::{Dir, File, include_dir};

use crate::error::{Error, Result};
use crate::fs;
use crate::spec::AssignmentKind;

static TEMPLATES: Dir = include_dir!("$CARGO_MANIFEST_DIR/templates");

#[derive(Debug, Clone)]
pub struct InitOutcome {
    pub dir: PathBuf,
}

/// Writes a fresh private instructor package at `dir` (must not already
/// exist, or must be empty -- `init` refuses to write into a directory
/// that might already hold something, same "refuse to guess" posture
/// `publish` takes toward its own required inputs).
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
    let template_root = TEMPLATES.get_dir(kind_name).unwrap_or_else(|| {
        panic!("templates/{kind_name}/ is missing from the embedded template tree")
    });

    // Truncated to whole seconds -- a placeholder with nanosecond noise
    // reads as generated cruft rather than a value meant to be edited.
    let deadline = (Local::now() + Duration::weeks(1))
        .trunc_subsecs(0)
        .to_rfc3339();
    let placeholders = HashMap::from([("id", id), ("deadline", deadline.as_str())]);

    for file in walk_files(template_root) {
        let rel_path = file
            .path()
            .strip_prefix(template_root.path())
            .expect("file is under template_root by construction")
            .with_extension(""); // drop the trailing `.tmpl`
        let rel_path = substitute(&rel_path.to_string_lossy(), &placeholders);
        let contents = file
            .contents_utf8()
            .unwrap_or_else(|| panic!("template {} is not valid UTF-8", file.path().display()));

        let dst = dir.join(&rel_path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dst, substitute(contents, &placeholders))?;
    }

    Ok(InitOutcome {
        dir: dir.to_path_buf(),
    })
}

/// Every file under `dir`, recursively -- `include_dir`'s `Dir` only
/// exposes direct children (`files()`/`dirs()`), so this walks the tree
/// itself.
fn walk_files<'a>(dir: &'a Dir<'a>) -> Vec<&'a File<'a>> {
    let mut files: Vec<&File> = dir.files().collect();
    for subdir in dir.dirs() {
        files.extend(walk_files(subdir));
    }
    files
}

/// Single-pass substitution of `{key}` tokens against `placeholders`: a
/// `{` is only ever treated as the start of a placeholder if the text up
/// to the *next* `}` exactly matches a known key -- anything else (an
/// unrelated `{...}`, e.g. TOML's own inline-table syntax like
/// `{ path = "../{id}" }`) is emitted verbatim, character by character,
/// which is also exactly why a single multi-key pass (not one `.replace`
/// call per key run back-to-back) is what's needed here: chained
/// `.replace` calls risk one substitution's *output* being reinterpreted
/// by the next call, which a single left-to-right scan never does.
fn substitute(text: &str, placeholders: &HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'{'
            && let Some(rel_end) = text[i + 1..].find('}')
        {
            let key = &text[i + 1..i + 1 + rel_end];
            if let Some(value) = placeholders.get(key) {
                out.push_str(value);
                i += rel_end + 2; // past the closing `}`
                continue;
            }
        }
        let ch = text[i..].chars().next().expect("i < text.len()");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
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
    fn substitute_replaces_only_exact_known_keys() {
        let placeholders = HashMap::from([("id", "hw3")]);
        assert_eq!(substitute("id = \"{id}\"", &placeholders), "id = \"hw3\"");
        // TOML's own inline-table braces must survive untouched, with the
        // placeholder nested inside them still substituted.
        assert_eq!(
            substitute("{id} = { path = \"../{id}\" }", &placeholders),
            "hw3 = { path = \"../hw3\" }"
        );
        // An unrelated brace pair that isn't a known key is left alone.
        assert_eq!(
            substitute("println!(\"{}\", x)", &placeholders),
            "println!(\"{}\", x)"
        );
    }

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
