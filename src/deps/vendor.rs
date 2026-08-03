use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::spec::Spec;

/// `vendor_dir/<VENDOR_CONFIG_FILE>`: the exact `.cargo/config.toml`
/// snippet `cargo vendor` printed on stdout for this workspace, persisted
/// verbatim -- crates-io *and* any git-dependency `[source."git+..."]`
/// block alike. `prepare` and `Nextest` both read this file rather than
/// re-deriving a config of their own, so a git dependency's source
/// override is never silently dropped (a hand-rolled crates-io-only config
/// used to do exactly that).
pub const VENDOR_CONFIG_FILE: &str = "config.toml";

pub fn batch_vendor_dir(submissions_dir: &Path) -> PathBuf {
    absolutize(&submissions_dir.join(".vendor"))
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
/// `assignment_dir` -- both `{id}` and `{harness}` at once, sourced straight
/// from the checked-in `Cargo.lock` -- into `vendor_dir` (with its own
/// `config.toml`, see `VENDOR_CONFIG_FILE`). Trusted, online, run once per
/// `evaluate`/`ci` invocation before any submission is touched -- never on
/// student code. Refuses up front if `Cargo.lock` doesn't match the blessed
/// hash `autograder lock` recorded (`crate::deps::lock::verify`), so
/// vendoring can never silently pull a different dependency graph than the
/// one grading is meant to check submissions against.
pub fn vendor(assignment_dir: &Path, vendor_dir: &Path, spec: &Spec) -> Result<()> {
    if let Some(message) = crate::deps::lock::verify(assignment_dir, spec) {
        return Err(Error::InvalidSpec(message));
    }

    let vendor_dir = absolutize(vendor_dir);
    let manifest_path = assignment_dir.join("Cargo.toml");

    if vendor_dir.exists() {
        crate::exec::fs::remove_dir_all(&vendor_dir)?;
    }

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

    Ok(())
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
    fn empty_workspace(assignment_dir: &Path) -> Spec {
        write(
            &assignment_dir.join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\nmembers = [\"harness\", \"hw3\"]\n",
        );
        write(
            &assignment_dir.join("hw3/Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&assignment_dir.join("hw3/src/lib.rs"), "");
        write(
            &assignment_dir.join("harness/Cargo.toml"),
            "[package]\nname = \"harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nhw3 = { path = \"../hw3\" }\n",
        );
        write(
            &assignment_dir.join("harness/src/main.rs"),
            "fn main() {}\n",
        );

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
        write(&assignment_dir.join("autograder.toml"), &toml);

        let outcome = crate::deps::lock::lock(assignment_dir).unwrap();
        let spec_toml = std::fs::read_to_string(assignment_dir.join("autograder.toml")).unwrap();
        let spec: Spec = toml::from_str(&spec_toml).unwrap();
        assert_eq!(spec.assignment.cargo_lock_sha256, outcome.sha256);
        spec
    }

    /// Regression test: a relative `submissions_dir` must not produce a
    /// relative `vendor_dir` (see `absolutize`'s doc comment).
    #[test]
    fn batch_vendor_dir_absolutizes_a_relative_submissions_dir() {
        let relative = Path::new("examples/hw3/submissions");
        let absolutized = batch_vendor_dir(relative);

        assert!(absolutized.is_absolute());
        assert_eq!(
            absolutized,
            std::env::current_dir()
                .unwrap()
                .join(relative)
                .join(".vendor")
        );
    }

    #[test]
    fn batch_vendor_dir_leaves_an_already_absolute_submissions_dir_alone() {
        assert_eq!(
            batch_vendor_dir(Path::new("/subs")),
            PathBuf::from("/subs/.vendor")
        );
    }

    #[test]
    fn vendor_with_no_workspace_dependencies_produces_an_empty_vendor_dir_and_config() {
        let package = tempfile::tempdir().unwrap();
        let spec = empty_workspace(package.path());
        let vendor_dir = package.path().join(".vendor");

        vendor(package.path(), &vendor_dir, &spec).unwrap();

        // Nothing to redirect, so `cargo vendor` prints no config -- but
        // the directory and the (empty) config file are still both in place.
        assert!(vendor_dir.is_dir());
        assert!(vendor_dir.join(VENDOR_CONFIG_FILE).is_file());
    }

    /// A re-vendor must not leave the previous run's crates behind: a
    /// version resolved from a since-changed `Cargo.lock` is exactly what
    /// `manifest_check` would read back as "the vendored version".
    #[test]
    fn vendor_clears_a_previous_runs_vendor_dir() {
        let package = tempfile::tempdir().unwrap();
        let spec = empty_workspace(package.path());
        let vendor_dir = package.path().join(".vendor");
        write(&vendor_dir.join("serde/Cargo.toml"), "stale");

        vendor(package.path(), &vendor_dir, &spec).unwrap();

        assert!(!vendor_dir.join("serde").exists());
    }

    #[test]
    fn vendor_refuses_to_run_against_a_mismatched_lockfile() {
        let package = tempfile::tempdir().unwrap();
        let mut spec = empty_workspace(package.path());
        spec.assignment.cargo_lock_sha256 = "not-the-real-hash".repeat(4);

        let err = vendor(package.path(), &package.path().join(".vendor"), &spec).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }
}
