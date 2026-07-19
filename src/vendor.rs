use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::spec::Spec;

#[derive(Debug, Clone)]
pub struct VendorOutcome {
    pub vendor_dir: PathBuf,
    pub cargo_config_path: PathBuf,
}

/// The `.cargo/config.toml` that replaces the crates.io source with the
/// vendored directory. Callers must pass an absolute `vendor_dir`: Cargo
/// resolves a relative `[source.X].directory` relative to the config
/// file's own directory, not the process's cwd, so a relative
/// `package_dir` would get prepended a second time, silently pointing at
/// a directory that doesn't exist.
pub fn vendor_config_toml(vendor_dir: &Path) -> String {
    format!(
        "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"{}\"\n",
        vendor_dir.display()
    )
}

/// `package_dir.join("vendor")`, absolutized.
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
/// plus `<package_dir>/.cargo/config.toml`. Trusted, online, one-time per
/// assignment -- never runs on student code. Refuses up front if
/// `Cargo.lock` doesn't match the blessed hash `autograder lock` recorded
/// (`crate::lock::verify`), so vendoring can never silently pull a
/// different dependency graph than the one grading is meant to check
/// submissions against.
pub fn prefetch(package_dir: &Path, spec: &Spec) -> Result<VendorOutcome> {
    if let Some(message) = crate::lock::verify(package_dir, spec) {
        return Err(Error::InvalidSpec(message));
    }

    let vendor_dir = package_dir.join("vendor");
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
    // the directory, since there's nothing to put in it.
    crate::fs::create_dir_all(&vendor_dir)?;

    let cargo_dir = package_dir.join(".cargo");
    crate::fs::create_dir_all(&cargo_dir)?;
    let cargo_config_path = cargo_dir.join("config.toml");
    crate::fs::write(
        &cargo_config_path,
        vendor_config_toml(&absolute_vendor_dir(package_dir)),
    )?;

    Ok(VendorOutcome {
        vendor_dir,
        cargo_config_path,
    })
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
formula = "sum"
base = 0.0
"#,
            "0".repeat(64),
        );
        write(&package_dir.join("autograder.toml"), &toml);

        let outcome = crate::lock::lock(package_dir).unwrap();
        let spec_toml = std::fs::read_to_string(package_dir.join("autograder.toml")).unwrap();
        let spec: Spec = toml::from_str(&spec_toml).unwrap();
        assert_eq!(spec.assignment.cargo_lock_sha256, outcome.sha256);
        spec
    }

    #[test]
    fn vendor_config_points_at_the_vendor_dir() {
        let config = vendor_config_toml(Path::new("/pkg/vendor"));
        assert!(config.contains("replace-with = \"vendored-sources\""));
        assert!(config.contains("directory = \"/pkg/vendor\""));
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
    fn prefetch_with_no_workspace_dependencies_produces_an_empty_vendor_dir_and_config() {
        let package = tempfile::tempdir().unwrap();
        let spec = empty_workspace(package.path());

        let outcome = prefetch(package.path(), &spec).unwrap();

        assert!(outcome.vendor_dir.is_dir());
        assert!(outcome.cargo_config_path.is_file());
        let config = std::fs::read_to_string(&outcome.cargo_config_path).unwrap();
        assert!(config.contains("vendored-sources"));
    }

    #[test]
    fn prefetch_refuses_to_run_against_a_mismatched_lockfile() {
        let package = tempfile::tempdir().unwrap();
        let mut spec = empty_workspace(package.path());
        spec.assignment.cargo_lock_sha256 = "not-the-real-hash".repeat(4);

        let err = prefetch(package.path(), &spec).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }
}
