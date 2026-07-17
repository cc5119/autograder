//! Emits the starter/template repo for distribution to students (design
//! §5.1, §11.2): the public harness vendored under `.autograder/public/`, a
//! thin GitHub Actions wrapper around `autograder ci`, and a student
//! `Cargo.toml` constrained to the assignment's dependency allowlist.
//!
//! `package_dir` is expected to be the **public** assignment repo (already
//! holding `autograder.public.toml`, `harness/`, `fixtures/`, and — once
//! `prefetch` has run against it — `vendor/` + `.cargo/`), never the private
//! instructor package (design §5: the two repos are independent and the
//! authoritative tier never depends on the public one, or vice versa).

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::spec::{self, Spec};

/// Placeholder release coordinates for the downloaded `autograder` binary
/// (design §11.2). The real pinned version + sha256 come from the release
/// pipeline (M3 step 19, `.github/workflows/release.yml` in this repo); an
/// instructor stands up their own fork/release and edits these before
/// distributing a starter repo.
const RELEASE_REPO: &str = "your-org/autograder";
const RELEASE_VERSION: &str = "v0.1.0";
const RELEASE_SHA256_PLACEHOLDER: &str = "<pinned-sha256>";

#[derive(Debug, Clone)]
pub struct ScaffoldOutcome {
    pub out_dir: PathBuf,
}

/// Builds the starter template at `out_dir` from the public assignment
/// package at `package_dir`.
pub fn scaffold(package_dir: &Path, out_dir: &Path) -> Result<ScaffoldOutcome> {
    let public_spec_path = package_dir.join(spec::PUBLIC_SPEC_FILE);
    if !public_spec_path.is_file() {
        return Err(Error::InvalidSpec(format!(
            "scaffold requires {} in {} (the public assignment repo, not the private instructor package)",
            spec::PUBLIC_SPEC_FILE,
            package_dir.display()
        )));
    }
    let spec = Spec::load_file(&public_spec_path)?;

    let public_dir = out_dir.join(".autograder/public");
    std::fs::create_dir_all(&public_dir).map_err(|source| Error::Io {
        path: public_dir.clone(),
        source,
    })?;
    copy_file(
        &public_spec_path,
        &public_dir.join(spec::PUBLIC_SPEC_FILE),
    )?;
    copy_dir_if_exists(&package_dir.join("harness"), &public_dir.join("harness"))?;
    copy_dir_if_exists(&package_dir.join("fixtures"), &public_dir.join("fixtures"))?;
    copy_dir_if_exists(&package_dir.join("vendor"), &public_dir.join("vendor"))?;
    copy_dir_if_exists(&package_dir.join(".cargo"), &public_dir.join(".cargo"))?;

    let workflow_dir = out_dir.join(".github/workflows");
    std::fs::create_dir_all(&workflow_dir).map_err(|source| Error::Io {
        path: workflow_dir.clone(),
        source,
    })?;
    let workflow_path = workflow_dir.join("autograde.yml");
    std::fs::write(&workflow_path, autograde_workflow_yaml()).map_err(|source| Error::Io {
        path: workflow_path.clone(),
        source,
    })?;

    let manifest_path = out_dir.join("Cargo.toml");
    std::fs::write(&manifest_path, student_manifest_toml(&spec)).map_err(|source| Error::Io {
        path: manifest_path.clone(),
        source,
    })?;

    Ok(ScaffoldOutcome {
        out_dir: out_dir.to_path_buf(),
    })
}

/// The thin GitHub Actions wrapper (design §11.2): downloads a
/// version-pinned prebuilt `autograder` binary, verifies its checksum, and
/// runs `autograder ci` against the vendored public harness. No compile
/// step, so student CI never needs a Rust toolchain of its own for the
/// grader itself.
fn autograde_workflow_yaml() -> String {
    format!(
        r#"name: autograde
on:
  push:
    branches: [main]        # default branch only
jobs:
  public-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          curl -fsSL https://github.com/{RELEASE_REPO}/releases/download/{RELEASE_VERSION}/autograder-x86_64-linux -o autograder
          echo "{RELEASE_SHA256_PLACEHOLDER}  autograder" | sha256sum -c -   # verify before exec
          chmod +x autograder
      - run: ./autograder ci --harness .autograder/public
"#
    )
}

