use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::spec::Spec;

#[derive(Debug, Clone)]
pub struct VendorOutcome {
    pub vendor_dir: PathBuf,
    pub cargo_config_path: PathBuf,
}

/// A synthetic `Cargo.toml` whose only purpose is to give `cargo vendor` a
/// manifest to resolve `[allowed-crates]` against. Never built; a stub
/// `src/lib.rs` accompanies it only because cargo requires a target to
/// parse the manifest at all.
pub fn synthetic_manifest_toml(spec: &Spec) -> String {
    let mut out = String::new();
    out.push_str("[package]\n");
    out.push_str("name = \"autograder-vendor-probe\"\n");
    out.push_str("version = \"0.0.0\"\n");
    out.push_str("edition = \"2024\"\n\n");
    out.push_str("[dependencies]\n");
    for (name, version) in &spec.allowed_crates {
        out.push_str(&format!("{name} = \"{version}\"\n"));
    }
    out
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

/// Runs `cargo vendor` against a synthetic manifest built from
/// `spec.allowed_crates`, producing `<package_dir>/vendor/` plus
/// `<package_dir>/.cargo/config.toml`. Trusted, online, one-time per
/// assignment -- never runs on student code.
pub fn prefetch(package_dir: &Path, spec: &Spec) -> Result<VendorOutcome> {
    let vendor_dir = package_dir.join("vendor");

    if spec.allowed_crates.is_empty() {
        // `cargo vendor` refuses to run with zero dependencies ("There is
        // no dependency to vendor in this project"); an empty allowlist is
        // a legitimate spec, so produce an empty vendor dir instead of
        // treating this as an error.
        crate::fs::create_dir_all(&vendor_dir)?;
    } else {
        let manifest_dir = package_dir.join(".vendor-manifest");
        let src_dir = manifest_dir.join("src");
        crate::fs::create_dir_all(&src_dir)?;
        let manifest_path = manifest_dir.join("Cargo.toml");
        crate::fs::write(&manifest_path, synthetic_manifest_toml(spec))?;
        crate::fs::write(&src_dir.join("lib.rs"), "")?;

        let output = Command::new("cargo")
            .arg("vendor")
            .arg(&vendor_dir)
            .arg("--manifest-path")
            .arg(&manifest_path)
            .output()
            .map_err(|source| Error::Other(format!("failed to run cargo vendor: {source}")))?;

        if !output.status.success() {
            return Err(Error::Other(format!(
                "cargo vendor failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }

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

    fn spec_with_crates(crates: &[(&str, &str)]) -> Spec {
        let mut toml = String::from(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"


[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]
"#,
        );
        for (name, version) in crates {
            toml.push_str(&format!("{name} = \"{version}\"\n"));
        }
        toml.push_str(
            r#"
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
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn synthetic_manifest_lists_every_allowed_crate() {
        let spec = spec_with_crates(&[("serde", "1"), ("rand", "0.8")]);
        let manifest = synthetic_manifest_toml(&spec);
        assert!(manifest.contains("serde = \"1\""));
        assert!(manifest.contains("rand = \"0.8\""));
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
    fn prefetch_with_empty_allowlist_produces_an_empty_vendor_dir_and_config() {
        let package = tempfile::tempdir().unwrap();
        let spec = spec_with_crates(&[]);

        let outcome = prefetch(package.path(), &spec).unwrap();

        assert!(outcome.vendor_dir.is_dir());
        assert!(outcome.cargo_config_path.is_file());
        let config = std::fs::read_to_string(&outcome.cargo_config_path).unwrap();
        assert!(config.contains("vendored-sources"));
    }
}
