use std::path::Path;
use std::process::Command;
use std::time::Duration;

use subprocess::Exec;

use crate::error::{Error, Result};
use crate::exec::fs;

use super::exec::exec_with_timeout;
use super::{MountMode, ProcessStatus, Profile, Sandbox, SandboxOutcome, SandboxSpec};

/// How long to give `podman kill`/`podman rm -f` to finish once we've
/// decided a run timed out. These are trusted, fixed-argument commands we
/// control (not attacker-influenced), so a short bound is just insurance
/// against podman itself being wedged -- it isn't meant to be tight.
const CONTAINER_KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Default path an operator can drop a hardened seccomp profile at for
/// `ContainerSandbox::new` to pick up.
const DEFAULT_SECCOMP_PROFILE: &str = "/etc/autograder/seccomp.json";

/// Preps cgroups/config before `exec`ing the real command; baked into the
/// image (`container/isolate-setup.sh`). Prepended for `Profile::IsolateRun`.
const ISOLATE_SETUP_WRAPPER: &str = "isolate-setup.sh";

/// Rootless-podman shell-out: builds a `podman run` argv from a
/// `SandboxSpec` and runs it as a host child process. Live execution needs
/// podman; argv construction and outcome parsing are pure and unit-tested
/// without it.
pub struct ContainerSandbox {
    pub podman_bin: String,
    pub base_image: String,
    /// `--user` value, e.g. `"65534:65534"` for a non-root sandbox user.
    pub user: String,
    /// A custom seccomp profile for `--security-opt seccomp=...`. `None`
    /// falls back to podman's own bundled default (still denies `mount`,
    /// `ptrace`, `unshare`, `keyctl`, etc.)
    pub seccomp_profile: Option<std::path::PathBuf>,
}

impl ContainerSandbox {
    pub fn new(base_image: impl Into<String>) -> Self {
        Self {
            podman_bin: "podman".into(),
            base_image: base_image.into(),
            user: "65534:65534".into(),
            seccomp_profile: discover_seccomp_profile(),
        }
    }

    /// Builds the `podman run` argv, excluding the `podman` binary itself.
    fn build_argv(&self, spec: &SandboxSpec, cidfile: &Path) -> Vec<String> {
        let mut argv = vec!["run".to_string(), "--rm".to_string()];

        argv.push(format!("--cidfile={}", cidfile.display()));
        argv.push("--network=none".to_string());

        if let Some(limits) = &spec.limits {
            let mem = limits.memory_bytes;
            argv.push(format!("--memory={mem}"));
            argv.push(format!("--memory-swap={mem}"));
            argv.push(format!("--cpus={}", limits.cpus));
            argv.push(format!("--pids-limit={}", limits.pids));
        }

        // Each profile's whole flag set lives in its own arm -- see
        // `Profile`'s doc comment for why `IsolateRun` looks the way it
        // does.
        match spec.profile {
            Profile::Build => {
                argv.push("--read-only".to_string());
                argv.push("--cap-drop=ALL".to_string());
                argv.push("--security-opt".to_string());
                argv.push("no-new-privileges".to_string());
                argv.push("--user".to_string());
                argv.push(self.user.clone());
            }
            Profile::IsolateRun => {
                argv.push("--read-only".to_string());
                argv.push("--cgroupns=private".to_string());
                argv.push("--cap-add=SYS_ADMIN".to_string());
                argv.push("--security-opt".to_string());
                argv.push("unmask=/sys/fs/cgroup".to_string());
                argv.push("--tmpfs".to_string());
                argv.push("/usr/local/etc".to_string());
                argv.push("--tmpfs".to_string());
                argv.push("/var/local/lib/isolate".to_string());
                argv.push("--tmpfs".to_string());
                argv.push("/run/isolate".to_string());
            }
        }

        if let Some(profile) = &self.seccomp_profile {
            argv.push("--security-opt".to_string());
            argv.push(format!("seccomp={}", profile.display()));
        }

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

        argv.push("-w".to_string());
        argv.push(spec.workdir.display().to_string());

        argv.push(self.base_image.clone());
        // The wrapper lives only inside the container image, so it's
        // applied here rather than spliced into `spec.program`/`args`
        // (`LocalSandbox` would then try to exec it and fail).
        if let Profile::IsolateRun = spec.profile {
            argv.push(ISOLATE_SETUP_WRAPPER.to_string())
        }
        argv.push(spec.command.clone());
        argv.extend(spec.args.iter().cloned());

        argv
    }
}

