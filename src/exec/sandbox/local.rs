use subprocess::Exec;

use crate::error::Result;

use super::exec::exec_with_timeout;
use super::{Sandbox, SandboxOutcome, SandboxSpec};

/// Runs the command as a host child process with a wall-clock timeout and
/// output cap — best-effort limits, runner-isolated rather than
/// kernel-isolated. Mounts and the network flag are no-ops here (the
/// command already runs against the host filesystem/network); this is what
/// makes `LocalSandbox` usable without podman, so it's what the CI tier
/// uses and what M1/M2 can verify locally.
pub struct LocalSandbox;

impl Sandbox for LocalSandbox {
    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome> {
        let mut exec = Exec::cmd(&spec.program)
            .args(&spec.args)
            .env_extend(&spec.env);
        if let Some(dir) = &spec.workdir {
            exec = exec.cwd(dir);
        }

        let outcome = exec_with_timeout(
            exec,
            spec.limits.as_ref().map(|l| l.wall_clock),
            spec.limits.as_ref().map(|l| l.max_output_bytes as usize),
        )?;

        Ok(SandboxOutcome {
            exit_code: outcome.exit_code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            timed_out: outcome.timed_out,
            // No cgroup accounting without a container runtime: OOM is
            // never detected locally, and only wall-clock is reported.
            oom: false,
            cpu_ms: Some(outcome.wall_clock.as_millis() as u64),
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
        let spec = SandboxSpec::new("true", Some(limits(Duration::from_secs(5))));
        let outcome = LocalSandbox.run(&spec).unwrap();

        assert!(outcome.succeeded());
        assert!(!outcome.timed_out);
    }

    #[test]
    fn kills_a_runaway_command_on_wall_clock_timeout() {
        let mut spec = SandboxSpec::new("sleep", Some(limits(Duration::from_millis(150))));
        spec.args = vec!["5".into()];

        let outcome = LocalSandbox.run(&spec).unwrap();

        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
    }

    #[test]
    fn caps_captured_output() {
        let mut spec = SandboxSpec::new("sh", Some(limits(Duration::from_secs(5))));
        spec.limits.as_mut().unwrap().max_output_bytes = 5;
        spec.args = vec!["-c".into(), "printf '0123456789'".into()];

        let outcome = LocalSandbox.run(&spec).unwrap();

        assert_eq!(outcome.stdout, b"01234");
    }

    #[test]
    fn none_limits_runs_with_no_timeout_or_output_cap() {
        let spec = SandboxSpec::new("true", None);
        let outcome = LocalSandbox.run(&spec).unwrap();

        assert!(outcome.succeeded());
        assert!(!outcome.timed_out);
    }
}
