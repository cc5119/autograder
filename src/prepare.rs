use std::path::Path;

use crate::error::{Error, Result};
use crate::manifest_check::{self, ManifestDiagnostic};
use crate::spec::{AssignmentKind, Spec};
use crate::vendor;

/// How the student's checkout is wired to the harness, decided structurally
/// from `[assignment].kind`. Offline cargo env + manifest allowlist diff are
/// added in M2 (steps 12-13); this only identifies the wiring target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wiring {
    /// `linked-library`: a driver crate scaffold path-depends on the
    /// student's package (by `[student].package-name`).
    LinkedLibrary { driver_dir: std::path::PathBuf },
    /// `binary-harness`: the built binary target the judge will spawn.
    BinaryHarness { bin_name: String },
}

/// The offline cargo environment installed into the workspace so the build
/// stage can only resolve vendored crates (design §8.2). `vendor_dir` is
/// `None` when the assignment package hasn't been prefetched yet (or has an
/// empty allowlist) — `env` is then empty and no `.cargo/config.toml` is
/// written.
#[derive(Debug, Clone, Default)]
pub struct OfflineEnv {
    pub vendor_dir: Option<std::path::PathBuf>,
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct PrepareOutcome {
    pub wiring: Wiring,
    pub offline_env: OfflineEnv,
    /// Allowlist/`[patch]`/git/path violations found in the student
    /// `Cargo.toml`, in the order found (design §8.3, §8.4). Non-empty means
    /// the batch should record `DisallowedDependency` and skip Evaluate —
    /// wired in the pipeline (step 15) rather than here, since Prepare stays
    /// a pure "assemble + diagnose" stage.
    pub manifest_diagnostics: Vec<ManifestDiagnostic>,
}

/// Assembles the workspace: student checkout (already at `workspace`) with
/// the instructor `harness/` and `fixtures/` overlaid on top — instructor
/// files win, and any student file at a matching path is replaced (design
/// §7.2). `package_dir` is the instructor assignment package containing
/// `harness/`, `fixtures/`, and (once prefetched) `vendor/`.
pub fn prepare(workspace: &Path, package_dir: &Path, spec: &Spec) -> Result<PrepareOutcome> {
    let harness_dir = package_dir.join("harness");
    if harness_dir.is_dir() {
        overlay_dir(workspace, &harness_dir)?;
    }
    let fixtures_dir = package_dir.join("fixtures");
    if fixtures_dir.is_dir() {
        overlay_dir(&workspace.join("fixtures"), &fixtures_dir)?;
    }

    let wiring = match spec.assignment.kind {
        AssignmentKind::LinkedLibrary => {
            let driver_dir = workspace.join("driver");
            std::fs::create_dir_all(&driver_dir).map_err(|source| Error::Io {
                path: driver_dir.clone(),
                source,
            })?;
            Wiring::LinkedLibrary { driver_dir }
        }
        AssignmentKind::BinaryHarness => {
            let bin_name = spec.student.bin_name.clone().ok_or_else(|| {
                Error::InvalidSpec("binary-harness assignment missing [student].bin-name".into())
            })?;
            Wiring::BinaryHarness { bin_name }
        }
    };

    let offline_env = install_offline_env(workspace, package_dir)?;
    let manifest_diagnostics = diagnose_manifest(workspace, spec, offline_env.vendor_dir.as_deref())?;

    Ok(PrepareOutcome {
        wiring,
        offline_env,
        manifest_diagnostics,
    })
}

/// Writes `workspace/.cargo/config.toml` to replace the crates.io source
/// with the assignment's vendored crates, and returns the `CARGO_NET_OFFLINE`
/// env var the build/run sandbox spec must set (design §8.2). A no-op when
/// the package hasn't been prefetched (no `vendor/` dir yet).
fn install_offline_env(workspace: &Path, package_dir: &Path) -> Result<OfflineEnv> {
    let vendor_dir = package_dir.join("vendor");
    if !vendor_dir.is_dir() {
        return Ok(OfflineEnv::default());
    }

    let cargo_dir = workspace.join(".cargo");
    std::fs::create_dir_all(&cargo_dir).map_err(|source| Error::Io {
        path: cargo_dir.clone(),
        source,
    })?;
    let config_path = cargo_dir.join("config.toml");
    std::fs::write(
        &config_path,
        vendor::vendor_config_toml(&vendor::absolute_vendor_dir(package_dir)),
    )
    .map_err(|source| Error::Io {
        path: config_path.clone(),
        source,
    })?;

    let mut env = std::collections::BTreeMap::new();
    env.insert("CARGO_NET_OFFLINE".to_string(), "true".to_string());

    Ok(OfflineEnv {
        vendor_dir: Some(vendor_dir),
        env,
    })
}

/// Diffs the student's `Cargo.toml` (now at the workspace root) against
/// `[allowed-crates]`, so a disallowed dependency produces a precise
/// diagnostic (design §8.3) instead of an opaque offline-resolution
/// failure at build time. Absent manifest -> no diagnostics (the build
/// stage will fail on its own with a clear "no Cargo.toml" error).
fn diagnose_manifest(
    workspace: &Path,
    spec: &Spec,
    vendor_dir: Option<&Path>,
) -> Result<Vec<ManifestDiagnostic>> {
    let manifest_path = workspace.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(Vec::new());
    }
    let contents = std::fs::read_to_string(&manifest_path).map_err(|source| Error::Io {
        path: manifest_path.clone(),
        source,
    })?;
    manifest_check::check_manifest(&contents, &spec.allowed_crates, vendor_dir)
}

/// Copies `overlay_root`'s tree onto `dest`, path-for-path. Any existing
/// file at a destination path is removed first, so instructor files always
/// win over student files at the same path.
fn overlay_dir(dest: &Path, overlay_root: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|source| Error::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    let entries = std::fs::read_dir(overlay_root).map_err(|source| Error::Io {
        path: overlay_root.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: overlay_root.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            overlay_dir(&dest_path, &entry.path())?;
        } else if file_type.is_file() {
            if dest_path.exists() {
                std::fs::remove_file(&dest_path).map_err(|source| Error::Io {
                    path: dest_path.clone(),
                    source,
                })?;
            }
            std::fs::copy(entry.path(), &dest_path).map_err(|source| Error::Io {
                path: dest_path.clone(),
                source,
            })?;
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

    const SPEC_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "linked-library"
deadline = "2026-02-14T23:59:59-08:00"

[student]
package-name = "bst"

[toolchain]
channel = "1.86.0"

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
        assert!(outcome.manifest_diagnostics[0].to_string().contains("tokio"));
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

    #[test]
    fn instructor_harness_replaces_student_file_at_same_path() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();

        write(&workspace.path().join("tests/foo.rs"), "student version");
        write(&package.path().join("harness/tests/foo.rs"), "instructor version");
        write(&package.path().join("harness/tests/hidden.rs"), "hidden test");

        overlay_dir(workspace.path(), &package.path().join("harness")).unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.path().join("tests/foo.rs")).unwrap(),
            "instructor version"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("tests/hidden.rs")).unwrap(),
            "hidden test"
        );
    }

    #[test]
    fn unrelated_student_files_are_left_alone() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();

        write(&workspace.path().join("src/lib.rs"), "student lib");
        write(&package.path().join("harness/tests/foo.rs"), "instructor version");

        overlay_dir(workspace.path(), &package.path().join("harness")).unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.path().join("src/lib.rs")).unwrap(),
            "student lib"
        );
    }
}
