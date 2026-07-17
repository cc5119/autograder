//! Child-process execution shared by `LocalSandbox` (runs `spec.program`
//! directly) and `ContainerSandbox` (runs `podman` with a constructed argv):
//! both are, at bottom, "spawn a host child process, enforce a wall-clock
//! timeout, and cap captured output" — the only difference is which argv is
//! built from the `SandboxSpec`.

use std::io::Read;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub struct ExecOutcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub wall_clock: Duration,
}

/// Spawns `cmd`, waits up to `wall_clock`, and captures stdout/stderr each
/// capped at `max_output_bytes`. Kills the child on timeout.
pub fn run_with_timeout(
    mut cmd: Command,
    wall_clock: Duration,
    max_output_bytes: usize,
) -> Result<ExecOutcome> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let program = format!("{:?}", cmd);
    let mut child: Child = cmd.spawn().map_err(|source| Error::Other(format!(
        "failed to spawn {program}: {source}"
    )))?;

    let stdout: ChildStdout = child.stdout.take().expect("piped stdout");
    let stderr: ChildStderr = child.stderr.take().expect("piped stderr");
    let stdout_handle = thread::spawn(move || read_capped(stdout, max_output_bytes));
    let stderr_handle = thread::spawn(move || read_capped(stderr, max_output_bytes));

    let start = Instant::now();
    let mut timed_out = false;
    loop {
        if let Some(_status) = child
            .try_wait()
            .map_err(|source| Error::Other(format!("failed to poll child: {source}")))?
        {
            break;
        }
        if start.elapsed() >= wall_clock {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let wall_clock_elapsed = start.elapsed();

    let status = child
        .wait()
        .map_err(|source| Error::Other(format!("failed to reap child: {source}")))?;
    let stdout = stdout_handle.join().expect("stdout reader thread panicked");
    let stderr = stderr_handle.join().expect("stderr reader thread panicked");

    Ok(ExecOutcome {
        exit_code: status.code(),
        stdout,
        stderr,
        timed_out,
        wall_clock: wall_clock_elapsed,
    })
}

/// Reads `source` to completion but stops accumulating once `cap` bytes have
/// been read, so a chatty/adversarial child can't force unbounded memory use
/// (design §10, `max-output-bytes`). Still drains the pipe past the cap so
/// the child doesn't block writing to a full pipe buffer.
fn read_capped<R: Read>(mut source: R, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match source.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let remaining = cap - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(remaining)]);
                }
            }
            Err(_) => break,
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_code() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let outcome = run_with_timeout(cmd, Duration::from_secs(5), 1024).unwrap();

        assert_eq!(outcome.exit_code, Some(3));
        assert_eq!(outcome.stdout, b"out\n");
        assert_eq!(outcome.stderr, b"err\n");
        assert!(!outcome.timed_out);
    }

    #[test]
    fn kills_on_timeout() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5"]);
        let outcome = run_with_timeout(cmd, Duration::from_millis(150), 1024).unwrap();

        assert!(outcome.timed_out);
        assert!(outcome.wall_clock < Duration::from_secs(2));
    }

    #[test]
    fn caps_output_bytes() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "for i in $(seq 1 1000); do echo line$i; done"]);
        let outcome = run_with_timeout(cmd, Duration::from_secs(5), 10).unwrap();

        assert_eq!(outcome.stdout.len(), 10);
    }
}
