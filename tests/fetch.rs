//! Integration tests for the Fetch stage's public workflow
//! (`LocalPath::fetch`, `DirectorySource`, `fetch_batch`, `read_fetch_record`).
//! The `git`-argv-builder and `run_git` unit tests stay in `src/fetch.rs`,
//! since they exercise private functions unreachable from here.

use autograder::fetch::{DirectorySource, Fetchable, fetch_batch, read_fetch_record};
use autograder::model::{LocalPath, StageStatus};
use autograder::source::SubmissionsSource;
use jiff::Zoned;

fn local_path(path: impl Into<std::path::PathBuf>) -> LocalPath {
    LocalPath(path.into())
}

/// `LocalPath::fetch` ignores the deadline entirely -- this stands in for
/// it in tests that don't care what it is.
fn test_deadline() -> Zoned {
    "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
        .parse()
        .unwrap()
}

#[test]
fn fetches_a_populated_directory_into_a_workspace() {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("main.rs"), b"fn main() {}").unwrap();
    let dest = tempfile::tempdir().unwrap();
    let workspace = dest.path().join("job");

    let outcome = local_path(src.path())
        .fetch(&workspace, &test_deadline())
        .unwrap();

    assert_eq!(outcome.status, StageStatus::Ok);
    assert!(workspace.join("main.rs").exists());
    assert!(outcome.graded_commit.is_some());
}

#[test]
fn missing_directory_yields_fetch_failed_not_a_panic() {
    let dest = tempfile::tempdir().unwrap();
    let workspace = dest.path().join("job");

    let outcome = local_path("/nonexistent/path/for/sure")
        .fetch(&workspace, &test_deadline())
        .unwrap();

    assert_eq!(outcome.status, StageStatus::FetchFailed);
    assert!(outcome.workspace.is_none());
}

#[test]
fn empty_directory_yields_fetch_failed() {
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let workspace = dest.path().join("job");

    let outcome = local_path(src.path())
        .fetch(&workspace, &test_deadline())
        .unwrap();

    assert_eq!(outcome.status, StageStatus::FetchFailed);
}

#[test]
fn directory_source_treats_each_subdir_as_a_student() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("alice")).unwrap();
    std::fs::create_dir(root.path().join("bob")).unwrap();

    let submissions = DirectorySource::new(root.path()).submissions().unwrap();

    assert_eq!(submissions.len(), 2);
    assert_eq!(submissions[0].student_id, "alice");
    assert_eq!(submissions[1].student_id, "bob");
}

#[test]
fn fetch_batch_writes_a_fetch_record_per_student() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("alice")).unwrap();
    std::fs::write(root.path().join("alice/main.rs"), b"fn main() {}").unwrap();
    let source = DirectorySource::new(root.path());
    let work_dir = tempfile::tempdir().unwrap();

    let records = fetch_batch(&source, work_dir.path(), &test_deadline()).unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, "alice");
    assert_eq!(records[0].1.status, StageStatus::Ok);
    assert!(work_dir.path().join("alice/checkout/main.rs").is_file());

    let record = read_fetch_record(&work_dir.path().join("alice"))
        .unwrap()
        .unwrap();
    assert_eq!(record.status, StageStatus::Ok);
    assert!(record.graded_commit.is_some());
}

#[test]
fn read_fetch_record_is_none_when_nothing_has_fetched_yet() {
    let work_dir = tempfile::tempdir().unwrap();
    assert!(
        read_fetch_record(&work_dir.path().join("alice"))
            .unwrap()
            .is_none()
    );
}
