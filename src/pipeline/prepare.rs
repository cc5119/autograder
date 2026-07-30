use std::path::Path;

use crate::deps::cargo_lock::CargoLock;
use crate::deps::lock;
use crate::deps::vendor;
use crate::error::Result;
use crate::model::JobContext;
use crate::pipeline::manifest_check::{self, ManifestDiagnostic};
use crate::spec::Spec;

/// The offline cargo environment installed into the workspace so the build
/// stage can only resolve vendored crates. `vendor_dir` is `None` when the
/// assignment hasn't been vendored yet (or has an empty allowlist).
#[derive(Debug, Clone, Default)]
pub struct OfflineEnv {
    pub vendor_dir: Option<std::path::PathBuf>,
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct PrepareOutcome {
    pub offline_env: OfflineEnv,
    /// Allowlist/`[patch]`/git/path violations found in the student
    /// `Cargo.toml`, in the order found. Non-empty means the batch should
    /// record `DisallowedDependency` and skip Evaluate -- wired in the
    /// pipeline rather than here, since Prepare stays a pure
    /// "assemble + diagnose" stage.
    pub manifest_diagnostics: Vec<ManifestDiagnostic>,
}

/// Installs the offline cargo env (if vendored) and diagnoses the
/// student's `Cargo.toml` against the allowlist.
///
/// Checks `assignment_dir/Cargo.lock` against the blessed hash first
/// (`crate::deps::lock::verify`) -- a mismatch (a student's edited/deleted lock
/// for `ci`, a stale one for `grade`) short-circuits straight to a
/// diagnostic, since the allowlist itself is derived from that same lock
/// and can't be trusted otherwise.
pub fn prepare(ctx: &JobContext, assignment_dir: &Path, spec: &Spec) -> Result<PrepareOutcome> {
    let submission_dir = ctx.submission_package_dir();
    let offline_env = install_offline_env(&submission_dir, assignment_dir)?;

    let manifest_diagnostics = match lock::verify(assignment_dir, spec) {
        Some(message) => vec![ManifestDiagnostic::LockfileMismatch(message)],
        None => diagnose_manifest(
            &submission_dir,
            assignment_dir,
            spec,
            offline_env.vendor_dir.as_deref(),
        )?,
    };

    Ok(PrepareOutcome {
        offline_env,
        manifest_diagnostics,
    })
}

/// Writes `submission_dir/.cargo/config.toml`, copied verbatim from
/// `vendor_dir`'s own `config.toml` (see `vendor::VENDOR_CONFIG_FILE`). A
/// no-op when the package hasn't been vendored. `Nextest` doesn't discover
/// this file -- its builds run with `workdir` at `repo_root`, not a
/// descendant of it, so it writes the same config at `repo_root` directly
/// instead -- but `diagnose_manifest` below still reads it, and it's what a
/// student's own local `cargo test`/`ci` run picks up.
fn install_offline_env(submission_dir: &Path, assignment_dir: &Path) -> Result<OfflineEnv> {
    let vendor_dir = assignment_dir.join("vendor");
    if !vendor_dir.is_dir() {
        return Ok(OfflineEnv::default());
    }

    let vendor_config =
        crate::exec::fs::read_to_string(&vendor_dir.join(vendor::VENDOR_CONFIG_FILE))
            .unwrap_or_default();
    let cargo_dir = submission_dir.join(".cargo");
    crate::exec::fs::create_dir_all(&cargo_dir)?;
    let config_path = cargo_dir.join("config.toml");
    crate::exec::fs::write(&config_path, &vendor_config)?;

    let mut env = std::collections::BTreeMap::new();
    env.insert("CARGO_NET_OFFLINE".to_string(), "true".to_string());

    Ok(OfflineEnv {
        vendor_dir: Some(vendor_dir),
        env,
    })
}

/// Diffs the student's `Cargo.toml` against `{id}`'s own direct
/// dependencies as resolved in the blessed `Cargo.lock` (the real
/// allowlist -- see `manifest_check`'s module doc comment), so a
/// disallowed dependency produces a precise diagnostic instead of an
/// opaque offline-resolution failure at build time.
fn diagnose_manifest(
    submission_dir: &Path,
    assignment_dir: &Path,
    spec: &Spec,
    vendor_dir: Option<&Path>,
) -> Result<Vec<ManifestDiagnostic>> {
    let manifest_path = submission_dir.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(Vec::new());
    }
    let contents = crate::exec::fs::read_to_string(&manifest_path)?;

    let lock_contents = crate::exec::fs::read_to_string(&assignment_dir.join("Cargo.lock"))?;
    let lock = CargoLock::parse(&lock_contents)?;
    let allowed_crates = lock.direct_dependencies(spec.assignment.id.as_str());

