//! Child-process execution shared by `LocalSandbox` and `ContainerSandbox`,
//! built on the `subprocess` crate: both spawn a host child process, enforce
//! a wall-clock timeout, and cap captured output.

use std::io;
use std::time::{Duration, Instant};

use subprocess::{Exec, ExitStatus, Redirection};

use crate::error::{Error, Result};

use super::ProcessStatus;

pub struct ExecOutcome {
    /// Never `MemoryExceeded`
    pub status: ProcessStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall_clock: Duration,
}

fn process_status(status: Option<ExitStatus>) -> ProcessStatus {
    let Some(status) = status else {
        return ProcessStatus::Unknown;
    };
    if let Some(code) = status.code() {
        ProcessStatus::Exited(code as i32)
    } else if let Some(signal) = status.signal() {
        ProcessStatus::Signaled(signal)
    } else {
        ProcessStatus::Unknown
    }
}

/// Spawns `exec`, capturing stdout/stderr (a combined cap of
/// `max_output_bytes` across both streams, when given) for up to
/// `wall_clock` (when given). `None` for either means genuinely unbounded --
/// used by stages that carry no `SandboxLimits` at all (the run stage
/// relies on `isolate` instead). On timeout the child is killed; the caller
/// is responsible for any further cleanup its own sandbox needs.
pub fn exec_with_timeout(
    exec: Exec,
    wall_clock: Option<Duration>,
    max_output_bytes: Option<usize>,
) -> Result<ExecOutcome> {
    let start = Instant::now();

    let mut job = exec
        .stdin(Redirection::Null)
        .stdout(Redirection::Pipe)
        .stderr(Redirection::Pipe)
        .start()
        .map_err(|source| Error::Other(format!("failed to spawn: {source}")))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut communicator = job
        .communicate()
        .map_err(|source| Error::Other(format!("failed to attach to output: {source}")))?;
    if let Some(wall_clock) = wall_clock {
        communicator = communicator.limit_time(wall_clock);
    }
    if let Some(max_output_bytes) = max_output_bytes {
        communicator = communicator.limit_size(max_output_bytes);
    }
    let read_result = communicator.read_to(&mut stdout, &mut stderr);

    let timed_out = match read_result {
        Ok(()) => false,
        Err(source) if source.kind() == io::ErrorKind::TimedOut => true,
        Err(source) => return Err(Error::Other(format!("failed reading output: {source}"))),
    };

    if timed_out {
        let _ = job.kill(); // SIGKILL; no-op if already reaped
    }

    let status = job
        .wait_timeout(Duration::from_secs(2))
        .map_err(|source| Error::Other(format!("failed to reap: {source}")))?;

    let status = if timed_out {
        ProcessStatus::TimedOut
    } else {
        process_status(status)
    };

    Ok(ExecOutcome {
        status,
        stdout,
        stderr,
        wall_clock: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_code() {
        let exec = Exec::cmd("sh").args(&["-c", "echo out; echo err >&2; exit 3"]);
        let outcome = exec_with_timeout(exec, Some(Duration::from_secs(5)), Some(1024)).unwrap();

        assert_eq!(outcome.status, ProcessStatus::Exited(3));
        assert_eq!(outcome.stdout, b"out\n");
        assert_eq!(outcome.stderr, b"err\n");
    }

    #[test]
    fn kills_on_timeout() {
        let exec = Exec::cmd("sh").args(&["-c", "sleep 5"]);
        let outcome =
            exec_with_timeout(exec, Some(Duration::from_millis(150)), Some(1024)).unwrap();

        assert_eq!(outcome.status, ProcessStatus::TimedOut);
        assert!(outcome.wall_clock < Duration::from_secs(2));
    }

    #[test]
    fn caps_combined_output_bytes() {
        let exec = Exec::cmd("sh").args(&["-c", "for i in $(seq 1 1000); do echo line$i; done"]);
        let outcome = exec_with_timeout(exec, Some(Duration::from_secs(5)), Some(10)).unwrap();

        assert_eq!(outcome.stdout.len(), 10);
    }

    #[test]
    fn none_wall_clock_and_output_cap_run_unbounded() {
        let exec = Exec::cmd("sh").args(&["-c", "echo out; exit 0"]);
        let outcome = exec_with_timeout(exec, None, None).unwrap();

        assert_eq!(outcome.status, ProcessStatus::Exited(0));
        assert_eq!(outcome.stdout, b"out\n");
    }
}
