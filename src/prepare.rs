use std::path::Path;

use crate::error::Result;
use crate::manifest_check::{self, ManifestDiagnostic};
use crate::spec::Spec;
use crate::vendor;

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
pub fn prepare(workspace: &Path, package_dir: &Path, spec: &Spec) -> Result<PrepareOutcome> {
    let offline_env = install_offline_env(workspace, package_dir)?;
    let manifest_diagnostics =
        diagnose_manifest(workspace, spec, offline_env.vendor_dir.as_deref())?;

    Ok(PrepareOutcome {
        offline_env,
        manifest_diagnostics,
    })
}

/// Writes `workspace/.cargo/config.toml` to replace the crates.io source
/// with the assignment's vendored crates, and returns the
/// `CARGO_NET_OFFLINE` env var the sandbox spec must set. A no-op when the
/// package hasn't been prefetched. `library`'s build doesn't discover this
/// file (its `driver_dir` is a sibling of `workspace`, not a descendant --
/// `Library` passes the equivalent `--config` override directly instead),
/// but `diagnose_manifest` below still reads it, and `binary` still uses
/// it as-is since it builds directly in `workspace`.
fn install_offline_env(workspace: &Path, package_dir: &Path) -> Result<OfflineEnv> {
    let vendor_dir = package_dir.join("vendor");
    if !vendor_dir.is_dir() {
        return Ok(OfflineEnv::default());
    }

    let cargo_dir = workspace.join(".cargo");
    crate::fs::create_dir_all(&cargo_dir)?;
    let config_path = cargo_dir.join("config.toml");
    crate::fs::write(
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

/// Diffs the student's `Cargo.toml` against `[allowed-crates]`, so a
/// disallowed dependency produces a precise diagnostic instead of an
/// opaque offline-resolution failure at build time. Absent manifest -> no
/// diagnostics (the build stage fails on its own with a clear error).
fn diagnose_manifest(
    workspace: &Path,
    spec: &Spec,
    vendor_dir: Option<&Path>,
) -> Result<Vec<ManifestDiagnostic>> {
    let manifest_path = workspace.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(Vec::new());
    }
    let contents = crate::fs::read_to_string(&manifest_path)?;
    manifest_check::check_manifest(&contents, &spec.allowed_crates, vendor_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const SPEC_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00"


[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]
serde = "1"

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
"#;

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

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
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

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
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

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let outcome = prepare(workspace.path(), package.path(), &spec).unwrap();

        assert!(outcome.offline_env.vendor_dir.is_none());
        assert!(!workspace.path().join(".cargo/config.toml").exists());
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

        for kind in ["library", "binary"] {
            let toml = SPEC_TOML.replace("kind = \"library\"", &format!("kind = \"{kind}\""));
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