    manifest_check::check_manifest(&contents, &allowed_crates, vendor_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::cargo_lock::sha256_hex;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn spec_toml(cargo_lock_sha256: &str) -> String {
        format!(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{cargo_lock_sha256}"


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
        )
    }

    /// Writes `assignment_dir/Cargo.lock` recording `hw3`'s given direct
    /// dependencies, and returns a `Spec` whose `cargo-lock-sha256` matches
    /// it -- exactly what `crate::deps::lock::lock` would have left behind, but
    /// hand-crafted so these tests don't need a real `cargo update`.
    fn spec_with_lock(assignment_dir: &Path, deps: &[(&str, &str)]) -> Spec {
        let mut lock = String::from(
            "version = 4\n\n[[package]]\nname = \"hw3\"\nversion = \"0.1.0\"\ndependencies = [\n",
        );
        for (name, _) in deps {
            lock.push_str(&format!(" \"{name}\",\n"));
        }
        lock.push_str("]\n");
        for (name, version) in deps {
            lock.push_str(&format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n"
            ));
        }
        write(&assignment_dir.join("Cargo.lock"), &lock);

        toml::from_str(&spec_toml(&sha256_hex(&lock))).unwrap()
    }

    /// A `JobContext` whose `workspace` is `repo_root` -- matching `hw3`,
    /// `spec_toml`'s hardcoded `[assignment].id`, so `ctx.submission_package_dir()`
    /// resolves to `repo_root/hw3`.
    fn ctx(repo_root: &Path) -> JobContext {
        JobContext {
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            workspace: repo_root.to_path_buf(),
        }
    }

    #[test]
    fn prepare_installs_offline_env_when_the_package_has_been_vendored() {
        let repo_root = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        let submission_dir = repo_root.path().join("hw3");
        write(&submission_dir.join("src/lib.rs"), "// student code");
        write(
            &submission_dir.join("Cargo.toml"),
            "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
        );
        write(
            &package.path().join("vendor/serde/Cargo.toml"),
            "[package]\nname = \"serde\"\nversion = \"1.4.0\"\n",
        );
        write(
            &package.path().join("vendor/config.toml"),
            "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n\
             [source.vendored-sources]\ndirectory = \"/pkg/vendor\"\n",
        );

        let spec = spec_with_lock(package.path(), &[("serde", "1.4.0")]);
        let outcome = prepare(&ctx(repo_root.path()), package.path(), &spec).unwrap();

        assert!(outcome.manifest_diagnostics.is_empty());
        assert_eq!(
            outcome.offline_env.env.get("CARGO_NET_OFFLINE"),
            Some(&"true".to_string())
        );
        let config = std::fs::read_to_string(submission_dir.join(".cargo/config.toml")).unwrap();
        assert!(config.contains("vendored-sources"));
    }

    #[test]
    fn prepare_surfaces_a_disallowed_dependency_diagnostic() {
        let repo_root = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        let submission_dir = repo_root.path().join("hw3");
        write(&submission_dir.join("src/lib.rs"), "// student code");
        write(
            &submission_dir.join("Cargo.toml"),
            "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\ntokio = \"1\"\n",
        );

        let spec = spec_with_lock(package.path(), &[("serde", "1.4.0")]);
        let outcome = prepare(&ctx(repo_root.path()), package.path(), &spec).unwrap();

        assert_eq!(outcome.manifest_diagnostics.len(), 1);
        assert!(
            outcome.manifest_diagnostics[0]
                .to_string()
                .contains("tokio")
        );
    }

    #[test]
    fn prepare_without_a_vendored_vendor_dir_skips_offline_env() {
        let repo_root = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        let submission_dir = repo_root.path().join("hw3");
        write(&submission_dir.join("src/lib.rs"), "// student code");

        let spec = spec_with_lock(package.path(), &[]);
        let outcome = prepare(&ctx(repo_root.path()), package.path(), &spec).unwrap();

        assert!(outcome.offline_env.vendor_dir.is_none());
        assert!(!submission_dir.join(".cargo/config.toml").exists());
    }

    #[test]
    fn prepare_surfaces_a_lockfile_mismatch_diagnostic_instead_of_checking_the_manifest() {
        let repo_root = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        let submission_dir = repo_root.path().join("hw3");
        write(&submission_dir.join("src/lib.rs"), "// student code");
        write(
            &submission_dir.join("Cargo.toml"),
            "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n",
        );
        // A Cargo.lock is on disk, but doesn't match the spec's blessed hash.
        write(&package.path().join("Cargo.lock"), "version = 4\n");
        let spec: Spec = toml::from_str(&spec_toml(&"0".repeat(64))).unwrap();

        let outcome = prepare(&ctx(repo_root.path()), package.path(), &spec).unwrap();

        assert_eq!(outcome.manifest_diagnostics.len(), 1);
        assert!(matches!(
            outcome.manifest_diagnostics[0],
            ManifestDiagnostic::LockfileMismatch(_)
        ));
    }

    #[test]
    fn prepare_never_touches_the_harness() {
        let repo_root = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        let submission_dir = repo_root.path().join("hw3");
        write(&submission_dir.join("src/lib.rs"), "// student code");
        write(
            &package.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\n",
        );
        write(&package.path().join("harness/tests/judge.rs"), "// judge");
        write(&package.path().join("hw3/tests/judge.rs"), "// judge");

        let spec = spec_with_lock(package.path(), &[]);
        let toml = spec_toml(&spec.assignment.cargo_lock_sha256);
        let spec: Spec = toml::from_str(&toml).unwrap();
        prepare(&ctx(repo_root.path()), package.path(), &spec).unwrap();

        assert_eq!(
            std::fs::read_to_string(package.path().join("harness/Cargo.toml")).unwrap(),
            "[package]\nname = \"driver\"\n"
        );
        assert!(!submission_dir.join("harness").exists());
        assert!(!submission_dir.join("tests").exists());
    }
}
