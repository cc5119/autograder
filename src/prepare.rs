use std::path::Path;

use crate::error::{Error, Result};
use crate::manifest_check::{self, ManifestDiagnostic};
use crate::model::Tier;
use crate::spec::{AssignmentKind, Spec};
use crate::vendor;

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
    /// Where the `library` driver crate was built (`package_dir/harness`
    /// itself, in place, for `Tier::Ci`; the passed-in `driver_dir`, copied
    /// into, for `Tier::Authoritative` -- see `prepare`'s doc comment).
    /// Unused by `binary`, which builds the student's own binary directly
    /// in `workspace` -- the value is still filled in (mirroring the
    /// passed-in `driver_dir` hint) so callers don't need a `kind`-specific
    /// case to construct a `JobContext`.
    pub driver_dir: std::path::PathBuf,
    pub offline_env: OfflineEnv,
    /// Allowlist/`[patch]`/git/path violations found in the student
    /// `Cargo.toml`, in the order found (design §8.3, §8.4). Non-empty means
    /// the batch should record `DisallowedDependency` and skip Evaluate —
    /// wired in the pipeline (step 15) rather than here, since Prepare stays
    /// a pure "assemble + diagnose" stage.
    pub manifest_diagnostics: Vec<ManifestDiagnostic>,
}

/// Assembles the workspace: the student checkout (already at `workspace`)
/// plus the harness. `package_dir` is the assignment package containing
/// `harness/` and (once prefetched) `vendor/` -- the private instructor
/// package for `Tier::Authoritative`, or a checked-out starter repo (see
/// `scaffold`) for `Tier::Ci`.
///
/// For `library`, how `harness/` (the driver crate — no `driver/`
/// subdirectory in the checked-in package, see `evaluator::library`) gets
/// wired to the checkout depends on the tier, because the two tiers differ
/// in exactly the way that matters for this:
///
/// - **`Authoritative`**: `workspace` is an arbitrary, per-job checkout
///   location under the host's storage dir -- not something `harness/`'s
///   own manifest can know ahead of time. So `harness/` is copied into
///   `driver_dir`, a *fresh, per-job* directory that is a **sibling** of
///   `workspace`, not nested inside it (nothing an evaluator builds ever
///   lands inside the student's own checkout), and the evaluator
///   redirects the dependency with a per-invocation `--config` override
///   (`evaluator::library`). Freshness matters even though the two are
///   separate: sharing one `harness/` directory across jobs would race
///   different students' builds against the same `Cargo.lock`/`target/`.
/// - **`Ci`**: `workspace` is wherever the student's own checkout already
///   is, and (per `scaffold`'s starter layout) `harness/` is *already*
///   sitting right there as a sibling of the student's crate, wired via a
///   plain path dependency instead of a patch -- no per-job unpredictability
///   to work around, since there's only ever one checkout `harness/` needs
///   to see: the very repo it's part of. So `driver_dir` is `package_dir/
///   harness` itself, built **in place**, no copy at all. This isn't just
///   simpler: copying it elsewhere would actively break it, since a plain
///   path dependency has no `--config`-patchable registry source to
///   redirect (patches only apply to registry/git-sourced dependencies,
///   never to ones already resolved via `path`) -- so a stale copy would
///   silently resolve `../<id>` against whatever happens to be there
///   instead of the student's actual crate.
pub fn prepare(
    workspace: &Path,
    driver_dir: &Path,
    package_dir: &Path,
    spec: &Spec,
    tier: Tier,
) -> Result<PrepareOutcome> {
    let harness_dir = package_dir.join("harness");
    let resolved_driver_dir = match spec.assignment.kind {
        AssignmentKind::Library => match tier {
            Tier::Ci => harness_dir.clone(),
            Tier::Authoritative => {
                std::fs::create_dir_all(driver_dir).map_err(|source| Error::Io {
                    path: driver_dir.to_path_buf(),
                    source,
                })?;
                if harness_dir.is_dir() {
                    copy_dir_into(driver_dir, &harness_dir)?;
                }
                driver_dir.to_path_buf()
            }
        },
        AssignmentKind::Binary => {
            if harness_dir.is_dir() {
                copy_dir_into(workspace, &harness_dir)?;
            }
            driver_dir.to_path_buf()
        }
    };

    let offline_env = install_offline_env(workspace, package_dir)?;
    let manifest_diagnostics =
        diagnose_manifest(workspace, spec, offline_env.vendor_dir.as_deref())?;

    Ok(PrepareOutcome {
        driver_dir: resolved_driver_dir,
        offline_env,
        manifest_diagnostics,
    })
}

