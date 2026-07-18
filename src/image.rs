//! Per-assignment base image build (design §10, §13 -- M4 step 20): a
//! container image carrying the pinned Rust toolchain and `cargo-nextest`,
//! tagged `autograder-base:<toolchain-channel>`. `ContainerSandbox::
//! preflight` (see `sandbox::container`) checks for exactly this tag before
//! any job runs -- one clear "image missing" error up front instead of
//! every student in the batch scoring a misleading `build_failed`. This
//! module is the other half: it produces the `Containerfile` and the
//! `podman build` command that actually creates that tag.
//!
//! `base_image_tag` is also the single source of truth other code reads
//! (`ContainerSandbox::new`'s callers) so the tag `preflight` checks for and
//! the tag this module builds can never drift apart.
//!
//! Command/file construction is pure and unit-tested; the live `podman
//! build` **[deferred: needs podman]**, matching every other container
//! invocation in this codebase (ground rule 3).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::spec::Spec;

/// The tag `ContainerSandbox::preflight` checks for and `build_base_image`
/// produces -- one name, no separate config to keep in sync with it.
pub fn base_image_tag(spec: &Spec) -> String {
    format!("autograder-base:{}", spec.toolchain.channel)
}

/// The `Containerfile` contents for `spec`: the pinned toolchain plus
/// `cargo-nextest` (a hard dependency for both tiers' judge protocol, see
/// design §9.1's closing note), so no job-time step ever needs network
/// access to install it. Pre-warming the vendored dependency build (design
/// §13) is left to a separate `cargo build` layer added by the caller once
/// `vendor/` exists, rather than baked in here, so this function stays
/// callable before `prefetch` has ever run for the assignment.
pub fn containerfile(spec: &Spec) -> String {
    format!(
        "FROM docker.io/library/rust:{channel}\n\
         RUN cargo install cargo-nextest --locked\n",
        channel = spec.toolchain.channel
    )
}

/// The `podman build` argv (excluding the `podman` binary itself) that
/// produces `tag` from `containerfile_path`, built against `context_dir`.
pub fn build_argv(tag: &str, containerfile_path: &Path, context_dir: &Path) -> Vec<String> {
    vec![
        "build".to_string(),
        "-t".to_string(),
        tag.to_string(),
        "-f".to_string(),
        containerfile_path.display().to_string(),
        context_dir.display().to_string(),
    ]
}

/// Writes the `Containerfile` into `package_dir` and shells out to `podman
/// build`, tagging the result `base_image_tag(spec)`. Trusted, one-time
/// per assignment (an instructor action, analogous to `vendor::prefetch`) --
/// never runs as part of grading a student. **[deferred: needs podman]**
/// live execution; `containerfile`/`build_argv` above are unit-tested
/// without it.
pub fn build_base_image(podman_bin: &str, spec: &Spec, package_dir: &Path) -> Result<()> {
    let containerfile_path = package_dir.join("Containerfile");
    std::fs::write(&containerfile_path, containerfile(spec)).map_err(|source| Error::Io {
        path: containerfile_path.clone(),
        source,
    })?;

    let tag = base_image_tag(spec);
    let argv = build_argv(&tag, &containerfile_path, package_dir);
    let output = Command::new(podman_bin)
        .args(&argv)
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{podman_bin} build`: {source}")))?;

    if !output.status.success() {
        return Err(Error::Other(format!(
            "podman build failed for {tag}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// Unused directly by this module, kept for callers that want the
/// `Containerfile`'s on-disk location without re-deriving it.
pub fn containerfile_path(package_dir: &Path) -> PathBuf {
    package_dir.join("Containerfile")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        let toml = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00"

[toolchain]
channel = "1.86.0"

[allowed-crates]

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
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn base_image_tag_is_keyed_by_toolchain_channel() {
        assert_eq!(base_image_tag(&spec()), "autograder-base:1.86.0");
    }

    #[test]
    fn containerfile_pins_the_toolchain_and_installs_nextest() {
        let file = containerfile(&spec());
        assert!(file.contains("rust:1.86.0"));
        assert!(file.contains("cargo-nextest"));
    }

    #[test]
    fn build_argv_names_the_tag_containerfile_and_context() {
        let argv = build_argv(
            "autograder-base:1.86.0",
            Path::new("/pkg/Containerfile"),
            Path::new("/pkg"),
        );
        assert_eq!(
            argv,
            vec![
                "build",
                "-t",
                "autograder-base:1.86.0",
                "-f",
                "/pkg/Containerfile",
                "/pkg",
            ]
        );
    }

    #[test]
    fn build_base_image_writes_the_containerfile_before_shelling_out() {
        // A fake `podman` that always fails, standing in for "no podman on
        // this host" -- this test only asserts the Containerfile gets
        // written first and the failure is surfaced clearly, not that a
        // real image gets built (that's the deferred/live part).
        let package_dir = tempfile::tempdir().unwrap();
        let err = build_base_image(
            "autograder-podman-does-not-exist",
            &spec(),
            package_dir.path(),
        )
        .unwrap_err();

        assert!(package_dir.path().join("Containerfile").is_file());
        assert!(err.to_string().contains("failed to run"));
    }
}
