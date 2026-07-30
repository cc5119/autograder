//! Integration tests for `autograder::package::publish::publish`. Packages are
//! scaffolded via `common::library_package` (the real `init`), then
//! individual tests overwrite just the file(s) relevant to what they're
//! checking -- never a second, hand-written copy of the schema/template
//! shape.

use autograder::error::Error;
use autograder::package::publish::{PublishMode, publish};
use autograder::spec::SPEC_FILE;

use crate::common::{library_package, write};

const JUDGE_RS: &str = r#"
    #[test]
    fn insert_basic() {
        assert!(true);
    }

    #[cfg(not(feature = "student"))]
    #[test]
    fn balance_adversarial() {
        assert!(true);
    }
"#;

const SOLUTION_SRC: &str = r#"
    #[cfg(not(feature = "student"))]
    use std::collections::HashSet;

    pub struct Stack<T> {
        items: Vec<T>,
    }

    impl<T> Stack<T> {
        pub fn new() -> Self {
            Stack { items: Vec::new() }
        }

        pub fn push(&mut self, value: T) {
            cfg_select! {
                feature = "student" => {
                    todo!()
                }
                _ => {
                    self.items.push(value);
                }
            }
        }

        pub fn len(&self) -> usize {
            self.items.len()
        }

        #[cfg(not(feature = "student"))]
        fn dedup_hint(&self) -> HashSet<usize> {
            HashSet::new()
        }
    }
"#;

#[test]
fn publish_produces_the_documented_starter_tree() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");

    let out_dir = tempfile::tempdir().unwrap();
    let outcome = publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    assert!(outcome.out_dir.join(SPEC_FILE).is_file());
    assert!(outcome.out_dir.join("harness/tests/judge.rs").is_file());
    assert!(
        outcome
            .out_dir
            .join(".github/workflows/autograde.yml")
            .is_file()
    );
    assert!(outcome.out_dir.join("Cargo.toml").is_file());
    assert!(outcome.out_dir.join("hw3/Cargo.toml").is_file());
    assert!(outcome.out_dir.join("hw3/src/lib.rs").is_file());
}

#[test]
fn publish_ships_the_spec_file_verbatim_under_the_same_name() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    let out_dir = tempfile::tempdir().unwrap();

    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    let source = std::fs::read_to_string(package_dir.path().join(SPEC_FILE)).unwrap();
    let shipped = std::fs::read_to_string(out_dir.path().join(SPEC_FILE)).unwrap();
    assert_eq!(shipped, source);
}

#[test]
fn publish_ships_the_workspace_root_cargo_lock_verbatim() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    let out_dir = tempfile::tempdir().unwrap();

    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    let source = std::fs::read_to_string(package_dir.path().join("Cargo.lock")).unwrap();
    let shipped = std::fs::read_to_string(out_dir.path().join("Cargo.lock")).unwrap();
    assert_eq!(shipped, source);
}

#[test]
fn publish_refuses_to_ship_a_stale_cargo_lock() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    // The instructor changed a dependency but forgot to rerun `autograder
    // lock` -- the checked-in Cargo.lock no longer matches the hash
    // recorded in autograder.toml.
    write(
        &package_dir.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"hw3\"\nversion = \"9.9.9\"\n",
    );
    let out_dir = tempfile::tempdir().unwrap();

    let err = publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap_err();
    assert!(matches!(err, Error::InvalidSpec(_)));
    assert!(!out_dir.path().join(SPEC_FILE).exists());
}

#[test]
fn publish_derives_a_public_harness_with_only_the_kept_test_and_a_path_dependency() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    write(&package_dir.path().join("harness/tests/judge.rs"), JUDGE_RS);
    let out_dir = tempfile::tempdir().unwrap();

    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    let judge = std::fs::read_to_string(out_dir.path().join("harness/tests/judge.rs")).unwrap();
    assert!(judge.contains("fn insert_basic"));
    assert!(!judge.contains("balance_adversarial"));

    let manifest = std::fs::read_to_string(out_dir.path().join("harness/Cargo.toml")).unwrap();
    assert!(!manifest.contains("patch"));
    assert!(manifest.contains("path = \"../hw3\""));
}

#[test]
fn publish_errors_clearly_when_the_solution_directory_is_missing() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    std::fs::remove_dir_all(package_dir.path().join("hw3")).unwrap();
    let out_dir = tempfile::tempdir().unwrap();

    let err = publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap_err();
    assert!(matches!(err, Error::InvalidSpec(_)));
}

#[test]
fn emitted_workflow_runs_ci_from_the_repo_root_inside_podman() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    let out_dir = tempfile::tempdir().unwrap();

    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    let workflow =
        std::fs::read_to_string(out_dir.path().join(".github/workflows/autograde.yml")).unwrap();
    assert!(workflow.contains("on:\n  push:\n    branches: [main]"));
    assert!(workflow.contains("sha256sum -c -"));
    assert!(workflow.contains("command -v podman"));
    assert!(workflow.contains("./autograder vendor ."));
    assert!(workflow.contains("podman pull autograder-base:1.86.0"));
    assert!(workflow.contains("./autograder ci"));
    assert!(!workflow.contains("--harness"));
}

