//! Emits the starter/template repo for distribution to students (design
//! §5.1, §11.2) from the **private instructor package** in one pass: copy
//! everything real, then strip the sensitive parts in place. No
//! hand-maintained `public/` sibling repo, and no separately-assembled
//! `Cargo.toml`/`src/` built up from spec fields in Rust code.
//!
//! `package_dir` is the instructor package (`autograder.toml`, `harness/`,
//! `<assignment-id>/` — the required reference solution). `[assignment].id`
//! is the single identifier for everything
//! student-facing (design §5): the crate name the harness's `Cargo.toml`
//! depends on (`evaluator::linked_library`), the binary name for
//! `binary-harness` (`prepare::Wiring::BinaryHarness`), *and* the directory
//! name the reference solution must live in — `package_dir.join(&spec.assignment.id)`.
//! No `[student]` section to keep in sync with it, no `--solution` flag.
//!
//! From `package_dir`, `scaffold` produces `out_dir` as:
//! - A full recursive copy of `package_dir/<id>/` (the reference solution —
//!   `Cargo.toml`, `src/`, anything else that's there), with every `.rs`
//!   file then stripped in place via [`crate::stub`]: pub signatures
//!   survive, bodies become `todo!()`, private items and `#[cfg(test)]`
//!   modules are dropped. `cargo fix` then runs directly in `out_dir` to
//!   prune imports the stripping left unused — no separate scratch copy,
//!   since `out_dir` already *is* a real, disposable copy of the solution
//!   crate at this point. A solution directory is required: if it's
//!   missing, or its own `Cargo.toml` `[package].name` doesn't match
//!   `[assignment].id`, `scaffold` refuses to guess and errors out clearly.
//! - `.autograder/public/` — `autograder.public.toml` with `points` and
//!   non-public `[[scoring.tests]]` entries stripped
//!   ([`crate::publish::derive_public_spec_toml`]), a copy of `harness/`
//!   with test files filtered down to the public-visibility tests named in
//!   that same spec transform ([`crate::publish::keep_only_named_tests`])
//!   and its `Cargo.toml`'s `[patch]` table dropped (it points at the
//!   solution directory, which doesn't exist once copied out).
//!   Deliberately *not* copied here: `vendor/`/`.cargo/` (`prefetch`'s
//!   offline-vendoring output) — that's grading-only infrastructure (design
//!   §8), not something the published starter carries; a student's own
//!   `autograder ci` run resolves dependencies normally rather than
//!   against the vendored allowlist. `fixtures/` isn't copied either —
//!   nothing in this codebase currently reads it (same reasoning as the
//!   `fixtures/` overlay removed from `prepare.rs`); reintroduce it,
//!   correctly scoped, once an evaluator actually needs fixture data.
//! - `.github/workflows/autograde.yml` — a thin wrapper around `autograder
//!   ci`. The one file with no private-repo counterpart to copy, so it's
//!   still generated.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::publish;
use crate::spec::{self, Spec};

/// Placeholder release coordinates for the downloaded `autograder` binary
/// (design §11.2). The real pinned version + sha256 come from the release
/// pipeline (M3 step 19, `.github/workflows/release.yml` in this repo); an
/// instructor stands up their own fork/release and edits these before
/// distributing a starter repo.
const RELEASE_REPO: &str = "your-org/autograder";
const RELEASE_VERSION: &str = "v0.1.0";
const RELEASE_SHA256_PLACEHOLDER: &str = "<pinned-sha256>";

#[derive(Debug, Clone)]
pub struct ScaffoldOutcome {
    pub out_dir: PathBuf,
}

