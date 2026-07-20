//! Parses `Cargo.lock` (the workspace-root lockfile shared by `{id}` and
//! `{harness}`, produced by `crate::deps::lock`) enough to derive two things: the
//! exact resolved version of any locked crate, and a given package's own
//! direct dependencies. The latter *is* the student-facing allowlist --
//! read straight off the blessed lock instead of a hand-typed
//! `[allowed-crates]` table that could drift from it.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Deserialize)]
struct RawLockfile {
    #[serde(default, rename = "package")]
    packages: Vec<RawPackage>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPackage {
    name: String,
    version: String,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CargoLock {
    packages: Vec<RawPackage>,
}

impl CargoLock {
    pub fn parse(toml_str: &str) -> Result<CargoLock> {
        let raw: RawLockfile = toml::from_str(toml_str)
            .map_err(|source| Error::Other(format!("failed to parse Cargo.lock: {source}")))?;
        Ok(CargoLock {
            packages: raw.packages,
        })
    }

    /// The exact resolved version of `crate_name`, if it's locked and
    /// unambiguous (more than one locked version of the same name is
    /// possible in a real dependency graph, but never for a workspace
    /// member's own name, which is all callers ever look up here).
    fn version_of(&self, crate_name: &str) -> Option<&str> {
        let mut matches = self.packages.iter().filter(|p| p.name == crate_name);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(&first.version)
    }

    /// `package_name`'s own direct dependencies (name -> exact locked
    /// version), resolved from its `Cargo.lock` `dependencies` entries.
    /// Each entry is `"name"`, or `"name version"` when Cargo needed to
    /// disambiguate multiple locked versions of that name.
    pub fn direct_dependencies(&self, package_name: &str) -> BTreeMap<String, String> {
        let Some(package) = self.packages.iter().find(|p| p.name == package_name) else {
            return BTreeMap::new();
        };
        package
            .dependencies
            .iter()
            .filter_map(|dep| {
                let mut parts = dep.split_whitespace();
                let name = parts.next()?;
                let version = match parts.next() {
                    Some(v) => v.to_string(),
                    None => self.version_of(name)?.to_string(),
                };
                Some((name.to_string(), version))
            })
            .collect()
    }
}

/// The SHA-256 (lowercase hex) of `contents` -- used to fingerprint a
/// `Cargo.lock` both when `autograder lock` records the "blessed" hash and
/// when `prepare`/`publish` verify a checkout's lockfile still matches it.
pub fn sha256_hex(contents: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(contents.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LOCK: &str = r#"
version = 4

[[package]]
name = "hw3"
version = "0.1.0"
dependencies = [
 "serde",
]

[[package]]
name = "harness"
version = "0.0.0"
dependencies = [
 "hw3",
 "serde",
]

[[package]]
name = "serde"
version = "1.0.219"
"#;

    #[test]
    fn direct_dependencies_resolves_names_to_exact_locked_versions() {
        let lock = CargoLock::parse(SAMPLE_LOCK).unwrap();
        let deps = lock.direct_dependencies("hw3");
        assert_eq!(deps.get("serde"), Some(&"1.0.219".to_string()));
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn direct_dependencies_includes_path_deps_resolved_from_the_workspace() {
        let lock = CargoLock::parse(SAMPLE_LOCK).unwrap();
        let deps = lock.direct_dependencies("harness");
        assert_eq!(deps.get("hw3"), Some(&"0.1.0".to_string()));
        assert_eq!(deps.get("serde"), Some(&"1.0.219".to_string()));
    }

    #[test]
    fn direct_dependencies_of_an_unknown_package_is_empty() {
        let lock = CargoLock::parse(SAMPLE_LOCK).unwrap();
        assert!(lock.direct_dependencies("nope").is_empty());
    }

    #[test]
    fn a_disambiguated_dependency_entry_uses_its_own_inline_version() {
        let lock_toml = r#"
[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "rand 0.8.5",
]

[[package]]
name = "rand"
version = "0.8.5"

[[package]]
name = "rand"
version = "0.9.0"
"#;
        let lock = CargoLock::parse(lock_toml).unwrap();
        let deps = lock.direct_dependencies("app");
        assert_eq!(deps.get("rand"), Some(&"0.8.5".to_string()));
    }

    #[test]
    fn sha256_hex_is_deterministic_and_sensitive_to_content() {
        let a = sha256_hex("hello");
        let b = sha256_hex("hello");
        let c = sha256_hex("hello!");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }
}
