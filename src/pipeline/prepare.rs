use std::path::Path;

use crate::deps::cargo_lock::CargoLock;
use crate::deps::lock;
use crate::deps::vendor;
use crate::error::Result;
use crate::pipeline::manifest_check::{self, ManifestDiagnostic};
use crate::spec::Spec;

/// The offline cargo environment installed into the workspace so the build
/// stage can only resolve vendored crates. `vendor_dir` is `None` when the
/// assignment hasn't been prefetched yet (or has an empty allowlist).
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

/// Installs the offline cargo env (if prefetched) and diagnoses the
/// student's `Cargo.toml` against the allowlist. Has no involvement in
/// wiring the judge/harness to `workspace` -- the caller has already
/// positioned that correctly by the time this runs (see
/// `evaluator::library`'s and `evaluator::binary`'s module doc comments),
/// so this function doesn't need to know the tier or assignment kind.
///
/// Checks `package_dir/Cargo.lock` against the blessed hash first
/// (`crate::deps::lock::verify`) -- a mismatch (a student's edited/deleted lock
/// for `ci`, a stale one for `grade`) short-circuits straight to a
/// diagnostic, since the allowlist itself is derived from that same lock
/// and can't be trusted otherwise.
pub fn prepare(workspace: &Path, package_dir: &Path, spec: &Spec) -> Result<PrepareOutcome> {
    let offline_env = install_offline_env(workspace, package_dir)?;

    let manifest_diagnostics = match lock::verify(package_dir, spec) {
        Some(message) => vec![ManifestDiagnostic::LockfileMismatch(message)],
        None => diagnose_manifest(
            workspace,
            package_dir,
            spec,
            offline_env.vendor_dir.as_deref(),
        )?,
    };

    Ok(PrepareOutcome {
        offline_env,
        manifest_diagnostics,
    })
}

