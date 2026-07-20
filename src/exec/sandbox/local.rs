use std::process::Command;

use crate::error::Result;
use crate::model::ResourceUsage;

use super::exec::run_with_timeout;
use super::{Sandbox, SandboxOutcome, SandboxSpec};

/// Runs the command as a host child process with a wall-clock timeout and
/// output cap — best-effort limits, runner-isolated rather than
/// kernel-isolated. Mounts and the network flag are no-ops here (the
/// command already runs against the host filesystem/network); this is what
/// makes `LocalSandbox` usable without podman, so it's what the CI tier
/// uses (design §10, §11.3) and what M1/M2 can verify locally.
pub struct LocalSandbox;

impl Sandbox for LocalSandbox {
    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        cmd.envs(&spec.env);
        if let Some(dir) = &spec.workdir {
            cmd.current_dir(dir);
        }

        let outcome = run_with_timeout(
            cmd,
            spec.limits.wall_clock,
            spec.limits.max_output_bytes as usize,
        )?;

        Ok(SandboxOutcome {
            exit_code: outcome.exit_code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            timed_out: outcome.timed_out,
            // No cgroup accounting without a container runtime: OOM is
            // never detected locally, and only wall-clock is reported.
            oom: false,
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
    use crate::exec::sandbox::SandboxLimits;
    use std::time::Duration;

    fn limits(wall_clock: Duration) -> SandboxLimits {
        SandboxLimits {
            wall_clock,
            cpus: 1,
            memory_bytes: 512 * 1024 * 1024,
            pids: 64,
            max_output_bytes: 4096,
        }
    }

    #[test]
    fn runs_a_command_to_completion() {
        let spec = SandboxSpec::new("true", limits(Duration::from_secs(5)));
        let outcome = LocalSandbox.run(&spec).unwrap();

        assert!(outcome.succeeded());
        assert!(!outcome.timed_out);
    }

    #[test]
    fn kills_a_runaway_command_on_wall_clock_timeout() {
        let mut spec = SandboxSpec::new("sleep", limits(Duration::from_millis(150)));
        spec.args = vec!["5".into()];

        let outcome = LocalSandbox.run(&spec).unwrap();

        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
    }

    #[test]
    fn caps_captured_output() {
        let mut spec = SandboxSpec::new("sh", limits(Duration::from_secs(5)));
        spec.limits.max_output_bytes = 5;
        spec.args = vec!["-c".into(), "printf '0123456789'".into()];

        let outcome = LocalSandbox.run(&spec).unwrap();

        assert_eq!(outcome.stdout, b"01234");
    }
}
