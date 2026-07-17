use std::process::Command;

use crate::error::{Error, Result};
use crate::model::ResourceUsage;

use super::exec::run_with_timeout;
use super::{MountMode, Sandbox, SandboxOutcome, SandboxSpec};

/// Podman exit code convention for a process killed by a signal (128 + signal
/// number); SIGKILL is 9. cgroup OOM-kills deliver SIGKILL, so `137` is the
/// best-effort (not certain — any SIGKILL looks the same) signal that memory
/// was exceeded (design §10).
const OOM_LIKELY_EXIT_CODE: i32 = 137;

/// Rootless-podman shell-out (design §10). Builds a `podman run` argv from a
/// `SandboxSpec` and runs it as a host child process, reusing the same
/// timeout/output-cap machinery as `LocalSandbox` — the isolation itself
/// comes entirely from the podman flags, not from anything this process
/// does. Live execution needs podman; argv construction and outcome parsing
/// are pure and unit-tested without it.
pub struct ContainerSandbox {
    pub podman_bin: String,
    pub base_image: String,
    /// `--user` value, e.g. `"65534:65534"` for a non-root sandbox user.
    pub user: String,
    pub seccomp_profile: std::path::PathBuf,
}

impl ContainerSandbox {
    pub fn new(base_image: impl Into<String>, seccomp_profile: impl Into<std::path::PathBuf>) -> Self {
        Self {
            podman_bin: "podman".into(),
            base_image: base_image.into(),
            user: "65534:65534".into(),
            seccomp_profile: seccomp_profile.into(),
        }
    }

    /// Builds the `podman run` argv (excluding the `podman` binary itself)
    /// for `spec`, per design §10's exact flag set.
    pub fn build_argv(&self, spec: &SandboxSpec) -> Vec<String> {
        let mut argv = vec!["run".to_string(), "--rm".to_string()];

        argv.push(if spec.network {
            "--network=bridge".to_string()
        } else {
            "--network=none".to_string()
        });

        let mem = spec.limits.memory_bytes;
        argv.push(format!("--memory={mem}"));
        argv.push(format!("--memory-swap={mem}"));
        argv.push(format!("--cpus={}", spec.limits.cpus));
        argv.push(format!("--pids-limit={}", spec.limits.pids));
        argv.push("--read-only".to_string());
        argv.push("--cap-drop=ALL".to_string());
        argv.push("--security-opt".to_string());
        argv.push("no-new-privileges".to_string());
        argv.push("--security-opt".to_string());
        argv.push(format!("seccomp={}", self.seccomp_profile.display()));
        argv.push("--user".to_string());
        argv.push(self.user.clone());

        for mount in &spec.mounts {
            let mode = match mount.mode {
                MountMode::ReadOnly => "ro",
                MountMode::ReadWrite => "rw",
            };
            argv.push("-v".to_string());
            argv.push(format!(
                "{}:{}:{mode}",
                mount.host_path.display(),
                mount.container_path.display()
            ));
        }

        for (key, value) in &spec.env {
            argv.push("-e".to_string());
            argv.push(format!("{key}={value}"));
        }

        if let Some(dir) = &spec.workdir {
            argv.push("-w".to_string());
            argv.push(dir.display().to_string());
        }

        argv.push(self.base_image.clone());
        argv.push(spec.program.clone());
        argv.extend(spec.args.iter().cloned());

        argv
    }
}

