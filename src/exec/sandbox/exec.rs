//! Child-process execution shared by `LocalSandbox` and `ContainerSandbox`,
//! built on the `subprocess` crate: both spawn a host child process, enforce
//! a wall-clock timeout, and cap captured output.

use std::io;
use std::time::{Duration, Instant};

use subprocess::{Exec, Redirection};

use crate::error::{Error, Result};

pub struct ExecOutcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub wall_clock: Duration,
}

/// Spawns `exec`, capturing stdout/stderr (a combined cap of
/// `max_output_bytes` across both streams) for up to `wall_clock`.
/// timeout the child is killed; the caller is responsible for any further
/// cleanup its own sandbox needs.
pub fn exec_with_timeout(
    exec: Exec,
    wall_clock: Duration,
    max_output_bytes: usize,
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
    let read_result = job
        .communicate()
        .map_err(|source| Error::Other(format!("failed to attach to output: {source}")))?
        .limit_time(wall_clock)
        .limit_size(max_output_bytes)
        .read_to(&mut stdout, &mut stderr);

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

    Ok(ExecOutcome {
        exit_code: status.and_then(|s| s.code()).map(|c| c as i32),
        stdout,
        stderr,
        timed_out,
        wall_clock: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_code() {
        let exec = Exec::cmd("sh").args(&["-c", "echo out; echo err >&2; exit 3"]);
        let outcome = exec_with_timeout(exec, Duration::from_secs(5), 1024).unwrap();

        assert_eq!(outcome.exit_code, Some(3));
        assert_eq!(outcome.stdout, b"out\n");
        assert_eq!(outcome.stderr, b"err\n");
        assert!(!outcome.timed_out);
    }

    #[test]
    fn kills_on_timeout() {
        let exec = Exec::cmd("sh").args(&["-c", "sleep 5"]);
        let outcome = exec_with_timeout(exec, Duration::from_millis(150), 1024).unwrap();

        assert!(outcome.timed_out);
        assert!(outcome.wall_clock < Duration::from_secs(2));
    }

    #[test]
    fn caps_combined_output_bytes() {
        let exec = Exec::cmd("sh").args(&["-c", "for i in $(seq 1 1000); do echo line$i; done"]);
        let outcome = exec_with_timeout(exec, Duration::from_secs(5), 10).unwrap();

        assert_eq!(outcome.stdout.len(), 10);
    }
}
