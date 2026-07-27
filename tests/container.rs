//! Grade-integrity regression tests driving the real `Nextest` evaluator
//! against a real `ContainerSandbox` -- needs podman + the base image (gated
//! by `Cargo.toml`'s `container-tests` feature).

#[allow(dead_code)]
mod common;

use autograder::exec::sandbox::{ContainerSandbox, Sandbox};
use autograder::id::AssignmentId;
use autograder::pipeline::evaluate_batch;
use autograder::pipeline::evaluator::nextest::Nextest;
use autograder::spec::Spec;
use autograder::store::Store;

use crate::common::{library_package, ok_fetch_record, write, write_fetch_record};

const DEFAULT_IMAGE: &str = "ghcr.io/cc5119/autograder-base:latest";

fn sandbox() -> ContainerSandbox {
    let image = std::env::var("AUTOGRADER_TEST_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.into());
    let sandbox = ContainerSandbox::new(image);
    sandbox
        .preflight()
        .expect("podman + the base image must be usable to run the container suite");
    sandbox
}

/// A student `build.rs` compiles before the judge and, on a
/// vulnerable build, overwrites the harness to forge the grade. Fixed, the
/// harness is un-writable in the sandbox and the real judge survives.
#[test]
fn student_build_script_cannot_forge_the_grade_by_overwriting_the_harness() {
    let package_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    // Trusted judge scores a known baseline off the student lib.
    let _ = library_package(package_dir.path(), "hw3");
    let autograder_toml = package_dir.path().join("autograder.toml");
    let contents = std::fs::read_to_string(&autograder_toml).unwrap();
    std::fs::write(
        &autograder_toml,
        contents.replace("base = 1.0", "base = 0.0"),
    )
    .unwrap();
    let spec = Spec::load(package_dir.path()).unwrap();
    write(
        &package_dir.path().join("harness/tests/judge.rs"),
        "#[test]\nfn trusted_judge() {\n    assert!(hw3::example());\n    println!(\"autograder: score=1\");\n}\n",
    );

    // Untrusted submission: build.rs tries to replace the judge.
    write(
        &submissions_dir.path().join("mallory/hw3/Cargo.toml"),
        "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        &submissions_dir.path().join("mallory/hw3/src/lib.rs"),
        "pub fn example() -> bool { true }\n",
    );
    write(
        &submissions_dir.path().join("mallory/hw3/build.rs"),
        "fn main() {\n    std::fs::write(\n        \"../harness/tests/judge.rs\",\n        \"#[test] fn all_pass() { println!(\\\"autograder: score=1000000\\\"); }\",\n    )\n    .ok();\n}\n",
    );
    write_fetch_record(submissions_dir.path(), "mallory", &ok_fetch_record());

    let evaluator = Nextest::new(&spec, package_dir.path(), sandbox()).unwrap();
    let store = Store::new(store_dir.path());

    let evals = evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
    )
    .unwrap();

    assert_eq!(evals.len(), 1);
    let stored_evals = store.latest_evals(AssignmentId::new("hw3")).unwrap();
    let names: Vec<&str> = stored_evals[0]
        .tests
        .iter()
        .map(|t| t.name.as_str())
        .collect();

    assert!(
        names.iter().any(|n| n.contains("trusted_judge")),
        "the trusted judge did not run; harness was overwritten. tests: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("all_pass")),
        "student's injected judge ran -- harness was writable (CRITICAL #1). tests: {names:?}"
    );

    let grade = autograder::pipeline::grade::grade(&evals[0], &spec.scoring);
    assert_eq!(
        grade.score, 1.0,
        "expected the trusted judge's baseline, not a forged score"
    );
}