/// Builds the starter template at `out_dir` from the private instructor
/// package at `package_dir`.
pub fn scaffold(package_dir: &Path, out_dir: &Path) -> Result<ScaffoldOutcome> {
    let private_spec_path = package_dir.join(spec::PRIVATE_SPEC_FILE);
    if !private_spec_path.is_file() {
        return Err(Error::InvalidSpec(format!(
            "scaffold requires {} in {} (the private instructor package)",
            spec::PRIVATE_SPEC_FILE,
            package_dir.display()
        )));
    }
    let spec = Spec::load_file(&private_spec_path)?;

    let solution_dir = package_dir.join(&spec.assignment.id);
    if !solution_dir.is_dir() {
        return Err(Error::InvalidSpec(format!(
            "scaffold requires a reference solution at {} (a directory named after \
             [assignment].id) -- there is nothing to copy the starter's Cargo.toml/src/ from",
            solution_dir.display()
        )));
    }
    check_solution_package_name(&solution_dir, &spec.assignment.id)?;

    copy_dir_if_exists(&solution_dir, out_dir)?;
    strip_rust_files_in_place(out_dir)?;
    run_cargo_fix(out_dir)?;

    let private_toml = std::fs::read_to_string(&private_spec_path).map_err(|source| Error::Io {
        path: private_spec_path.clone(),
        source,
    })?;
    let (public_spec_toml, public_test_names) =
        publish::derive_public_spec_toml(&private_toml)?;

    let public_dir = out_dir.join(".autograder/public");
    std::fs::create_dir_all(&public_dir).map_err(|source| Error::Io {
        path: public_dir.clone(),
        source,
    })?;
    let public_spec_path = public_dir.join(spec::PUBLIC_SPEC_FILE);
    std::fs::write(&public_spec_path, public_spec_toml).map_err(|source| Error::Io {
        path: public_spec_path,
        source,
    })?;

    copy_public_harness(
        &package_dir.join("harness"),
        &public_dir.join("harness"),
        &public_test_names,
    )?;

    let workflow_dir = out_dir.join(".github/workflows");
    std::fs::create_dir_all(&workflow_dir).map_err(|source| Error::Io {
        path: workflow_dir.clone(),
        source,
    })?;
    let workflow_path = workflow_dir.join("autograde.yml");
    std::fs::write(&workflow_path, autograde_workflow_yaml()).map_err(|source| Error::Io {
        path: workflow_path.clone(),
        source,
    })?;

    Ok(ScaffoldOutcome {
        out_dir: out_dir.to_path_buf(),
    })
}

/// The thin GitHub Actions wrapper (design §11.2): downloads a
/// version-pinned prebuilt `autograder` binary, verifies its checksum, and
/// runs `autograder ci` against the public harness copied into
/// `.autograder/public/`. No compile step, so student CI never needs a Rust
/// toolchain of its own for the grader itself.
fn autograde_workflow_yaml() -> String {
    format!(
        r#"name: autograde
on:
  push:
    branches: [main]        # default branch only
jobs:
  public-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          curl -fsSL https://github.com/{RELEASE_REPO}/releases/download/{RELEASE_VERSION}/autograder-x86_64-linux -o autograder
          echo "{RELEASE_SHA256_PLACEHOLDER}  autograder" | sha256sum -c -   # verify before exec
          chmod +x autograder
      - run: ./autograder ci --harness .autograder/public
"#
    )
}

/// Refuses to guess: `solution_dir` is only trusted as the reference
/// solution for `assignment_id` if its own `Cargo.toml` says so. Catches a
/// stale/copy-pasted solution directory (wrong crate, or an id that was
/// renamed in `autograder.toml` without renaming the directory) before it
/// silently produces a stub with the wrong API shape.
fn check_solution_package_name(solution_dir: &Path, assignment_id: &str) -> Result<()> {
    let manifest_path = solution_dir.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).map_err(|source| Error::Io {
        path: manifest_path.clone(),
        source,
    })?;
    let value: toml::Value = toml::from_str(&manifest).map_err(|source| Error::Toml {
        path: manifest_path.clone(),
        source: Box::new(source),
    })?;
    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    if package_name != Some(assignment_id) {
        return Err(Error::InvalidSpec(format!(
            "{} has [package].name = {:?}, expected {:?} to match [assignment].id -- \
             rename the package, or the solution directory/id, so they agree",
            manifest_path.display(),
            package_name.unwrap_or("<missing>"),
            assignment_id
        )));
    }
    Ok(())
}

