//! `autograder lock`: (re)resolves the workspace-root `Cargo.lock` shared by
//! `{id}` and `{harness}` from their manifests, and records its SHA-256 as
//! the "blessed" hash in `autograder.toml` -- the one value `prepare`
//! checks a checkout's `Cargo.lock` against before any build runs (both
//! `ci`, where it guards against a student editing or deleting the shipped
//! lock, and `grade`/`publish`, where it guards against the instructor
//! shipping a stale one). See `crate::deps::cargo_lock` for the lockfile parser
//! itself, and `crate::pipeline::manifest_check` for how its resolved graph replaces
//! a hand-typed `[allowed-crates]` table.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::deps::cargo_lock::sha256_hex;
use crate::error::{Error, Result};
use crate::exec;
use crate::spec::{SPEC_FILE, Spec};

#[derive(Debug, Clone)]
pub struct LockOutcome {
    pub lock_path: PathBuf,
    pub sha256: String,
}

/// Runs `cargo update` against the workspace root at `package_dir`
/// (resolving/refreshing `Cargo.lock` for every member, `{id}` and
/// `{harness}` alike, `[dependencies]` and `[dev-dependencies]` together --
/// there's no way to lock just one member in isolation, nor any reason to,
/// since they share one lockfile), then writes the resulting lock's hash
/// into `autograder.toml`'s `cargo-lock-sha256`.
pub fn lock(package_dir: &Path) -> Result<LockOutcome> {
    let manifest_path = package_dir.join("Cargo.toml");
    let output = Command::new("cargo")
        .arg("update")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .output()
        .map_err(|source| Error::Other(format!("failed to run cargo update: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "cargo update failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let lock_path = package_dir.join("Cargo.lock");
    let lock_contents = exec::fs::read_to_string(&lock_path)?;
    let sha256 = sha256_hex(&lock_contents);

    write_sha_into_spec(package_dir, &sha256)?;

    Ok(LockOutcome { lock_path, sha256 })
}

/// Splices `cargo-lock-sha256 = "<sha256>"` into `[assignment]` in
/// `autograder.toml`, in place -- via `toml_edit` rather than a full `Spec`
/// round-trip, so every other line (including the instructor's own
/// comments) survives untouched.
fn write_sha_into_spec(package_dir: &Path, sha256: &str) -> Result<()> {
    let spec_path = package_dir.join(SPEC_FILE);
    let contents = exec::fs::read_to_string(&spec_path)?;
    let mut doc = contents
        .parse::<toml_edit::DocumentMut>()
        .map_err(|source| {
            Error::Other(format!("failed to parse {}: {source}", spec_path.display()))
        })?;
    doc["assignment"]["cargo-lock-sha256"] = toml_edit::value(sha256);
    exec::fs::write(&spec_path, doc.to_string())
}

/// Verifies `package_dir/Cargo.lock` (as actually checked out -- a
/// student's own possibly-edited copy for `ci`, the instructor's for
/// `grade`/`publish`) still matches `spec.assignment.cargo_lock_sha256`,
/// the hash `lock` last recorded. `None` means it matches; `Some(message)`
/// is a human-readable explanation callers can either surface as a
/// diagnostic (`prepare`, non-fatal to the batch) or a hard error
/// (`publish`, refuses to ship).
pub fn verify(package_dir: &Path, spec: &Spec) -> Option<String> {
    let lock_path = package_dir.join("Cargo.lock");
    if !lock_path.is_file() {
        return Some(format!(
            "{} is missing -- it should be checked in alongside autograder.toml; run \
             `autograder lock` and commit the result",
            lock_path.display()
        ));
    }
    let Ok(contents) = exec::fs::read_to_string(&lock_path) else {
        return Some(format!("{} could not be read", lock_path.display()));
    };
    let found = sha256_hex(&contents);
    if found == spec.assignment.cargo_lock_sha256 {
        None
    } else {
        Some(format!(
            "Cargo.lock does not match the blessed lock recorded in autograder.toml \
             (expected sha256 {}, found {found}) -- don't edit or regenerate Cargo.lock by \
             hand; it pins the exact dependency versions grading uses. If you're the \
             instructor and just changed dependencies, run `autograder lock`.",
            spec.assignment.cargo_lock_sha256
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn spec_with_sha(sha256: &str) -> Spec {
        let toml = format!(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{sha256}"

[sandbox]
image = "autograder-base:1.86.0"

[build-limits]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256
max-output-bytes = "1MiB"

[scoring]
formula = "sum"
base = 0.0
"#
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn verify_is_none_when_the_hash_matches() {
        let package_dir = tempfile::tempdir().unwrap();
        write(&package_dir.path().join("Cargo.lock"), "lock contents\n");
        let spec = spec_with_sha(&sha256_hex("lock contents\n"));

        assert!(verify(package_dir.path(), &spec).is_none());
    }

    #[test]
    fn verify_reports_a_mismatched_hash() {
        let package_dir = tempfile::tempdir().unwrap();
        write(
            &package_dir.path().join("Cargo.lock"),
            "edited by a student\n",
        );
        let spec = spec_with_sha(&sha256_hex("original\n"));

        let message = verify(package_dir.path(), &spec).unwrap();
        assert!(message.contains("does not match"));
    }

    #[test]
    fn verify_reports_a_missing_lockfile() {
        let package_dir = tempfile::tempdir().unwrap();
        let spec = spec_with_sha(&sha256_hex("whatever\n"));

        let message = verify(package_dir.path(), &spec).unwrap();
        assert!(message.contains("is missing"));
    }

    #[test]
    fn write_sha_into_spec_preserves_comments_and_other_fields() {
        let package_dir = tempfile::tempdir().unwrap();
        write(
            &package_dir.path().join(SPEC_FILE),
            "# a helpful comment\n[assignment]\nid = \"hw3\"\ncargo-lock-sha256 = \"stale\"\n",
        );

        write_sha_into_spec(package_dir.path(), "fresh-hash").unwrap();

        let updated = std::fs::read_to_string(package_dir.path().join(SPEC_FILE)).unwrap();
        assert!(updated.contains("# a helpful comment"));
        assert!(updated.contains("id = \"hw3\""));
        assert!(updated.contains("cargo-lock-sha256 = \"fresh-hash\""));
        assert!(!updated.contains("stale"));
    }
}
