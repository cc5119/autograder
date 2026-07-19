pub mod container;
pub mod exec;
pub mod local;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::Result;
use crate::model::ResourceUsage;

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

/// What to run and under what isolation/limits. `LocalSandbox` runs
/// `program`/`args` directly on the host (mounts/network are no-ops there);
/// `ContainerSandbox` maps every field onto `podman run` flags.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<PathBuf>,
    pub mounts: Vec<Mount>,
    /// If false, network access is denied (`--network=none`).
    pub network: bool,
    pub limits: SandboxLimits,
}

impl SandboxSpec {
    pub fn new(program: impl Into<String>, limits: SandboxLimits) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            workdir: None,
            mounts: Vec::new(),
            network: false,
            limits,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxOutcome {
    pub exit_code: Option<i32>,
    /// Captured stdout, capped at `limits.max_output_bytes`.
    pub stdout: Vec<u8>,
    /// Captured stderr, capped at `limits.max_output_bytes`.
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub oom: bool,
    pub resource_usage: ResourceUsage,
}

impl SandboxOutcome {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && !self.oom && self.exit_code == Some(0)
    }
}
