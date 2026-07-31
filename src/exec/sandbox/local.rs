use subprocess::Exec;

use crate::error::Result;

use super::exec::exec_with_timeout;
use super::{Sandbox, SandboxOutcome, SandboxSpec};

/// Runs the command as a host child process with a wall-clock timeout and
/// output cap — best-effort limits, runner-isolated rather than
/// kernel-isolated. `mounts`/`profile` are no-ops here (the command already
/// runs against the host filesystem/network); this is what makes
/// `LocalSandbox` usable without podman, so it's what the CI tier uses and
/// what M1/M2 can verify locally.
pub struct LocalSandbox;

impl Sandbox for LocalSandbox {
    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome> {
        let exec = Exec::cmd(&spec.command)
            .args(&spec.args)
            .env_extend(&spec.env)
            .cwd(&spec.workdir);

        let outcome = exec_with_timeout(
            exec,
            spec.limits.as_ref().map(|l| l.wall_clock),
            spec.limits.as_ref().map(|l| l.max_output_bytes as usize),
        )?;

        Ok(SandboxOutcome {
            status: outcome.status,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            wall_clock_ms: Some(outcome.wall_clock.as_millis() as u64),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::sandbox::{Profile, SandboxLimits};
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

    fn spec(command: &str, args: Vec<String>, limits: Option<SandboxLimits>) -> SandboxSpec {
        SandboxSpec {
            command: command.to_string(),
            args,
            limits,
            workdir: std::env::current_dir().unwrap(),
            env: Default::default(),
            mounts: Vec::new(),
            profile: Profile::Build,
        }
    }

    #[test]
    fn runs_a_command_to_completion() {
        let spec = spec("true", Vec::new(), Some(limits(Duration::from_secs(5))));
        let outcome = LocalSandbox.run(&spec).unwrap();

        assert!(outcome.succeeded());
        assert!(!outcome.timed_out());
    }

    #[test]
    fn kills_a_runaway_command_on_wall_clock_timeout() {
        let spec = spec(
            "sleep",
            vec!["5".into()],
            Some(limits(Duration::from_millis(150))),
        );

        let outcome = LocalSandbox.run(&spec).unwrap();

        assert!(outcome.timed_out());
        assert!(!outcome.succeeded());
    }

    #[test]
    fn caps_captured_output() {
        let mut spec = spec(
            "sh",
            vec!["-c".into(), "printf '0123456789'".into()],
            Some(limits(Duration::from_secs(5))),
        );
        spec.limits.as_mut().unwrap().max_output_bytes = 5;

        let outcome = LocalSandbox.run(&spec).unwrap();

        assert_eq!(outcome.stdout, b"01234");
    }

    #[test]
    fn none_limits_runs_with_no_timeout_or_output_cap() {
        let spec = spec("true", Vec::new(), None);
        let outcome = LocalSandbox.run(&spec).unwrap();

        assert!(outcome.succeeded());
        assert!(!outcome.timed_out());
    }
}