impl Sandbox for ContainerSandbox {
    /// Runs `podman version` (is podman usable at all?) and `podman image
    /// exists` (was the base image ever built, without a registry round
    /// trip?) once before any student is touched.
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
                "container base image {:?} was not found locally",
                self.base_image
            ))),
            Err(source) => Err(Error::Other(format!(
                "failed to check for container base image {:?}: {source}",
                self.base_image
            ))),
        }
    }

    fn provides_isolate(&self) -> bool {
        true
    }

    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome> {
        // `--cidfile` gives us a way to address the container after the
        // fact: conmon detaches from the `podman run` client process by
        // design, so on timeout killing that client's pid does not stop
        // the container. `tempdir()` (rather than a pre-created file) is
        // required because `podman run --cidfile` refuses to start if the
        // path already exists; dropping the dir at the end of this
        // function also cleans up the cidfile.
        let cidfile_dir = fs::temp_dir()?;
        let cidfile_path = cidfile_dir.path().join("cid");

        let argv = self.build_argv(spec, &cidfile_path);
        let exec = Exec::cmd(&self.podman_bin).args(&argv);

        let outcome = exec_with_timeout(
            exec,
            spec.limits.as_ref().map(|l| l.wall_clock),
            spec.limits.as_ref().map(|l| l.max_output_bytes as usize),
        )?;

        if outcome.status == ProcessStatus::TimedOut {
            kill_container_by_cidfile(&self.podman_bin, &cidfile_path);
        }

        Ok(SandboxOutcome {
            status: decode_podman_status(outcome.status),
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            wall_clock_ms: Some(outcome.wall_clock.as_millis() as u64),
        })
    }
}

/// Reverses podman's own convention for reporting a signal-killed
/// *contained* process: `podman run` itself exits normally with `128 +
/// signal` rather than being signaled itself.
fn decode_podman_status(raw: ProcessStatus) -> ProcessStatus {
    match raw {
        ProcessStatus::Exited(code) if code > 128 => ProcessStatus::Signaled(code - 128),
        other => other,
    }
}

/// Stops and removes the container identified by `cidfile` -- run on
/// timeout instead of killing the already-detached `podman run` client.
/// Uses `kill`, not `stop`, to skip the graceful SIGTERM-then-wait step.
/// Best-effort: a container that already exited, or a cidfile never
/// written because `podman run` failed first, are expected and ignored.
fn kill_container_by_cidfile(podman_bin: &str, cidfile: &Path) {
    let cidfile_arg = format!("--cidfile={}", cidfile.display());

    let kill = Exec::cmd(podman_bin).args(["kill", &cidfile_arg]);
    match exec_with_timeout(kill, Some(CONTAINER_KILL_TIMEOUT), Some(0)) {
        Ok(outcome) if outcome.status == ProcessStatus::TimedOut => {
            tracing::warn!(cidfile = %cidfile.display(), "`podman kill --cidfile` itself timed out");
        }
        _ => {}
    }

    let rm = Exec::cmd(podman_bin).args(["rm", "-f", &cidfile_arg]);
    match exec_with_timeout(rm, Some(CONTAINER_KILL_TIMEOUT), Some(0)) {
        Ok(outcome) if outcome.status == ProcessStatus::TimedOut => {
            tracing::warn!(cidfile = %cidfile.display(), "`podman rm -f --cidfile` itself timed out");
        }
        _ => {}
    }
}

