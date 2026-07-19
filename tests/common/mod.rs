//! Shared fixtures for the integration test modules under `tests/`. Package
//! fixtures are built via the real `autograder::init::init` (which itself
//! runs `autograder::lock::lock`), never hand-written TOML/lockfile
//! strings -- that way a fixture can never drift from what the templates
//! and schema actually produce, and a regression in either gets caught
//! here instead of silently tolerated by a stale stand-in.

use std::path::Path;
use std::process::Command;

use autograder::spec::{AssignmentKind, Spec};

pub fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Scaffolds a real `library`-kind instructor package at `dir` (via
/// `init`, which also locks it) and returns the loaded spec. Callers
/// overwrite/add files under `dir` afterward for test-specific content
/// (e.g. `harness/tests/judge.rs`) -- editing content, not dependencies,
/// never invalidates the lock `init` already produced.
pub fn library_package(dir: &Path, id: &str) -> Spec {
    autograder::init::init(dir, id, AssignmentKind::Library).unwrap();
    Spec::load(dir).unwrap()
}

/// Same as [`library_package`], for `binary`-kind.
pub fn binary_package(dir: &Path, id: &str) -> Spec {
    autograder::init::init(dir, id, AssignmentKind::Binary).unwrap();
    Spec::load(dir).unwrap()
}

/// A `Command` for the real, compiled `autograder` binary -- for
/// subprocess-level tests that need to exercise the actual CLI, not just
/// the library functions it delegates to.
pub fn autograder_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_autograder"))
}