#[test]
fn emitted_workspace_manifest_matches_the_source_verbatim() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    let out_dir = tempfile::tempdir().unwrap();

    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    let source = std::fs::read_to_string(package_dir.path().join("Cargo.toml")).unwrap();
    let shipped = std::fs::read_to_string(out_dir.path().join("Cargo.toml")).unwrap();
    assert_eq!(shipped, source);
    assert!(shipped.contains("harness"));
    assert!(shipped.contains("hw3"));
}

#[test]
fn emitted_student_manifest_matches_the_solutions_own_cargo_toml() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    let out_dir = tempfile::tempdir().unwrap();

    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    let solution_manifest =
        std::fs::read_to_string(package_dir.path().join("hw3/Cargo.toml")).unwrap();
    let starter_manifest = std::fs::read_to_string(out_dir.path().join("hw3/Cargo.toml")).unwrap();
    assert_eq!(starter_manifest, solution_manifest);
}

#[test]
fn publish_rejects_a_package_dir_without_a_private_spec() {
    let package_dir = tempfile::tempdir().unwrap();
    let out_dir = tempfile::tempdir().unwrap();

    let err = publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap_err();
    assert!(matches!(err, Error::InvalidSpec(_)));
}

#[test]
fn publish_rejects_a_package_dir_without_a_root_workspace_manifest() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    std::fs::remove_file(package_dir.path().join("Cargo.toml")).unwrap();
    let out_dir = tempfile::tempdir().unwrap();

    let err = publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap_err();
    assert!(matches!(err, Error::InvalidSpec(_)));
}

#[test]
fn publish_derives_a_building_stub_from_the_id_named_solution_dir() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    write(&package_dir.path().join("hw3/src/lib.rs"), SOLUTION_SRC);
    write(
        &package_dir.path().join("publish.toml"),
        "allowed-warnings = [\"unused_variables\"]\n",
    );

    let out_dir = tempfile::tempdir().unwrap();
    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    let src = std::fs::read_to_string(out_dir.path().join("hw3/src/lib.rs")).unwrap();
    assert!(src.contains("pub struct Stack"));
    assert!(src.contains("pub fn new"));
    assert!(src.contains("pub fn push"));
    assert!(src.contains("todo!"));
    assert!(!src.contains("dedup_hint"));
    assert!(!src.contains("HashSet"));

    let build = std::process::Command::new("cargo")
        .arg("build")
        .current_dir(out_dir.path().join("hw3"))
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "published starter failed to build: {}",
        String::from_utf8_lossy(&build.stderr)
    );
}

#[test]
fn cargo_test_at_the_starter_root_runs_the_public_harness() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    write(&package_dir.path().join("harness/tests/judge.rs"), JUDGE_RS);

    let out_dir = tempfile::tempdir().unwrap();
    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    let test = std::process::Command::new("cargo")
        .arg("test")
        .current_dir(out_dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&test.stdout);
    assert!(
        test.status.success(),
        "cargo test at the starter root failed: {}{}",
        stdout,
        String::from_utf8_lossy(&test.stderr)
    );
    assert!(stdout.contains("insert_basic"));
}

#[test]
fn publish_solution_mode_keeps_the_real_implementation_and_reference_only_helpers() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    write(&package_dir.path().join("hw3/src/lib.rs"), SOLUTION_SRC);
    write(
        &package_dir.path().join("publish.toml"),
        "allowed-warnings = [\"unused_variables\"]\n",
    );

    let out_dir = tempfile::tempdir().unwrap();
    publish(package_dir.path(), out_dir.path(), PublishMode::Solution).unwrap();

    let src = std::fs::read_to_string(out_dir.path().join("hw3/src/lib.rs")).unwrap();
    assert!(src.contains("pub struct Stack"));
    assert!(src.contains("self.items.push(value)"));
    assert!(!src.contains("todo!"));
    assert!(src.contains("dedup_hint"));
    assert!(src.contains("HashSet"));

    let build = std::process::Command::new("cargo")
        .arg("build")
        .current_dir(out_dir.path().join("hw3"))
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "published solution failed to build: {}",
        String::from_utf8_lossy(&build.stderr)
    );
}

#[test]
fn publish_solution_mode_still_strips_the_harnesss_adversarial_tests() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    write(&package_dir.path().join("harness/tests/judge.rs"), JUDGE_RS);
    let out_dir = tempfile::tempdir().unwrap();

    publish(package_dir.path(), out_dir.path(), PublishMode::Solution).unwrap();

    let judge = std::fs::read_to_string(out_dir.path().join("harness/tests/judge.rs")).unwrap();
    assert!(judge.contains("fn insert_basic"));
    assert!(!judge.contains("balance_adversarial"));
}

#[test]
fn publish_rejects_a_solution_dir_whose_package_name_does_not_match_the_id() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    write(
        &package_dir.path().join("hw3/Cargo.toml"),
        "[package]\nname = \"wrong-name\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );

    let out_dir = tempfile::tempdir().unwrap();
    let err = publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap_err();
    assert!(matches!(err, Error::InvalidSpec(_)));
}

#[test]
fn publish_never_copies_a_vendor_directory_dropped_in_the_solution_crate() {
    let package_dir = tempfile::tempdir().unwrap();
    library_package(package_dir.path(), "hw3");
    write(
        &package_dir.path().join("hw3/vendor/some-crate/src/lib.rs"),
        "not checked into the starter\n",
    );
    let out_dir = tempfile::tempdir().unwrap();

    publish(package_dir.path(), out_dir.path(), PublishMode::Starter).unwrap();

    assert!(!out_dir.path().join("hw3/vendor").exists());
}

