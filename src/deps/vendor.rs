use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::spec::Spec;

#[derive(Debug, Clone)]
pub struct VendorOutcome {
    pub vendor_dir: PathBuf,
    pub cargo_config_path: PathBuf,
}

/// `vendor_dir/<VENDOR_CONFIG_FILE>`: the exact `.cargo/config.toml`
/// snippet `cargo vendor` printed on stdout for this workspace, persisted
/// verbatim -- crates-io *and* any git-dependency `[source."git+..."]`
/// block alike. `prepare` and `Nextest` both read this file rather than
/// re-deriving a config of their own, so a git dependency's source
/// override is never silently dropped (a hand-rolled crates-io-only config
/// used to do exactly that).
pub const VENDOR_CONFIG_FILE: &str = "config.toml";

/// `vendor_dir/<LOCK_MARKER_FILE>`: the sha256 of the `Cargo.lock` this
/// vendor dir was built from, so `verify` can tell "vendored, and still
/// fresh" from "vendored against a lock that's since changed" without
/// re-running `cargo vendor`.
const LOCK_MARKER_FILE: &str = ".lock-sha256";

pub fn absolute_vendor_dir(package_dir: &Path) -> PathBuf {
    absolutize(&package_dir.join("vendor"))
}

/// Any path handed to Cargo through `--config`/a config file's
/// `directory`/`path` value should go through this first -- Cargo resolves
/// a relative one relative to the config file's own directory, not the
/// process's cwd, so a relative CLI argument can silently resolve to the
/// wrong place.
pub fn absolutize(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Runs `cargo vendor --locked` directly against the workspace root at
/// `package_dir` -- both `{id}` and `{harness}` at once, sourced straight
/// from the checked-in `Cargo.lock` -- producing `<package_dir>/vendor/`
/// (with its own `config.toml` and `.lock-sha256` marker, see
/// `VENDOR_CONFIG_FILE`/`LOCK_MARKER_FILE`) plus `<package_dir>/.cargo/config.toml`
/// for a plain local `cargo test`/`cargo build` run from `package_dir`.
/// Trusted, online, one-time per assignment -- never runs on student code.
/// Refuses up front if `Cargo.lock` doesn't match the blessed hash
/// `autograder lock` recorded (`crate::deps::lock::verify`), so vendoring
/// can never silently pull a different dependency graph than the one
/// grading is meant to check submissions against.
pub fn vendor(package_dir: &Path, spec: &Spec) -> Result<VendorOutcome> {
    if let Some(message) = crate::deps::lock::verify(package_dir, spec) {
        return Err(Error::InvalidSpec(message));
    }

    let vendor_dir = absolute_vendor_dir(package_dir);
    let manifest_path = package_dir.join("Cargo.toml");

    let output = Command::new("cargo")
        .arg("vendor")
        .arg(&vendor_dir)
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--locked")
        .output()
        .map_err(|source| Error::Other(format!("failed to run cargo vendor: {source}")))?;

    if !output.status.success() {
        return Err(Error::Other(format!(
            "cargo vendor failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    // A workspace with zero total dependencies is a legitimate spec, and
    // `cargo vendor` exits successfully for one -- it just never creates
    // the directory (or prints any config), since there's nothing to
    // vendor or redirect.
    crate::exec::fs::create_dir_all(&vendor_dir)?;

    // `cargo vendor`'s own stdout is the one place that already knows
    // about every source it redirected -- crates-io *and* each git
    // dependency's own repo -- since `vendor_dir` was passed as an
    // absolute path above, the `directory = "..."` line it prints is
    // already absolute too.
    let vendor_config = String::from_utf8_lossy(&output.stdout).into_owned();
    crate::exec::fs::write(&vendor_dir.join(VENDOR_CONFIG_FILE), &vendor_config)?;

    let lock_contents = crate::exec::fs::read_to_string(&package_dir.join("Cargo.lock"))?;
    crate::exec::fs::write(
        &vendor_dir.join(LOCK_MARKER_FILE),
        crate::deps::cargo_lock::sha256_hex(&lock_contents),
    )?;

    let cargo_dir = package_dir.join(".cargo");
    crate::exec::fs::create_dir_all(&cargo_dir)?;
    let cargo_config_path = cargo_dir.join("config.toml");
    crate::exec::fs::write(&cargo_config_path, &vendor_config)?;

    Ok(VendorOutcome {
        vendor_dir,
        cargo_config_path,
    })
}

/// Verifies `package_dir/vendor` is present and was built from the
/// `Cargo.lock` currently checked in at `package_dir` (its sha256,
/// recorded at `vendor` time in `LOCK_MARKER_FILE`) before `evaluate`
/// trusts an `--offline` build against it. `None` means it's safe to
/// proceed; `Some(message)` is a human-readable explanation meant for a
/// hard, batch-wide failure raised before any submission is evaluated --
/// unlike `crate::deps::lock::verify`, there's no per-submission
/// diagnostic to fall back to here, since an unvendored/stale dependency
/// set isn't something any individual submission did wrong.
pub fn verify(package_dir: &Path, spec: &Spec) -> Option<String> {
    let vendor_dir = package_dir.join("vendor");
    if !vendor_dir.is_dir() {
        return Some(format!(
            "{} is missing -- this assignment has never been vendored; run `autograder vendor \
             {}` before evaluating submissions",
            vendor_dir.display(),
            package_dir.display()
        ));
    }
    let marker_path = vendor_dir.join(LOCK_MARKER_FILE);
    let Ok(found) = crate::exec::fs::read_to_string(&marker_path) else {
        return Some(format!(
            "{} is missing -- {} predates vendor freshness tracking, or was assembled by hand \
             rather than `autograder vendor`; re-run `autograder vendor {}`",
            marker_path.display(),
            vendor_dir.display(),
            package_dir.display()
        ));
    };
    if found.trim() == spec.assignment.cargo_lock_sha256 {
        None
    } else {
        Some(format!(
            "{} was vendored from a different Cargo.lock than the one currently checked in \
             (expected sha256 {}, found {}) -- re-run `autograder vendor {}`",
            vendor_dir.display(),
            spec.assignment.cargo_lock_sha256,
            found.trim(),
            package_dir.display()
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

    /// A minimal, real, dependency-free workspace (`harness` + `hw3`), with
    /// a genuine `cargo update`-produced `Cargo.lock` whose hash is baked
    /// into the returned `Spec` -- exactly what `lock::lock` would leave
    /// behind, built without a network round-trip since there's nothing to
    /// resolve.
    fn empty_workspace(package_dir: &Path) -> Spec {
        write(
            &package_dir.join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\nmembers = [\"harness\", \"hw3\"]\n",
        );
        write(
            &package_dir.join("hw3/Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&package_dir.join("hw3/src/lib.rs"), "");
        write(
            &package_dir.join("harness/Cargo.toml"),
            "[package]\nname = \"harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nhw3 = { path = \"../hw3\" }\n",
        );
        write(&package_dir.join("harness/src/main.rs"), "fn main() {}\n");

        let toml = format!(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{}"

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
"#,
            "0".repeat(64),
        );
        write(&package_dir.join("autograder.toml"), &toml);

        let outcome = crate::deps::lock::lock(package_dir).unwrap();
        let spec_toml = std::fs::read_to_string(package_dir.join("autograder.toml")).unwrap();
        let spec: Spec = toml::from_str(&spec_toml).unwrap();
        assert_eq!(spec.assignment.cargo_lock_sha256, outcome.sha256);
        spec
    }

    /// Regression test: a relative `package_dir` must not produce a
    /// relative `vendor_dir` (see `absolutize`'s doc comment).
    #[test]
    fn absolute_vendor_dir_absolutizes_a_relative_package_dir() {
        let relative = Path::new("examples/hw3/instructor");
        let absolutized = absolute_vendor_dir(relative);

        assert!(absolutized.is_absolute());
        assert_eq!(
            absolutized,
            std::env::current_dir()
                .unwrap()
                .join(relative)
                .join("vendor")
        );
    }

    #[test]
    fn absolute_vendor_dir_leaves_an_already_absolute_package_dir_alone() {
        assert_eq!(
            absolute_vendor_dir(Path::new("/pkg")),
            PathBuf::from("/pkg/vendor")
        );
    }

    #[test]
    fn vendor_with_no_workspace_dependencies_produces_an_empty_vendor_dir_and_config() {
        let package = tempfile::tempdir().unwrap();
        let spec = empty_workspace(package.path());

        let outcome = vendor(package.path(), &spec).unwrap();

        // Nothing to redirect, so `cargo vendor` prints no config -- but
        // the directory, the (empty) config file, and the lock marker are
        // still all in place, and `verify` is satisfied by them.
        assert!(outcome.vendor_dir.is_dir());
        assert!(outcome.cargo_config_path.is_file());
        assert!(outcome.vendor_dir.join(VENDOR_CONFIG_FILE).is_file());
        assert!(verify(package.path(), &spec).is_none());
    }

    #[test]
    fn vendor_refuses_to_run_against_a_mismatched_lockfile() {
        let package = tempfile::tempdir().unwrap();
        let mut spec = empty_workspace(package.path());
        spec.assignment.cargo_lock_sha256 = "not-the-real-hash".repeat(4);

        let err = vendor(package.path(), &spec).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn verify_reports_a_missing_vendor_dir() {
        let package = tempfile::tempdir().unwrap();
        let spec = empty_workspace(package.path());

        let message = verify(package.path(), &spec).unwrap();
        assert!(message.contains("never been vendored"));
    }

    #[test]
    fn verify_reports_a_vendor_dir_built_from_a_different_lock() {
        let package = tempfile::tempdir().unwrap();
        let spec = empty_workspace(package.path());
        vendor(package.path(), &spec).unwrap();

        let mut stale_spec = spec;
        stale_spec.assignment.cargo_lock_sha256 = "0".repeat(64);

        let message = verify(package.path(), &stale_spec).unwrap();
        assert!(message.contains("different Cargo.lock"));
    }

    #[test]
    fn verify_reports_a_vendor_dir_missing_its_lock_marker() {
        let package = tempfile::tempdir().unwrap();
        let spec = empty_workspace(package.path());
        std::fs::create_dir_all(package.path().join("vendor")).unwrap();

        let message = verify(package.path(), &spec).unwrap();
        assert!(message.contains("predates vendor freshness tracking"));
    }
}
