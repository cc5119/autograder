//! Shared fixtures for the integration test modules under `tests/`. Package
//! fixtures are built via the real `autograder::package::init::init` (which itself
//! runs `autograder::deps::lock::lock`), never hand-written TOML/lockfile
//! strings -- that way a fixture can never drift from what the templates
//! and schema actually produce, and a regression in either gets caught
//! here instead of silently tolerated by a stale stand-in.

use std::path::Path;
use std::process::Command;

use autograder::model::StageStatus;
use autograder::spec::{AssignmentKind, Spec};
use autograder::submissions::FetchRecord;

pub fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A `FetchRecord` reporting success, as `autograder fetch` would have
/// written it -- the common case for tests that just need `evaluate_batch`
/// to see a prior fetch as having gone fine.
pub fn ok_fetch_record() -> FetchRecord {
    FetchRecord {
        status: StageStatus::Ok,
        graded_commit: Some("fixture".to_string()),
        message: None,
        fetched_at: jiff::Timestamp::now(),
        submission_date: None,
    }
}

/// Writes `record` to `<submissions_dir>/.meta/<student_id>.json` --
/// the exact layout `autograder fetch --out` produces, hand-built instead
/// of run through `fetch_batch` (which needs a real git remote). Pair with
/// writing the submission's own files directly at
/// `<submissions_dir>/<student_id>/<assignment.id>/...` for a complete
/// `evaluate_batch` fixture.
pub fn write_fetch_record(submissions_dir: &Path, student_id: &str, record: &FetchRecord) {
    write(
        &submissions_dir.join(".meta").join(format!("{student_id}.json")),
        &serde_json::to_string(record).unwrap(),
    );
}

/// Scaffolds a real `library`-kind instructor package at `dir` (via
/// `init`, which also locks it) and returns the loaded spec. Callers
/// overwrite/add files under `dir` afterward for test-specific content
/// (e.g. `harness/tests/judge.rs`) -- editing content, not dependencies,
/// never invalidates the lock `init` already produced.
pub fn library_package(dir: &Path, id: &str) -> Spec {
    autograder::package::init::init(dir, id, AssignmentKind::Library).unwrap();
    Spec::load(dir).unwrap()
}

/// Same as [`library_package`], for `binary`-kind.
pub fn binary_package(dir: &Path, id: &str) -> Spec {
    autograder::package::init::init(dir, id, AssignmentKind::Binary).unwrap();
    Spec::load(dir).unwrap()
}

/// A `Command` for the real, compiled `autograder` binary -- for
/// subprocess-level tests that need to exercise the actual CLI, not just
/// the library functions it delegates to.
pub fn autograder_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_autograder"))
}
