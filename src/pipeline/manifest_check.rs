//! Diffs a student `Cargo.toml` against an allowlist, producing precise
//! diagnostics instead of letting the student hit an opaque
//! offline-resolution failure, plus static rejections of `[patch]`, git
//! deps, and external path deps. The allowlist itself isn't hand-typed
//! anywhere -- `crate::pipeline::prepare` derives it fresh from the blessed
//! `Cargo.lock`'s resolved dependency graph (`{id}`'s own direct
//! dependencies, via `crate::deps::cargo_lock::CargoLock::direct_dependencies`),
//! so it can never drift from what the reference solution actually
//! depends on.
//!
//! The path-dependency rejection is not redundant with the blessed
//! `cargo-lock-sha256`, for two reasons. The sandbox builds `--offline`,
//! not `--locked` (see `crate::pipeline::evaluator::nextest`), so the
//! lockfile does not pin what a student's manifest resolves to -- what
//! actually constrains it is the vendor directory, and path dependencies
//! never go through the vendor directory at all. And a lockfile records no
//! location for a path dependency, only its name and version, so a
//! sibling crate directory shadowing an allowlisted name produces a
//! byte-identical lock entry that no hash could distinguish. The one
//! exception is a package the instructor owns and overlays themselves:
//! `[assignment].extra-packages`, accepted only at the exact relative path
//! the grading workspace's layout puts it at.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestDiagnostic {
    DisallowedCrate {
        name: String,
    },
    VersionOutsideVendored {
        name: String,
        requested: String,
        vendored: String,
    },
    MissingFeature {
        name: String,
        feature: String,
    },
    DisallowedPatchSection,
    GitDependency {
        name: String,
    },
    PathDependency {
        name: String,
    },
    /// `Cargo.lock`, as actually checked out, doesn't match the blessed
    /// hash recorded in `autograder.toml` -- see `crate::deps::lock::verify`,
    /// whose message is carried here verbatim.
    LockfileMismatch(String),
}