/// Writes `workspace/.cargo/config.toml` to replace the crates.io source
/// with the assignment's vendored crates, and returns the
/// `CARGO_NET_OFFLINE` env var the sandbox spec must set. A no-op when the
/// package hasn't been prefetched. `library`'s build doesn't discover this
/// file (it always runs with `workdir` at the shared `repo_root`, never a
/// descendant of `workspace` -- `Library` passes the equivalent `--config`
/// override directly instead),
/// but `diagnose_manifest` below still reads it, and `binary` still uses
/// it as-is since it builds directly in `workspace`.
fn install_offline_env(workspace: &Path, package_dir: &Path) -> Result<OfflineEnv> {
    let vendor_dir = package_dir.join("vendor");
    if !vendor_dir.is_dir() {
        return Ok(OfflineEnv::default());
    }

    let cargo_dir = workspace.join(".cargo");
    crate::exec::fs::create_dir_all(&cargo_dir)?;
    let config_path = cargo_dir.join("config.toml");
    crate::exec::fs::write(
        &config_path,
        vendor::vendor_config_toml(&vendor::absolute_vendor_dir(package_dir)),
    )?;

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
/// opaque offline-resolution failure at build time. Absent manifest -> no
/// diagnostics (the build stage fails on its own with a clear error).
fn diagnose_manifest(
    workspace: &Path,
    package_dir: &Path,
    spec: &Spec,
    vendor_dir: Option<&Path>,
) -> Result<Vec<ManifestDiagnostic>> {
    let manifest_path = workspace.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(Vec::new());
    }
    let contents = crate::exec::fs::read_to_string(&manifest_path)?;

    let lock_contents = crate::exec::fs::read_to_string(&package_dir.join("Cargo.lock"))?;
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
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{cargo_lock_sha256}"


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
"#
        )
    }

    /// Writes `package_dir/Cargo.lock` recording `hw3`'s given direct
    /// dependencies, and returns a `Spec` whose `cargo-lock-sha256` matches
    /// it -- exactly what `crate::deps::lock::lock` would have left behind, but
    /// hand-crafted so these tests don't need a real `cargo update`.
    fn spec_with_lock(package_dir: &Path, deps: &[(&str, &str)]) -> Spec {
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
        write(&package_dir.join("Cargo.lock"), &lock);

        toml::from_str(&spec_toml(&sha256_hex(&lock))).unwrap()
    }

    #[test]
    fn prepare_installs_offline_env_when_the_package_has_been_prefetched() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        write(&workspace.path().join("src/lib.rs"), "// student code");
        write(
            &workspace.path().join("Cargo.toml"),
            "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
        );
        write(
            &package.path().join("vendor/serde/Cargo.toml"),
            "[package]\nname = \"serde\"\nversion = \"1.4.0\"\n",
        );

        let spec = spec_with_lock(package.path(), &[("serde", "1.4.0")]);
        let outcome = prepare(workspace.path(), package.path(), &spec).unwrap();

        assert!(outcome.manifest_diagnostics.is_empty());
        assert_eq!(
            outcome.offline_env.env.get("CARGO_NET_OFFLINE"),
            Some(&"true".to_string())
        );
        let config = std::fs::read_to_string(workspace.path().join(".cargo/config.toml")).unwrap();
        assert!(config.contains("vendored-sources"));
    }

    #[test]
    fn prepare_surfaces_a_disallowed_dependency_diagnostic() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        write(&workspace.path().join("src/lib.rs"), "// student code");
        write(
            &workspace.path().join("Cargo.toml"),
            "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\ntokio = \"1\"\n",
        );

        let spec = spec_with_lock(package.path(), &[("serde", "1.4.0")]);
        let outcome = prepare(workspace.path(), package.path(), &spec).unwrap();

        assert_eq!(outcome.manifest_diagnostics.len(), 1);
        assert!(
            outcome.manifest_diagnostics[0]
                .to_string()
                .contains("tokio")
        );
    }

    #[test]
    fn prepare_without_a_prefetched_vendor_dir_skips_offline_env() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        write(&workspace.path().join("src/lib.rs"), "// student code");

        let spec = spec_with_lock(package.path(), &[]);
        let outcome = prepare(workspace.path(), package.path(), &spec).unwrap();

        assert!(outcome.offline_env.vendor_dir.is_none());
        assert!(!workspace.path().join(".cargo/config.toml").exists());
    }

    #[test]
    fn prepare_surfaces_a_lockfile_mismatch_diagnostic_instead_of_checking_the_manifest() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        write(&workspace.path().join("src/lib.rs"), "// student code");
        write(
            &workspace.path().join("Cargo.toml"),
            "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n",
        );
        // A Cargo.lock is on disk, but doesn't match the spec's blessed hash.
        write(&package.path().join("Cargo.lock"), "version = 4\n");
        let spec: Spec = toml::from_str(&spec_toml(&"0".repeat(64))).unwrap();

        let outcome = prepare(workspace.path(), package.path(), &spec).unwrap();

        assert_eq!(outcome.manifest_diagnostics.len(), 1);
        assert!(matches!(
            outcome.manifest_diagnostics[0],
            ManifestDiagnostic::LockfileMismatch(_)
        ));
    }

    /// Confirms `prepare` never touches `package_dir/harness` or
    /// `package_dir/<id>/tests`, for either kind.
    #[test]
    fn prepare_never_touches_the_harness_for_either_kind() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        write(&workspace.path().join("src/lib.rs"), "// student code");
        write(
            &package.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\n",
        );
        write(&package.path().join("harness/tests/judge.rs"), "// judge");
        write(&package.path().join("hw3/tests/judge.rs"), "// judge");

        let spec = spec_with_lock(package.path(), &[]);
        for kind in ["library", "binary"] {
            let toml = spec_toml(&spec.assignment.cargo_lock_sha256)
                .replace("kind = \"library\"", &format!("kind = \"{kind}\""));
            let spec: Spec = toml::from_str(&toml).unwrap();
            prepare(workspace.path(), package.path(), &spec).unwrap();
        }

        assert_eq!(
            std::fs::read_to_string(package.path().join("harness/Cargo.toml")).unwrap(),
            "[package]\nname = \"driver\"\n"
        );
        assert!(!workspace.path().join("harness").exists());
        assert!(!workspace.path().join("tests").exists());
    }
}
