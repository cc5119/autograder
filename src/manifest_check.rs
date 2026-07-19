//! Diffs a student `Cargo.toml` against an allowlist, producing precise
//! diagnostics instead of letting the student hit an opaque
//! offline-resolution failure, plus static rejections of `[patch]`, git
//! deps, and external path deps. The allowlist itself isn't hand-typed
//! anywhere -- `crate::prepare` derives it fresh from the blessed
//! `Cargo.lock`'s resolved dependency graph (`{id}`'s own direct
//! dependencies, via `crate::cargo_lock::CargoLock::direct_dependencies`),
//! so it can never drift from what the reference solution actually
//! depends on.

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
    /// hash recorded in `autograder.toml` -- see `crate::lock::verify`,
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
        if dep.path().is_some() {
            diagnostics.push(ManifestDiagnostic::PathDependency { name: name.clone() });
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
        let diagnostics = check_manifest(manifest, &allowlist(&[]), None).unwrap();
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
        let diagnostics = check_manifest(manifest, &allowlist(&[("serde", "1")]), None).unwrap();
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn flags_a_version_outside_the_allowlisted_range_with_no_vendor_dir() {
        let manifest = r#"
[dependencies]
serde = "2.0"
"#;
        let diagnostics = check_manifest(manifest, &allowlist(&[("serde", "1")]), None).unwrap();
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
        let diagnostics =
            check_manifest(manifest, &allowlist(&[("serde", "1")]), Some(vendor.path())).unwrap();

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
        let diagnostics =
            check_manifest(manifest, &allowlist(&[("serde", "1")]), Some(vendor.path())).unwrap();

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
        let diagnostics = check_manifest(manifest, &allowlist(&[]), None).unwrap();

        assert!(diagnostics.contains(&ManifestDiagnostic::GitDependency {
            name: "evil".into()
        }));
        assert!(diagnostics.contains(&ManifestDiagnostic::PathDependency {
            name: "local".into()
        }));
        assert!(diagnostics.contains(&ManifestDiagnostic::DisallowedPatchSection));
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
            Some(vendor.path()),
        )
        .unwrap();

        assert!(diagnostics.is_empty());
    }
}