impl std::fmt::Display for ManifestDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestDiagnostic::DisallowedCrate { name } => {
                write!(f, "disallowed crate: {name}")
            }
            ManifestDiagnostic::VersionOutsideVendored {
                name,
                requested,
                vendored,
            } => write!(
                f,
                "crate {name} requires version {requested}, but only {vendored} is vendored"
            ),
            ManifestDiagnostic::MissingFeature { name, feature } => write!(
                f,
                "crate {name} needs feature \"{feature}\", not enabled in the vendored build"
            ),
            ManifestDiagnostic::DisallowedPatchSection => {
                write!(f, "disallowed [patch] section in Cargo.toml")
            }
            ManifestDiagnostic::GitDependency { name } => {
                write!(f, "disallowed git dependency: {name}")
            }
            ManifestDiagnostic::PathDependency { name } => {
                write!(f, "disallowed external path dependency: {name}")
            }
            ManifestDiagnostic::LockfileMismatch(message) => write!(f, "{message}"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CargoManifest {
    #[serde(default)]
    dependencies: BTreeMap<String, DependencySpec>,
    #[serde(default)]
    patch: toml::Table,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DependencySpec {
    Version(String),
    Detailed(DetailedDependency),
}

#[derive(Debug, Default, Deserialize)]
struct DetailedDependency {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    git: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

impl DependencySpec {
    fn version(&self) -> Option<&str> {
        match self {
            DependencySpec::Version(v) => Some(v),
            DependencySpec::Detailed(d) => d.version.as_deref(),
        }
    }
    fn features(&self) -> &[String] {
        match self {
            DependencySpec::Version(_) => &[],
            DependencySpec::Detailed(d) => &d.features,
        }
    }
    fn git(&self) -> Option<&str> {
        match self {
            DependencySpec::Version(_) => None,
            DependencySpec::Detailed(d) => d.git.as_deref(),
        }
    }
    fn path(&self) -> Option<&str> {
        match self {
            DependencySpec::Version(_) => None,
            DependencySpec::Detailed(d) => d.path.as_deref(),
        }
    }
}

/// Diffs `manifest_toml` (a student `Cargo.toml`) against `allowed_crates`
/// (derived from the blessed `Cargo.lock`, see this module's doc comment),
/// consulting `vendor_dir` (if present) for the exact pinned
/// version/features actually vendored. Returns diagnostics in the order
/// they were found; an empty vec means the manifest is clean.
pub fn check_manifest(
    manifest_toml: &str,
    allowed_crates: &BTreeMap<String, String>,
    extra_packages: &[String],
    vendor_dir: Option<&Path>,
) -> Result<Vec<ManifestDiagnostic>> {
    let manifest: CargoManifest = toml::from_str(manifest_toml)
        .map_err(|source| Error::Other(format!("failed to parse student Cargo.toml: {source}")))?;

    let mut diagnostics = Vec::new();

    if !manifest.patch.is_empty() {
        diagnostics.push(ManifestDiagnostic::DisallowedPatchSection);
    }

    for (name, dep) in &manifest.dependencies {
        if dep.git().is_some() {
            diagnostics.push(ManifestDiagnostic::GitDependency { name: name.clone() });
            continue;
        }
        // A path dependency resolves off disk, so neither the allowlist's
        // version nor the vendored one says anything about it -- an
        // accepted one is fully checked by the path itself.
        if let Some(path) = dep.path() {
            if !is_extra_package_path(name, path, extra_packages) {
                diagnostics.push(ManifestDiagnostic::PathDependency { name: name.clone() });
            }
            continue;
        }

        let Some(allowed_req) = allowed_crates.get(name) else {
            diagnostics.push(ManifestDiagnostic::DisallowedCrate { name: name.clone() });
            continue;
        };

        let requested = dep.version().unwrap_or("*").to_string();

        if let Some(vendor_dir) = vendor_dir
            && let Some(vendored_version) = read_vendored_version(vendor_dir, name)
        {
            if !version_matches(&requested, &vendored_version) {
                diagnostics.push(ManifestDiagnostic::VersionOutsideVendored {
                    name: name.clone(),
                    requested,
                    vendored: vendored_version,
                });
                continue;
            }
            if let Some(available_features) = read_vendored_features(vendor_dir, name) {
                for feature in dep.features() {
                    if !available_features.contains(feature) {
                        diagnostics.push(ManifestDiagnostic::MissingFeature {
                            name: name.clone(),
                            feature: feature.clone(),
                        });
                    }
                }
            }
            continue;
        }

        // Not vendored yet: fall back to a best-effort check against the
        // allowlist's own version requirement string.
        if !requested_is_compatible_with_allowed(&requested, allowed_req) {
            diagnostics.push(ManifestDiagnostic::VersionOutsideVendored {
                name: name.clone(),
                requested,
                vendored: allowed_req.clone(),
            });
        }
    }

    Ok(diagnostics)
}

/// Whether `path` (as written in `{id}/Cargo.toml`, so relative to `{id}/`)
/// is the one location an `[assignment].extra-packages` dependency is
/// allowed to point at: `../<name>`, the sibling directory the grading
/// workspace overlays that package into.
///
/// Both halves matter. The name has to be a declared extra package,
/// because otherwise any allowlisted crate could be shadowed by a local
/// directory; and the path has to be exactly the sibling, because a
/// student ships their whole `{id}/**` subtree and could otherwise put a
/// crate named `messaging` at `{id}/messaging/` and point at that instead
/// of the instructor's.
fn is_extra_package_path(name: &str, path: &str, extra_packages: &[String]) -> bool {
    extra_packages.iter().any(|package| package == name)
        && normalize(path).as_deref() == Some(format!("../{name}").as_str())
}

/// `path` with `.` components dropped and separators collapsed, so
/// `"./../messaging"` and `"../messaging/"` both compare equal to
/// `"../messaging"`. `None` for anything absolute -- never legitimate here,
/// and not something a textual comparison should be trusted to judge.
fn normalize(path: &str) -> Option<String> {
    let path = Path::new(path);
    if path.is_absolute() {
        return None;
    }
    let components: Vec<&str> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::CurDir => None,
            std::path::Component::ParentDir => Some(".."),
            std::path::Component::Normal(part) => part.to_str(),
            // Absolute-path components, already rejected above.
            _ => None,
        })
        .collect();
    Some(components.join("/"))
}

/// Best-effort padding of a plain version string ("1", "0.8", "1.2") into a
/// full `major.minor.patch` so it can be parsed as a `semver::Version`.
fn pad_version(v: &str) -> String {
    let v = v.trim().trim_start_matches(['^', '~', '=', ' ']);
    match v.matches('.').count() {
        0 => format!("{v}.0.0"),
        1 => format!("{v}.0"),
        _ => v.to_string(),
    }
}

fn version_matches(requested: &str, vendored_version: &str) -> bool {
    let Ok(req) = semver::VersionReq::parse(requested) else {
        return true; // unparseable requirement: don't block on our own limitation
    };
    let Ok(version) = semver::Version::parse(&pad_version(vendored_version)) else {
        return true;
    };
    req.matches(&version)
}

/// Whether `requested` (a student's version requirement) could resolve to
/// some version satisfying `allowed_req` (the allowlist's requirement),
/// approximated by checking the requested string's own minimal version
/// against `allowed_req` when it parses as a plain version. Requirements
/// that don't parse this simply are accepted rather than false-flagged.
fn requested_is_compatible_with_allowed(requested: &str, allowed_req: &str) -> bool {
    let Ok(allowed) = semver::VersionReq::parse(allowed_req) else {
        return true;
    };
    let Ok(version) = semver::Version::parse(&pad_version(requested)) else {
        return true;
    };
    allowed.matches(&version)
}

#[derive(Debug, Deserialize)]
struct VendoredManifest {
    package: VendoredPackage,
    #[serde(default)]
    features: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
struct VendoredPackage {
    version: String,
}

fn read_vendored_version(vendor_dir: &Path, crate_name: &str) -> Option<String> {
    read_vendored_manifest(vendor_dir, crate_name).map(|m| m.package.version)
}

fn read_vendored_features(vendor_dir: &Path, crate_name: &str) -> Option<Vec<String>> {
    read_vendored_manifest(vendor_dir, crate_name).map(|m| m.features.into_keys().collect())
}

fn read_vendored_manifest(vendor_dir: &Path, crate_name: &str) -> Option<VendoredManifest> {
    let path = vendor_dir.join(crate_name).join("Cargo.toml");
    let contents = std::fs::read_to_string(path).ok()?;
    toml::from_str(&contents).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowlist(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn flags_a_crate_not_in_the_allowlist() {
        let manifest = r#"
[dependencies]
tokio = "1"
"#;
        let diagnostics = check_manifest(manifest, &allowlist(&[]), &[], None).unwrap();
        assert_eq!(
            diagnostics,
            vec![ManifestDiagnostic::DisallowedCrate {
                name: "tokio".into()
            }]
        );
        assert_eq!(diagnostics[0].to_string(), "disallowed crate: tokio");
    }

    #[test]
    fn allows_a_crate_within_the_allowlisted_range() {
        let manifest = r#"
[dependencies]
serde = "1.2"
"#;
        let diagnostics =
            check_manifest(manifest, &allowlist(&[("serde", "1")]), &[], None).unwrap();
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn flags_a_version_outside_the_allowlisted_range_with_no_vendor_dir() {
        let manifest = r#"
[dependencies]
serde = "2.0"
"#;
        let diagnostics =
            check_manifest(manifest, &allowlist(&[("serde", "1")]), &[], None).unwrap();
        assert_eq!(
            diagnostics,
            vec![ManifestDiagnostic::VersionOutsideVendored {
                name: "serde".into(),
                requested: "2.0".into(),
                vendored: "1".into(),
            }]
        );
    }

    #[test]
    fn flags_a_version_outside_the_exact_vendored_version() {
        let vendor = tempfile::tempdir().unwrap();
        let crate_dir = vendor.path().join("serde");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"serde\"\nversion = \"1.4.0\"\n",
        )
        .unwrap();

        let manifest = r#"
[dependencies]
serde = "=1.2.3"
"#;
        let diagnostics = check_manifest(
            manifest,
            &allowlist(&[("serde", "1")]),
            &[],
            Some(vendor.path()),
        )
        .unwrap();

        assert_eq!(
            diagnostics,
            vec![ManifestDiagnostic::VersionOutsideVendored {
                name: "serde".into(),
                requested: "=1.2.3".into(),
                vendored: "1.4.0".into(),
            }]
        );
    }

    #[test]
    fn flags_a_feature_missing_from_the_vendored_crate() {
        let vendor = tempfile::tempdir().unwrap();
        let crate_dir = vendor.path().join("serde");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"serde\"\nversion = \"1.4.0\"\n\n[features]\nstd = []\n",
        )
        .unwrap();

        let manifest = r#"
[dependencies]
serde = { version = "1", features = ["derive"] }
"#;
        let diagnostics = check_manifest(
            manifest,
            &allowlist(&[("serde", "1")]),
            &[],
            Some(vendor.path()),
        )
        .unwrap();

        assert_eq!(
            diagnostics,
            vec![ManifestDiagnostic::MissingFeature {
                name: "serde".into(),
                feature: "derive".into(),
            }]
        );
    }

    #[test]
    fn flags_git_and_path_dependencies_and_a_patch_section() {
        let manifest = r#"
[dependencies]
evil = { git = "https://example.com/evil.git" }
local = { path = "../local" }

[patch.crates-io]
serde = { path = "../fake-serde" }
"#;
        let diagnostics = check_manifest(manifest, &allowlist(&[]), &[], None).unwrap();

        assert!(diagnostics.contains(&ManifestDiagnostic::GitDependency {
            name: "evil".into()
        }));
        assert!(diagnostics.contains(&ManifestDiagnostic::PathDependency {
            name: "local".into()
        }));
        assert!(diagnostics.contains(&ManifestDiagnostic::DisallowedPatchSection));
    }

    fn extra(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The whole point of `[assignment].extra-packages`: `{id}` is allowed
    /// to depend on the instructor's support package, which can only be a
    /// path dependency.
    #[test]
    fn a_declared_extra_package_is_an_allowed_path_dependency() {
        let manifest = r#"
[dependencies]
messaging = { path = "../messaging" }
"#;
        let diagnostics =
            check_manifest(manifest, &allowlist(&[]), &extra(&["messaging"]), None).unwrap();

        assert!(diagnostics.is_empty());
    }

    /// Spelling variations of the same sibling directory are the same
    /// directory -- only where the path lands matters.
    #[test]
    fn an_extra_package_path_is_compared_after_normalizing_it() {
        for path in ["./../messaging", "../messaging/", ".././messaging"] {
            let manifest = format!("[dependencies]\nmessaging = {{ path = {path:?} }}\n");
            let diagnostics =
                check_manifest(&manifest, &allowlist(&[]), &extra(&["messaging"]), None).unwrap();

            assert!(diagnostics.is_empty(), "{path} should be accepted");
        }
    }

    /// A student ships their entire `{id}/**` subtree, so they can plant a
    /// crate named `messaging` inside their own package. Allowing the
    /// *name* without pinning the *path* would let that shadow the
    /// instructor's copy with one the student wrote.
    #[test]
    fn an_extra_package_name_pointed_somewhere_else_is_still_rejected() {
        for path in ["messaging", "../../messaging", "src/messaging", "/tmp/x"] {
            let manifest = format!("[dependencies]\nmessaging = {{ path = {path:?} }}\n");
            let diagnostics =
                check_manifest(&manifest, &allowlist(&[]), &extra(&["messaging"]), None).unwrap();

            assert_eq!(
                diagnostics,
                vec![ManifestDiagnostic::PathDependency {
                    name: "messaging".into()
                }],
                "{path} should be rejected"
            );
        }
    }

    /// The sibling location is only blessed for the packages the spec
    /// actually declares -- an undeclared name at the same path is not.
    #[test]
    fn an_undeclared_package_at_the_sibling_path_is_rejected() {
        let manifest = r#"
[dependencies]
messaging = { path = "../messaging" }
"#;
        let diagnostics =
            check_manifest(manifest, &allowlist(&[]), &extra(&["other"]), None).unwrap();

        assert_eq!(
            diagnostics,
            vec![ManifestDiagnostic::PathDependency {
                name: "messaging".into()
            }]
        );
    }

    #[test]
    fn clean_manifest_within_allowlist_and_vendor_produces_no_diagnostics() {
        let vendor = tempfile::tempdir().unwrap();
        let crate_dir = vendor.path().join("rand");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"rand\"\nversion = \"0.8.5\"\n\n[features]\nstd = []\nsmall_rng = []\n",
        )
        .unwrap();

        let manifest = r#"
[dependencies]
rand = { version = "0.8", features = ["small_rng"] }
"#;
        let diagnostics = check_manifest(
            manifest,
            &allowlist(&[("rand", "0.8")]),
            &[],
            Some(vendor.path()),
        )
        .unwrap();

        assert!(diagnostics.is_empty());
    }
}