/// Looks for an operator-provided seccomp profile
fn discover_seccomp_profile() -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(DEFAULT_SECCOMP_PROFILE);
    if path.is_file() {
        Some(path)
    } else {
        tracing::warn!(
            path = DEFAULT_SECCOMP_PROFILE,
            "no seccomp profile found; falling back to podman's built-in default profile"
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::sandbox::{Mount, SandboxLimits};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            command: "/judge.sh".to_string(),
            args: vec![],
            limits: Some(SandboxLimits {
                wall_clock: Duration::from_secs(10),
                cpus: 2,
                memory_bytes: 2 * 1024 * 1024 * 1024,
                pids: 256,
                max_output_bytes: 1024 * 1024,
            }),
            workdir: PathBuf::from("/repo"),
            env: std::collections::BTreeMap::new(),
            mounts: vec![
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
            ],
            profile: Profile::Build,
        }
    }

    fn sandbox() -> ContainerSandbox {
        let mut sandbox = ContainerSandbox::new("autograder-base:hw3");
        sandbox.seccomp_profile = Some(PathBuf::from("/etc/autograder/seccomp.json"));
        sandbox
    }

    fn cidfile() -> PathBuf {
        PathBuf::from("/tmp/autograder-test-cid")
    }

    #[test]
    fn argv_contains_the_design_mandated_isolation_flags() {
        let argv = sandbox().build_argv(&spec(), &cidfile());
        let joined = argv.join(" ");

        assert!(joined.contains("--cidfile=/tmp/autograder-test-cid"));
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
    fn no_seccomp_flag_when_profile_is_none() {
        let mut sandbox = sandbox();
        sandbox.seccomp_profile = None;
        let argv = sandbox.build_argv(&spec(), &cidfile());
        assert!(!argv.iter().any(|a| a.starts_with("seccomp=")));
        assert!(argv.contains(&"no-new-privileges".to_string()));
    }

    #[test]
    fn program_and_args_are_the_final_argv_entries() {
        let mut s = spec();
        s.args = vec!["--flag".into()];
        let argv = sandbox().build_argv(&s, &cidfile());
        assert_eq!(argv[argv.len() - 2], "/judge.sh");
        assert_eq!(argv[argv.len() - 1], "--flag");
    }

    #[test]
    fn none_limits_omits_resource_flags_but_keeps_other_isolation_flags() {
        let mut spec = spec();
        spec.limits = None;
        let argv = sandbox().build_argv(&spec, &cidfile());
        let joined = argv.join(" ");

        assert!(!joined.contains("--memory"));
        assert!(!joined.contains("--cpus"));
        assert!(!joined.contains("--pids-limit"));
        assert!(joined.contains("--read-only"));
        assert!(joined.contains("--cap-drop=ALL"));
        assert!(joined.contains("no-new-privileges"));
    }

    #[test]
    fn a_build_profile_emits_no_isolate_flags() {
        let argv = sandbox().build_argv(&spec(), &cidfile());
        let joined = argv.join(" ");

        assert!(!joined.contains("--cgroupns"));
        assert!(!joined.contains("--cap-add"));
        assert!(!joined.contains("unmask="));
        assert!(!joined.contains("--tmpfs"));
        assert!(!argv.iter().any(|a| a == "isolate-setup.sh"));
        assert_eq!(argv[argv.len() - 1], "/judge.sh");
    }

    #[test]
    fn an_isolate_run_profile_drops_hardening_and_adds_isolate_flags_and_the_setup_wrapper() {
        let mut s = spec();
        s.profile = Profile::IsolateRun;
        let argv = sandbox().build_argv(&s, &cidfile());
        let joined = argv.join(" ");

        // The three build-stage hardening flags isolate can't tolerate
        // (see `Profile::IsolateRun`'s doc comment) are gone.
        assert!(!joined.contains("--cap-drop"));
        assert!(!joined.contains("no-new-privileges"));
        assert!(!joined.contains("--user"));

        assert!(joined.contains("--read-only"));
        assert!(joined.contains("--cgroupns=private"));
        assert!(joined.contains("--cap-add=SYS_ADMIN"));
        assert!(joined.contains("--security-opt unmask=/sys/fs/cgroup"));
        assert!(joined.contains("--tmpfs /usr/local/etc"));
        assert!(joined.contains("--tmpfs /var/local/lib/isolate"));
        assert!(joined.contains("--tmpfs /run/isolate"));

        // wrapper, then program, as the final two argv entries (spec()'s
        // args is empty).
        assert_eq!(argv[argv.len() - 2], "isolate-setup.sh");
        assert_eq!(argv[argv.len() - 1], "/judge.sh");
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
        let err = sandbox_with_bin("false").preflight().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not usable in this environment"));
    }

    #[test]
    fn preflight_succeeds_when_podman_runs_cleanly() {
        assert!(sandbox_with_bin("true").preflight().is_ok());
    }

    /// A fake `podman` that answers `version` successfully but reports
    /// every `image exists` check as "not found".
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
    }
}
