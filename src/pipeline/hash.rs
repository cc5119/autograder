//! The input digest that decides whether an evaluation is still current.
//!
//! Covers both halves of what an evaluation is computed from: the student's
//! checkout, and the instructor material that judges it (the same files
//! `package_rules` copies, plus the spec). Hashing only the submission
//! would leave every result stale-blind to a fixed test or a changed
//! scoring policy -- the harness is as much an input as the student's code.
//!
//! `.git` is excluded: its internals (pack ordering, ref logs) differ
//! between clones of the same commit, so including it would make an
//! identical re-fetch look like a changed submission and defeat skipping.
//!
//! The spec goes in field by field (see [`spec_digest`]) rather than as raw
//! bytes, because only some of it is an evaluation input.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::exec::fs;
use crate::model::InputHash;
use crate::spec::Spec;

/// Hashes everything one evaluation would be computed from. Two runs with
/// equal hashes would do identical work, which is what lets `evaluate`
/// skip and `grade`/`show` report staleness.
pub fn input_hash(checkout_dir: &Path, assignment_dir: &Path, spec: &Spec) -> Result<InputHash> {
    let mut entries = Vec::new();

    for rel in fs::walk_regular_files(checkout_dir)? {
        if rel.starts_with(".git") {
            continue;
        }
        entries.push((
            format!("submission/{}", rel.display()),
            checkout_dir.join(rel),
        ));
    }

    for name in ["Cargo.toml", "Cargo.lock"] {
        entries.push((format!("instructor/{name}"), assignment_dir.join(name)));
    }
    let harness_dir = assignment_dir.join(&spec.assignment.harness);
    for rel in fs::walk_regular_files(&harness_dir)? {
        entries.push((
            format!("instructor/{}/{}", spec.assignment.harness, rel.display()),
            harness_dir.join(rel),
        ));
    }

    // Sorted so the digest depends on the file set, not on the order the
    // directory walk happened to return.
    entries.sort();

    let mut hasher = Sha256::new();
    hasher.update(spec_digest(spec).as_bytes());
    for (label, path) in entries {
        if !path.is_file() {
            continue;
        }
        let contents = fs::read(&path)?;
        // Length-prefixed: without it, a rename could be offset by a
        // content change and land on the same digest.
        hasher.update(label.as_bytes());
        hasher.update(b"\0");
        hasher.update(contents.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(&contents);
    }

    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(InputHash::new(hex))
}

/// The parts of the spec an evaluation actually depends on, rendered
/// canonically. Deliberately not the file's bytes:
///
/// * `[scoring]` is applied by `commands::grade` to an already-persisted
///   result, fresh from disk every run -- retuning a formula changes
///   scores without changing what any evaluation would produce.
/// * `[assignment].deadline` is the Fetch stage's input; by the time a
///   checkout is on disk, evaluate never consults it.
///
/// Hashing either would make an edit that provably cannot change an
/// `EvaluationResult` invalidate every one of them.
fn spec_digest(spec: &Spec) -> String {
    let limits = &spec.build_limits;
    format!(
        "id={}\nharness={}\ncargo-lock-sha256={}\nimage={}\n\
         wall-clock-ms={}\ncpus={}\nmemory={}\npids={}\nmax-output-bytes={}\n",
        spec.assignment.id,
        spec.assignment.harness,
        spec.assignment.cargo_lock_sha256,
        spec.sandbox.image,
        limits.wall_clock.0.as_millis(),
        limits.cpus,
        limits.memory.0,
        limits.pids,
        limits.max_output_bytes.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{SPEC_FILE, Spec};

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A minimal assignment dir: the spec plus the manifests and harness
    /// the instructor half of the hash reads.
    fn assignment(dir: &Path, harness_test: &str) -> Spec {
        assignment_with(dir, harness_test, "30s", "formula = \"sum\"\nbase = 0.0")
    }

    /// `wall_clock` and `scoring` are the two knobs the tests below vary:
    /// one is an evaluation input and one isn't.
    fn assignment_with(dir: &Path, harness_test: &str, wall_clock: &str, scoring: &str) -> Spec {
        write(
            &dir.join(SPEC_FILE),
            &format!(
                r#"
[assignment]
id = "hw3"
deadline = "2026-02-14T23:59:59[UTC]"
harness = "harness"
cargo-lock-sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[sandbox]
image = "example/image:latest"

[build-limits]
wall-clock = "{wall_clock}"
cpus = 1
memory = "512MiB"
pids = 64
max-output-bytes = "64KiB"

[scoring]
{scoring}
"#
            ),
        );
        write(&dir.join("Cargo.toml"), "[workspace]\n");
        write(&dir.join("Cargo.lock"), "version = 4\n");
        write(&dir.join("harness/tests/public.rs"), harness_test);
        Spec::load(dir).unwrap()
    }

    #[test]
    fn identical_inputs_hash_identically() {
        let checkout = tempfile::tempdir().unwrap();
        let assignment_dir = tempfile::tempdir().unwrap();
        write(&checkout.path().join("hw3/src/lib.rs"), "pub fn f() {}");
        let spec = assignment(assignment_dir.path(), "fn t() {}");

        let a = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();
        let b = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        assert_eq!(a, b);
    }

    #[test]
    fn changing_the_submission_changes_the_hash() {
        let checkout = tempfile::tempdir().unwrap();
        let assignment_dir = tempfile::tempdir().unwrap();
        write(&checkout.path().join("hw3/src/lib.rs"), "pub fn f() {}");
        let spec = assignment(assignment_dir.path(), "fn t() {}");
        let before = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        write(&checkout.path().join("hw3/src/lib.rs"), "pub fn f() { 1 }");
        let after = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        assert_ne!(before, after);
    }

    /// The whole reason the instructor side is in the digest: fixing a
    /// test has to invalidate results produced by the old one.
    #[test]
    fn changing_the_harness_changes_the_hash() {
        let checkout = tempfile::tempdir().unwrap();
        let assignment_dir = tempfile::tempdir().unwrap();
        write(&checkout.path().join("hw3/src/lib.rs"), "pub fn f() {}");
        let spec = assignment(assignment_dir.path(), "fn t() {}");
        let before = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        write(
            &assignment_dir.path().join("harness/tests/public.rs"),
            "fn t() { assert!(true) }",
        );
        let after = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        assert_ne!(before, after);
    }

    #[test]
    fn git_metadata_is_ignored() {
        let checkout = tempfile::tempdir().unwrap();
        let assignment_dir = tempfile::tempdir().unwrap();
        write(&checkout.path().join("hw3/src/lib.rs"), "pub fn f() {}");
        let spec = assignment(assignment_dir.path(), "fn t() {}");
        let before = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        write(&checkout.path().join(".git/HEAD"), "ref: refs/heads/main\n");
        let after = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        assert_eq!(before, after);
    }

    /// `grade` applies the scoring policy to an already-persisted result,
    /// so retuning a formula must not invalidate a single evaluation.
    #[test]
    fn changing_the_scoring_policy_does_not_change_the_hash() {
        let checkout = tempfile::tempdir().unwrap();
        let assignment_dir = tempfile::tempdir().unwrap();
        write(&checkout.path().join("hw3/src/lib.rs"), "pub fn f() {}");

        let before = {
            let spec = assignment_with(
                assignment_dir.path(),
                "fn t() {}",
                "30s",
                "formula = \"sum\"\nbase = 0.0",
            );
            input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap()
        };
        let after = {
            let spec = assignment_with(
                assignment_dir.path(),
                "fn t() {}",
                "30s",
                "formula = \"affine\"\nmax-sum = 9.0\nscale-min = 1.0\nscale-max = 7.0",
            );
            input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap()
        };

        assert_eq!(before, after);
    }

    /// A build limit is a real evaluation input: the same submission can
    /// pass under a longer wall clock and time out under a shorter one.
    #[test]
    fn changing_a_build_limit_changes_the_hash() {
        let checkout = tempfile::tempdir().unwrap();
        let assignment_dir = tempfile::tempdir().unwrap();
        write(&checkout.path().join("hw3/src/lib.rs"), "pub fn f() {}");

        let before = {
            let spec = assignment(assignment_dir.path(), "fn t() {}");
            input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap()
        };
        let after = {
            let spec = assignment_with(
                assignment_dir.path(),
                "fn t() {}",
                "120s",
                "formula = \"sum\"\nbase = 0.0",
            );
            input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap()
        };

        assert_ne!(before, after);
    }

    #[test]
    fn a_renamed_file_changes_the_hash_even_with_identical_contents() {
        let checkout = tempfile::tempdir().unwrap();
        let assignment_dir = tempfile::tempdir().unwrap();
        let spec = assignment(assignment_dir.path(), "fn t() {}");
        write(&checkout.path().join("hw3/src/a.rs"), "pub fn f() {}");
        let before = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        std::fs::remove_file(checkout.path().join("hw3/src/a.rs")).unwrap();
        write(&checkout.path().join("hw3/src/b.rs"), "pub fn f() {}");
        let after = input_hash(checkout.path(), assignment_dir.path(), &spec).unwrap();

        assert_ne!(before, after);
    }
}