/// Writes `workspace/.cargo/config.toml` to replace the crates.io source
/// with the assignment's vendored crates, and returns the `CARGO_NET_OFFLINE`
/// env var the build/run sandbox spec must set (design §8.2). A no-op when
/// the package hasn't been prefetched (no `vendor/` dir yet).
///
/// `library`'s actual build no longer discovers this file — its
/// `driver_dir` is a sibling of `workspace`, not a descendant, so Cargo's
/// directory-based config discovery never reaches it; `Library`
/// passes the equivalent `[source]` override directly via `--config`
/// instead (verified working with real vendored crates, offline). This
/// function's return value is still consulted for `diagnose_manifest`
/// below (a plain filesystem read, unrelated to Cargo's own config
/// resolution) and still applies as-is to a future `binary`
/// evaluator, which builds directly in `workspace`.
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

/// Recursively copies `src`'s tree onto `dest`, path-for-path. `dest` is
/// assumed to contain nothing at any of those paths — true by construction
/// for both callers (a freshly-created `driver_dir` that's never reused
/// across jobs; a `binary` workspace where the harness paths are
/// instructor-chosen to not collide with the student's own). If that
/// invariant is ever violated, `std::fs::copy` simply overwrites the file —
/// there's no separate collision-detection step to keep in sync.
fn copy_dir_into(dest: &Path, src: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|source| Error::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    let entries = std::fs::read_dir(src).map_err(|source| Error::Io {
        path: src.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: src.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_into(&dest_path, &entry.path())?;
        } else if file_type.is_file() {
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
        let driver_dir = tempfile::tempdir().unwrap();
        let outcome = prepare(
            workspace.path(),
            driver_dir.path(),
            package.path(),
            &spec,
            Tier::Authoritative,
        )
        .unwrap();

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
        let driver_dir = tempfile::tempdir().unwrap();
        let outcome = prepare(
            workspace.path(),
            driver_dir.path(),
            package.path(),
            &spec,
            Tier::Authoritative,
        )
        .unwrap();

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
        let driver_dir = tempfile::tempdir().unwrap();
        let outcome = prepare(
            workspace.path(),
            driver_dir.path(),
            package.path(),
            &spec,
            Tier::Authoritative,
        )
        .unwrap();

        assert!(outcome.offline_env.vendor_dir.is_none());
        assert!(!workspace.path().join(".cargo/config.toml").exists());
    }

    #[test]
    fn ci_tier_builds_the_library_harness_in_place_without_copying_it() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        write(&workspace.path().join("src/lib.rs"), "// student code");
        write(
            &package.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\n",
        );
        write(&package.path().join("harness/tests/judge.rs"), "// judge");

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        // A scratch dir that's never actually used for Ci -- confirms the
        // harness stays in place rather than getting copied there.
        let unused_driver_dir = tempfile::tempdir().unwrap();
        let outcome = prepare(
            workspace.path(),
            unused_driver_dir.path(),
            package.path(),
            &spec,
            Tier::Ci,
        )
        .unwrap();

        assert_eq!(outcome.driver_dir, package.path().join("harness"));
        assert!(
            std::fs::read_dir(unused_driver_dir.path())
                .unwrap()
                .next()
                .is_none(),
            "Ci tier must not copy the harness into the scratch driver_dir hint"
        );
    }

    #[test]
    fn authoritative_tier_copies_the_library_harness_into_a_fresh_driver_dir() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        write(&workspace.path().join("src/lib.rs"), "// student code");
        write(
            &package.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\n",
        );

        let spec: Spec = toml::from_str(SPEC_TOML).unwrap();
        let driver_dir = tempfile::tempdir().unwrap();
        let outcome = prepare(
            workspace.path(),
            driver_dir.path(),
            package.path(),
            &spec,
            Tier::Authoritative,
        )
        .unwrap();

        assert_eq!(outcome.driver_dir, driver_dir.path().to_path_buf());
        assert!(driver_dir.path().join("Cargo.toml").is_file());
    }

    #[test]
    fn copy_dir_into_recursively_copies_the_whole_tree() {
        let dest = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();

        write(
            &src.path().join("Cargo.toml"),
            "[package]\nname = \"driver\"\n",
        );
        write(&src.path().join("tests/judge.rs"), "// judge");

        copy_dir_into(dest.path(), src.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.path().join("Cargo.toml")).unwrap(),
            "[package]\nname = \"driver\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("tests/judge.rs")).unwrap(),
            "// judge"
        );
    }
}