/// Walks every `.rs` file under `dir` (recursively -- a solution crate may
/// have more than one module) and rewrites it in place via
/// [`crate::stub::strip_to_stub`]: pub signatures survive, bodies become
/// `todo!()`, private items and `#[cfg(test)]` modules are dropped.
fn strip_rust_files_in_place(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(|source| Error::Io {
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            strip_rust_files_in_place(&path)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            let source_text = std::fs::read_to_string(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            let stripped = crate::stub::strip_to_stub(&source_text)?;
            std::fs::write(&path, stripped).map_err(|source| Error::Io { path, source })?;
        }
    }
    Ok(())
}

/// Runs `cargo fix` directly in `out_dir` (already a full, disposable copy
/// of the solution crate at this point -- no separate scratch copy needed)
/// to prune whichever `use` lines `strip_rust_files_in_place` left unused.
/// The real compiler is the authority on what's unused, not hand-rolled
/// name resolution. Cleans up the `target/` directory `cargo fix` leaves
/// behind afterward, so the shipped starter tree carries no build output.
fn run_cargo_fix(out_dir: &Path) -> Result<()> {
    let output = std::process::Command::new("cargo")
        .args(["fix", "--allow-dirty", "--allow-staged", "--allow-no-vcs"])
        .current_dir(out_dir)
        .output()
        .map_err(|source| Error::Io {
            path: out_dir.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "cargo fix failed while stripping the starter at {}:\n{}",
            out_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let target_dir = out_dir.join("target");
    if target_dir.is_dir() {
        std::fs::remove_dir_all(&target_dir).map_err(|source| Error::Io {
            path: target_dir,
            source,
        })?;
    }

    Ok(())
}

fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    std::fs::copy(src, dst).map_err(|source| Error::Io {
        path: src.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Copies the instructor's `harness/` into the public starter, then
/// rewrites the two files that carry private-only content in place: the
/// manifest's `[patch]` table (points at `../solution`, meaningless once
/// copied out) and every `tests/*.rs` file (dropped down to just the
/// public-visibility tests named in `public_test_names`, via
/// [`crate::publish::keep_only_named_tests`]).
fn copy_public_harness(src: &Path, dst: &Path, public_test_names: &HashSet<String>) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    copy_dir_if_exists(src, dst)?;

    let manifest_path = dst.join("Cargo.toml");
    if manifest_path.is_file() {
        let manifest = std::fs::read_to_string(&manifest_path).map_err(|source| Error::Io {
            path: manifest_path.clone(),
            source,
        })?;
        let stripped = publish::strip_toml_table(&manifest, "patch")?;
        std::fs::write(&manifest_path, stripped).map_err(|source| Error::Io {
            path: manifest_path,
            source,
        })?;
    }

    let tests_dir = dst.join("tests");
    if tests_dir.is_dir() {
        for entry in std::fs::read_dir(&tests_dir).map_err(|source| Error::Io {
            path: tests_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| Error::Io {
                path: tests_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rs") {
                let source_text = std::fs::read_to_string(&path).map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })?;
                let filtered = publish::keep_only_named_tests(&source_text, public_test_names)?;
                std::fs::write(&path, filtered).map_err(|source| Error::Io { path, source })?;
            }
        }
    }

    Ok(())
}

/// Recursively copies `src` onto `dst`, or does nothing if `src` doesn't
/// exist (e.g. an assignment with no `fixtures/`).
fn copy_dir_if_exists(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst).map_err(|source| Error::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    for entry in std::fs::read_dir(src).map_err(|source| Error::Io {
        path: src.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::Io {
            path: src.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_if_exists(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            copy_file(&entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const PRIVATE_SPEC: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "linked-library"
deadline = "2026-02-14T23:59:59-08:00"


[toolchain]
channel = "1.86.0"

[allowed-crates]
serde = "1"
rand  = "0.8"

[limits.build]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256

[limits.run]
cpu-time = "5s"
wall-clock = "10s"
cpus = 1
memory = "512MiB"
pids = 128
max-output-bytes = "1MiB"

[scoring]
model = "weighted"

[[scoring.tests]]
name = "insert_basic"
points = 10
visibility = "public"

[[scoring.tests]]
name = "balance_adversarial"
points = 20
visibility = "private"
"#;

    const HARNESS_MANIFEST: &str = "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nhw3 = \"*\"\n\n[patch.crates-io]\nhw3 = { path = \"../hw3\" }\n";

    const JUDGE_RS: &str = r#"
        #[test]
        fn insert_basic() {
            assert!(true);
        }

        #[test]
        fn balance_adversarial() {
            assert!(true);
        }
    "#;

    fn write_solution_crate(solution_dir: &Path, package_name: &str) {
        write(
            &solution_dir.join("Cargo.toml"),
            &format!("[package]\nname = \"{package_name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n"),
        );
        write(
            &solution_dir.join("src/lib.rs"),
            r#"
                use std::collections::HashSet;

                pub struct Stack<T> {
                    items: Vec<T>,
                }

                impl<T> Stack<T> {
                    pub fn new() -> Self {
                        Stack { items: Vec::new() }
                    }

                    pub fn push(&mut self, value: T) {
                        self.items.push(value);
                    }

                    fn dedup_hint(&self) -> HashSet<usize> {
                        HashSet::new()
                    }
                }
            "#,
        );
    }

    /// Writes a full instructor package, solution included (named after
    /// `PRIVATE_SPEC`'s `id = "hw3"`) -- a solution is required for
    /// `scaffold` to produce anything, so every test that isn't
    /// specifically about a missing/mismatched solution needs one.
    fn write_instructor_package(package_dir: &Path) {
        write(&package_dir.join(spec::PRIVATE_SPEC_FILE), PRIVATE_SPEC);
        write(&package_dir.join("harness/Cargo.toml"), HARNESS_MANIFEST);
        write(&package_dir.join("harness/tests/judge.rs"), JUDGE_RS);
        write_solution_crate(&package_dir.join("hw3"), "hw3");
    }

    #[test]
    fn scaffold_produces_the_documented_starter_tree() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());

        let out_dir = tempfile::tempdir().unwrap();
        let outcome = scaffold(package_dir.path(), out_dir.path()).unwrap();

        assert!(outcome
            .out_dir
            .join(".autograder/public")
            .join(spec::PUBLIC_SPEC_FILE)
            .is_file());
        assert!(outcome
            .out_dir
            .join(".autograder/public/harness/tests/judge.rs")
            .is_file());
        assert!(outcome
            .out_dir
            .join(".github/workflows/autograde.yml")
            .is_file());
        assert!(outcome.out_dir.join("Cargo.toml").is_file());
        assert!(outcome.out_dir.join("src/lib.rs").is_file());
    }

    #[test]
    fn scaffold_derives_a_public_spec_with_no_points_or_hidden_tests() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        scaffold(package_dir.path(), out_dir.path()).unwrap();

        let public_spec = std::fs::read_to_string(
            out_dir
                .path()
                .join(".autograder/public")
                .join(spec::PUBLIC_SPEC_FILE),
        )
        .unwrap();
        assert!(!public_spec.contains("points"));
        assert!(!public_spec.contains("balance_adversarial"));
        assert!(public_spec.contains("insert_basic"));
    }

    #[test]
    fn scaffold_derives_a_public_harness_with_only_the_public_test_and_no_patch() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        scaffold(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(
            out_dir
                .path()
                .join(".autograder/public/harness/tests/judge.rs"),
        )
        .unwrap();
        assert!(judge.contains("fn insert_basic"));
        assert!(!judge.contains("balance_adversarial"));

        let manifest = std::fs::read_to_string(
            out_dir.path().join(".autograder/public/harness/Cargo.toml"),
        )
        .unwrap();
        assert!(!manifest.contains("patch"));
        assert!(!manifest.contains("solution"));
    }

    #[test]
    fn scaffold_errors_clearly_when_the_solution_directory_is_missing() {
        let package_dir = tempfile::tempdir().unwrap();
        write(&package_dir.path().join(spec::PRIVATE_SPEC_FILE), PRIVATE_SPEC);
        write(&package_dir.path().join("harness/Cargo.toml"), HARNESS_MANIFEST);
        write(&package_dir.path().join("harness/tests/judge.rs"), JUDGE_RS);
        // deliberately no `hw3/` solution directory
        let out_dir = tempfile::tempdir().unwrap();

        let err = scaffold(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn emitted_workflow_matches_the_design_reference_wrapper() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        scaffold(package_dir.path(), out_dir.path()).unwrap();

        let workflow =
            std::fs::read_to_string(out_dir.path().join(".github/workflows/autograde.yml"))
                .unwrap();
        assert!(workflow.contains("on:\n  push:\n    branches: [main]"));
        assert!(workflow.contains("sha256sum -c -"));
        assert!(workflow.contains("./autograder ci --harness .autograder/public"));
    }

    #[test]
    fn emitted_manifest_matches_the_solutions_own_cargo_toml() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        scaffold(package_dir.path(), out_dir.path()).unwrap();

        let solution_manifest =
            std::fs::read_to_string(package_dir.path().join("hw3/Cargo.toml")).unwrap();
        let starter_manifest = std::fs::read_to_string(out_dir.path().join("Cargo.toml")).unwrap();
        assert_eq!(starter_manifest, solution_manifest);
    }

    #[test]
    fn scaffold_rejects_a_package_dir_without_a_private_spec() {
        let package_dir = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();

        let err = scaffold(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    /// End-to-end: given the real solution crate `write_instructor_package`
    /// puts at `package_dir/<id>`, the emitted starter builds (implying
    /// `cargo fix` left it in a compiling state), exposes the pub API,
    /// stubs bodies to `todo!()`, and never leaks the private helper or its
    /// now-unused import.
    #[test]
    fn scaffold_derives_a_building_stub_from_the_id_named_solution_dir() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());

        let out_dir = tempfile::tempdir().unwrap();
        scaffold(package_dir.path(), out_dir.path()).unwrap();

        let src = std::fs::read_to_string(out_dir.path().join("src/lib.rs")).unwrap();
        assert!(src.contains("pub struct Stack"));
        assert!(src.contains("pub fn new"));
        assert!(src.contains("pub fn push"));
        assert!(src.contains("todo!"));
        assert!(!src.contains("dedup_hint"));
        assert!(!src.contains("HashSet"));

        let build = std::process::Command::new("cargo")
            .arg("build")
            .current_dir(out_dir.path())
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "scaffolded starter failed to build: {}",
            String::from_utf8_lossy(&build.stderr)
        );
    }

    #[test]
    fn scaffold_rejects_a_solution_dir_whose_package_name_does_not_match_the_id() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        write_solution_crate(&package_dir.path().join("hw3"), "wrong-name");

        let out_dir = tempfile::tempdir().unwrap();
        let err = scaffold(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }
}
