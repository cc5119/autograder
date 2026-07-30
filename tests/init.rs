//! Integration tests for `autograder::package::init::init` -- everything here only
//! touches its public API, so it lives here rather than inline in
//! `src/init.rs` (see `src/init.rs`'s own `is_valid_id` unit test for the
//! one piece of private logic worth isolating).

use autograder::error::Error;
use autograder::spec::Spec;
use jiff::Zoned;

#[test]
fn init_produces_a_loadable_library_package() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = autograder::package::init::init(dir.path(), "hw3").unwrap();

    assert!(outcome.dir.join("autograder.toml").is_file());
    assert!(outcome.dir.join("Cargo.toml").is_file());
    assert!(outcome.dir.join("hw3/Cargo.toml").is_file());
    assert!(outcome.dir.join("hw3/src/lib.rs").is_file());
    assert!(outcome.dir.join("harness/Cargo.toml").is_file());
    assert!(outcome.dir.join("harness/src/bin/driver.rs").is_file());
    assert!(outcome.dir.join("harness/tests/judge.rs").is_file());

    let spec = Spec::load_file(&outcome.dir.join("autograder.toml")).unwrap();
    assert_eq!(spec.assignment.id, "hw3");
    assert!(spec.assignment.deadline > Zoned::now());
}

#[test]
fn init_rejects_an_id_that_would_be_an_invalid_package_name() {
    let dir = tempfile::tempdir().unwrap();
    let err = autograder::package::init::init(dir.path(), "3-bad start").unwrap_err();
    assert!(matches!(err, Error::InvalidSpec(_)));
}

#[test]
fn init_refuses_to_write_into_a_nonempty_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("existing.txt"), "content").unwrap();

    let err = autograder::package::init::init(dir.path(), "hw3").unwrap_err();
    assert!(matches!(err, Error::InvalidSpec(_)));
}

#[test]
fn generated_solution_crate_builds() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = autograder::package::init::init(dir.path(), "hw3").unwrap();

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