impl Sandbox for ContainerSandbox {
    /// Two checks, run once before any student is touched, so a broken
    /// environment produces one clear top-level error instead of every
    /// student in the batch silently scoring a misleading `build_failed`
    /// (both failure modes below are, to `run`, just "the sandboxed command
    /// exited non-zero" — indistinguishable from a real `cargo build`
    /// failure without this check):
    ///
    /// 1. `podman version` — catches "podman isn't usable at all" (missing
    ///    binary, no rootless storage configured, no `/run`, etc.).
    /// 2. `podman image exists <base_image>` — catches "podman works fine,
    ///    but the assignment's base image was never built." This is a pure
    ///    local-storage check (no registry contact), so it fails cleanly
    ///    instead of surfacing as `podman run`'s much more confusing
    ///    registry short-name-resolution error when a job actually tries to
    ///    run against a nonexistent image. Never builds the image itself
    ///    (that stays a manual/M4-automation step, deliberately) — this
    ///    only tells you clearly that it's missing.
    fn preflight(&self) -> Result<()> {
        let output = Command::new(&self.podman_bin).arg("version").output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return Err(Error::Other(format!(
                    "podman is not usable in this environment (`{} version` failed): {}",
                    self.podman_bin,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            Err(source) => {
                return Err(Error::Other(format!(
                    "podman is not usable in this environment (failed to run `{}`): {source}",
                    self.podman_bin
                )));
            }
        }

        let exists = Command::new(&self.podman_bin)
            .args(["image", "exists", &self.base_image])
            .status();
        match exists {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => Err(Error::Other(format!(
                "container base image {:?} was not found locally. Jobs run with \
                 --network=none, so the base image must already have the pinned toolchain \
                 (and cargo-nextest) baked in -- build and tag one before grading, e.g.:\n  \
                 podman build -t {} -f Containerfile .",
                self.base_image, self.base_image
            ))),
            Err(source) => Err(Error::Other(format!(
                "failed to check for container base image {:?}: {source}",
                self.base_image
            ))),
        }
    }

    /// **[deferred: needs podman]** — argv construction (`build_argv`) is
    /// unit-tested directly; this method shells out and is exercised on a
    /// provisioned host.
    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome> {
        let argv = self.build_argv(spec);
        let mut cmd = Command::new(&self.podman_bin);
        cmd.args(&argv);

        let outcome = run_with_timeout(
            cmd,
            spec.limits.wall_clock,
            spec.limits.max_output_bytes as usize,
        )?;

        let oom = !outcome.timed_out && outcome.exit_code == Some(OOM_LIKELY_EXIT_CODE);

        Ok(SandboxOutcome {
            exit_code: outcome.exit_code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            timed_out: outcome.timed_out,
            oom,
            resource_usage: ResourceUsage {
                peak_memory_bytes: None,
                cpu_ms: Some(outcome.wall_clock.as_millis() as u64),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Mount, SandboxLimits};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    fn spec() -> SandboxSpec {
        let mut spec = SandboxSpec::new(
            "/judge.sh",
            SandboxLimits {
                wall_clock: Duration::from_secs(10),
                cpus: 2,
                memory_bytes: 2 * 1024 * 1024 * 1024,
                pids: 256,
                max_output_bytes: 1024 * 1024,
            },
        );
        spec.mounts = vec![
            Mount {
                host_path: PathBuf::from("/host/vendor"),
                container_path: PathBuf::from("/vendor"),
                mode: MountMode::ReadOnly,
            },
            Mount {
                host_path: PathBuf::from("/host/work/target"),
                container_path: PathBuf::from("/work/target"),
                mode: MountMode::ReadWrite,
            },
        ];
        spec
    }

    fn sandbox() -> ContainerSandbox {
        ContainerSandbox::new("autograder-base:hw3", "/etc/autograder/seccomp.json")
    }

    #[test]
    fn argv_contains_the_design_mandated_isolation_flags() {
        let argv = sandbox().build_argv(&spec());
        let joined = argv.join(" ");

        assert!(joined.contains("--network=none"));
        assert!(joined.contains("--memory=2147483648"));
        assert!(joined.contains("--memory-swap=2147483648"));
        assert!(joined.contains("--cpus=2"));
        assert!(joined.contains("--pids-limit=256"));
        assert!(joined.contains("--read-only"));
        assert!(joined.contains("--cap-drop=ALL"));
        assert!(joined.contains("no-new-privileges"));
        assert!(joined.contains("seccomp=/etc/autograder/seccomp.json"));
        assert!(joined.contains("--user 65534:65534"));
        assert!(joined.contains("-v /host/vendor:/vendor:ro"));
        assert!(joined.contains("-v /host/work/target:/work/target:rw"));
        assert!(argv.contains(&"autograder-base:hw3".to_string()));
    }

    #[test]
    fn network_true_uses_bridge_instead_of_none() {
        let mut s = spec();
        s.network = true;
        let argv = sandbox().build_argv(&s);
        assert!(argv.contains(&"--network=bridge".to_string()));
        assert!(!argv.contains(&"--network=none".to_string()));
    }

    #[test]
    fn program_and_args_are_the_final_argv_entries() {
        let mut s = spec();
        s.args = vec!["--flag".into()];
        let argv = sandbox().build_argv(&s);
        assert_eq!(argv[argv.len() - 2], "/judge.sh");
        assert_eq!(argv[argv.len() - 1], "--flag");
    }

    fn sandbox_with_bin(podman_bin: &str) -> ContainerSandbox {
        let mut sandbox = sandbox();
        sandbox.podman_bin = podman_bin.to_string();
        sandbox
    }

    #[test]
    fn preflight_fails_clearly_when_podman_is_not_on_path() {
        let err = sandbox_with_bin("autograder-podman-does-not-exist")
            .preflight()
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not usable in this environment"));
    }

    #[test]
    fn preflight_fails_clearly_when_podman_runs_but_errors() {
        // `false` always exits non-zero without needing a real podman
        // install, standing in for a podman binary that runs but can't
        // actually operate (e.g. this host's real "lstat /run" failure).
        let err = sandbox_with_bin("false").preflight().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not usable in this environment"));
    }

    #[test]
    fn preflight_succeeds_when_podman_runs_cleanly() {
        // `true` always exits zero, standing in for a working podman.
        assert!(sandbox_with_bin("true").preflight().is_ok());
    }

    /// A fake `podman` that answers `version` successfully but reports
    /// every `image exists` check as "not found" — standing in for a host
    /// where podman itself works fine but the assignment's base image was
    /// never built.
    fn fake_podman_missing_image() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("podman");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then exit 0; else exit 1; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[test]
    fn preflight_fails_clearly_when_the_base_image_is_missing() {
        let dir = fake_podman_missing_image();
        let sandbox = sandbox_with_bin(dir.path().join("podman").to_str().unwrap());

        let err = sandbox.preflight().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("was not found locally"));
        assert!(message.contains("autograder-base:hw3"));
        assert!(message.contains("podman build"));
    }
}