/// A student `Cargo.toml` pre-populated with exactly `[allowed-crates]`, so
/// the starter repo already reflects the dependency constraint the offline
/// vendored build enforces (design §8) rather than students discovering it
/// only when their build fails.
fn student_manifest_toml(spec: &Spec) -> String {
    let package_name = spec
        .student
        .package_name
        .clone()
        .or_else(|| spec.student.bin_name.clone())
        .unwrap_or_else(|| "solution".to_string());

    let mut out = String::new();
    out.push_str("[package]\n");
    out.push_str(&format!("name = \"{package_name}\"\n"));
    out.push_str("version = \"0.1.0\"\n");
    out.push_str("edition = \"2021\"\n\n");
    out.push_str("[dependencies]\n");
    for (name, version) in &spec.allowed_crates {
        out.push_str(&format!("{name} = \"{version}\"\n"));
    }
    out
}

fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    std::fs::copy(src, dst).map_err(|source| Error::Io {
        path: src.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Recursively copies `src` onto `dst`, or does nothing if `src` doesn't
/// exist (e.g. an assignment with no `fixtures/`, or a package that hasn't
/// been prefetched yet and so has no `vendor/`).
fn copy_dir_if_exists(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst).map_err(|source| Error::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    for entry in std::fs::read_dir(src).map_err(|source| Error::Io {
        path: src.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::Io {
            path: src.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_if_exists(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            copy_file(&entry.path(), &dst_path)?;
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

    const PUBLIC_SPEC: &str = r#"
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
rand  = "0.8"

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
[[scoring.tests]]
name = "insert_basic"
visibility = "public"
"#;

    #[test]
    fn scaffold_produces_the_documented_starter_tree() {
        let package_dir = tempfile::tempdir().unwrap();
        write(
            &package_dir.path().join(spec::PUBLIC_SPEC_FILE),
            PUBLIC_SPEC,
        );
        write(
            &package_dir.path().join("harness/tests/public_test.rs"),
            "// public test",
        );
        write(
            &package_dir.path().join("fixtures/sample.json"),
            "{}",
        );

        let out_dir = tempfile::tempdir().unwrap();
        let outcome = scaffold(package_dir.path(), out_dir.path()).unwrap();

        assert!(outcome
            .out_dir
            .join(".autograder/public")
            .join(spec::PUBLIC_SPEC_FILE)
            .is_file());
        assert!(outcome
            .out_dir
            .join(".autograder/public/harness/tests/public_test.rs")
            .is_file());
        assert!(outcome
            .out_dir
            .join(".autograder/public/fixtures/sample.json")
            .is_file());
        assert!(outcome
            .out_dir
            .join(".github/workflows/autograde.yml")
            .is_file());
        assert!(outcome.out_dir.join("Cargo.toml").is_file());
    }

    #[test]
    fn emitted_workflow_matches_the_design_reference_wrapper() {
        let package_dir = tempfile::tempdir().unwrap();
        write(
            &package_dir.path().join(spec::PUBLIC_SPEC_FILE),
            PUBLIC_SPEC,
        );
        let out_dir = tempfile::tempdir().unwrap();

        scaffold(package_dir.path(), out_dir.path()).unwrap();

        let workflow =
            std::fs::read_to_string(out_dir.path().join(".github/workflows/autograde.yml"))
                .unwrap();
        assert!(workflow.contains("on:\n  push:\n    branches: [main]"));
        assert!(workflow.contains("sha256sum -c -"));
        assert!(workflow.contains("./autograder ci --harness .autograder/public"));
    }

    #[test]
    fn emitted_student_manifest_is_constrained_to_the_allowlist() {
        let package_dir = tempfile::tempdir().unwrap();
        write(
            &package_dir.path().join(spec::PUBLIC_SPEC_FILE),
            PUBLIC_SPEC,
        );
        let out_dir = tempfile::tempdir().unwrap();

        scaffold(package_dir.path(), out_dir.path()).unwrap();

        let manifest = std::fs::read_to_string(out_dir.path().join("Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"bst\""));
        assert!(manifest.contains("serde = \"1\""));
        assert!(manifest.contains("rand = \"0.8\""));
    }

    #[test]
    fn scaffold_rejects_a_package_dir_without_a_public_spec() {
        let package_dir = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();

        let err = scaffold(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }
}
