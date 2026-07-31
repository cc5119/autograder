pub mod container;
pub mod exec;
pub mod local;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::Result;

pub use container::ContainerSandbox;
pub use local::LocalSandbox;

/// Runs a command under resource limits and isolation. Impls:
/// `LocalSandbox` (limits-only, used wherever podman isn't available) and
/// `ContainerSandbox` (rootless podman shell-out).
pub trait Sandbox {
    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome>;

    /// Cheap up-front check that this sandbox is actually usable, run once
    /// before a batch rather than discovered per-student. `LocalSandbox`
    /// has nothing to check (it just runs a host process) and accepts the
    /// default no-op; `ContainerSandbox` overrides this to catch a broken
    /// Podman setup as one clear error instead of every student in the
    /// batch scoring a misleading `build_failed`.
    fn preflight(&self) -> Result<()> {
        Ok(())
    }

    /// Whether `isolate` is available to the judge's tests. Only the
    /// container image ships it.
    fn provides_isolate(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
}

/// A host directory made available at `container_path`. `LocalSandbox`
/// ignores mounts (the command already runs against the host filesystem);
/// `ContainerSandbox` turns each into a `-v host:container:ro|rw` flag.
#[derive(Debug, Clone)]
pub struct Mount {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub mode: MountMode,
}

#[derive(Debug, Clone)]
pub struct SandboxLimits {
    pub wall_clock: Duration,
    pub cpus: u32,
    pub memory_bytes: u64,
    pub pids: u32,
    pub max_output_bytes: u64,
}

/// A named, fixed bundle of `podman run` isolation flags -- read one
/// variant top to bottom in `ContainerSandbox::build_argv` and you see
/// everything a given kind of run gets, rather than reconstructing it from
/// several independently-toggled fields. `LocalSandbox` ignores this
/// entirely (there's no container to configure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Build stages (`cargo build -p <id|harness>`): never runs student
    /// code directly, only trusted `cargo`. Fully hardened.
    Build,
    /// The run stage (`cargo nextest run`), which nests `isolate` --
    /// `isolate` itself is what boxes the untrusted student process, and it
    /// needs to run as root and `chown`/`setuid` into its own per-box uid
    /// range to do that, which `Build`'s hardening forbids. Validated
    /// empirically in `spike/isolate-podman/` (`run.txt`, profile
    /// `prod-no-nnp-root-user-no-capdrop`): dropping `--cap-drop`,
    /// `no-new-privileges`, or `--user` alone each still fails
    /// (`Must be started as root` / `Cannot switch to root group` /
    /// `Cannot chown busy`); only dropping all three together, while
    /// keeping `--read-only` and adding `SYS_ADMIN` + the cgroup unmask +
    /// isolate's own writable scratch dirs, passes end to end.
    IsolateRun,
}

/// What to run and under what isolation/limits. `LocalSandbox` runs
/// `program`/`args` directly on the host (`mounts`/`profile` are no-ops
/// there, since there's no container to configure); `ContainerSandbox` maps
/// every field onto `podman run` flags.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workdir: PathBuf,
    pub mounts: Vec<Mount>,
    /// `None` means genuinely unbounded -- no `--memory`/`--cpus`/
    /// `--pids-limit`, no wall-clock timeout, no output cap.
    pub limits: Option<SandboxLimits>,
    pub profile: Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    /// Exited normally (or with a nonzero code) -- the exit code is real
    /// either way.
    Exited(i32),
    /// Killed by this signal.
    Signaled(i32),
    /// The wall-clock limit was hit before the process finished.
    TimedOut,
    /// Couldn't be determined (the process was never successfully reaped).
    Unknown,
}

impl ProcessStatus {
    /// Mirrors `autograder_test::Status::describe`.
    pub fn describe(&self) -> String {
        match self {
            ProcessStatus::Exited(code) => format!("exited ({code})"),
            ProcessStatus::Signaled(signal) => format!("signaled ({signal})"),
            ProcessStatus::TimedOut => "timed out".to_string(),
            ProcessStatus::Unknown => "unknown".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxOutcome {
    pub status: ProcessStatus,
    /// Captured stdout, capped at `limits.max_output_bytes`.
    pub stdout: Vec<u8>,
    /// Captured stderr, capped at `limits.max_output_bytes`.
    pub stderr: Vec<u8>,
    pub wall_clock_ms: Option<u64>,
}

impl SandboxOutcome {
    pub fn succeeded(&self) -> bool {
        self.status == ProcessStatus::Exited(0)
    }

    pub fn timed_out(&self) -> bool {
        self.status == ProcessStatus::TimedOut
    }
}
