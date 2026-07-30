use std::io;
use std::path::PathBuf;
use std::process::Command as Proc;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use subprocess::{Exec, Redirection};

use crate::{Command, Outcome, Status, cargo_bin_exe};

fn resolve_target_binary(target: &str) -> PathBuf {
    cargo_bin_exe(target).unwrap_or_else(|| target_dir().join("debug").join(target))
}

/// One `cargo metadata` call per process -- the target dir is a single
/// workspace-wide value, independent of which target is being resolved.
/// Only used locally: never called from `isolate`, which must not shell
/// out to cargo from inside the sandboxed run stage.
fn target_dir() -> &'static PathBuf {
    static TARGET_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TARGET_DIR.get_or_init(|| {
        let out = Proc::new("cargo")
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .output()
            .expect("cargo metadata");
        let meta: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("parse cargo metadata");
        PathBuf::from(meta["target_directory"].as_str().expect("target_directory"))
    })
}

/// Runs the target as a host child process, enforcing `cmd.wall_time` with
/// the same spawn/communicate/kill-on-timeout/reap pattern the main
/// autograder crate uses for its own local sandbox (`src/exec/sandbox/exec.rs`)
/// -- unlike `isolate`, nothing else here bounds how long the child can run.
pub(crate) fn run(cmd: Command) -> Outcome {
    ensure_built(&cmd.target, &cmd.features);
    let bin = resolve_target_binary(&cmd.target);

    let mut exec = Exec::cmd(&bin).args(&cmd.args).env_extend(&cmd.env);
    if let Some(c) = &cmd.cwd {
        exec = exec.cwd(c);
    }
    exec = match cmd.stdin {
        Some(data) => exec.stdin(data),
        None => exec.stdin(Redirection::Null),
    };

    let start = Instant::now();
    let mut job = exec
        .stdout(Redirection::Pipe)
        .stderr(Redirection::Pipe)
        .start()
        .expect("spawn");

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let read_result = job
        .communicate()
        .expect("attach to output")
        .limit_time(cmd.wall_time)
        .read_to(&mut stdout, &mut stderr);

    let timed_out = match read_result {
        Ok(()) => false,
        Err(e) if e.kind() == io::ErrorKind::TimedOut => true,
        Err(e) => panic!("failed reading output: {e}"),
    };

    if timed_out {
        let _ = job.kill(); // SIGKILL; no-op if already reaped
    }

    let exit_status = job
        .wait_timeout(Duration::from_secs(2))
        .expect("failed to reap");
    let wall_time = start.elapsed();

    let status = if timed_out {
        Status::TimedOut
    } else {
        match exit_status.and_then(|s| s.code()) {
            Some(code) => Status::Exited(code as i32),
            None => Status::Signaled(exit_status.and_then(|s| s.signal()).unwrap_or(0)),
        }
    };

    Outcome {
        status,
        stdout,
        stderr,
        wall_time,
        cpu_time: None,
        peak_memory_bytes: None,
    }
}

struct BuildResult {
    ok: bool,
    stderr: Vec<u8>,
}

/// Builds `target` (with `features`, if any) at most once per distinct
/// `(target, features)` pair per test binary, even across concurrently
/// running tests -- `cargo test` runs tests in threads, so without this
/// every test spawning the same target would race its own `cargo build`.
/// No-op if `cargo_bin_exe` already found it (cargo built it as a
/// prerequisite of this test binary, so it's already current -- with
/// whatever features workspace unification gave it, `features` is only
/// consulted for the separate-package `cargo build -p` fallback below).
/// Panics on build failure, once per caller, rather than silently
/// proceeding to spawn a stale or missing binary.
fn ensure_built(target: &str, features: &[String]) {
    if cargo_bin_exe(target).is_some() {
        return;
    }

    // Distinguishes builds of the same target under different feature
    // sets, so switching features between tests doesn't reuse a stale
    // cached result for what's really a different build.
    let key = if features.is_empty() {
        target.to_string()
    } else {
        format!("{target}+{}", features.join(","))
    };

    // A handful of distinct (target, features) pairs tops (one harness
    // realistically spawns `driver` and/or one student binary) -- a linear
    // scan is cheaper than a HashMap here and needs no lazy/random-seeded
    // initializer, so this can be a plain const `static`.
    static BUILDS: RwLock<Vec<(String, BuildResult)>> = RwLock::new(Vec::new());

    // Fast path: a shared read lock, cheap and contention-free among
    // readers -- what every call hits once `key` has been built.
    let cached = BUILDS
        .read()
        .unwrap()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, r)| (r.ok, r.stderr.clone()));

    // Slow path: only the first caller for a given key takes the
    // exclusive write lock and actually builds; re-check first in case
    // another thread already raced past the read-lock miss and built it.
    let (ok, stderr) = cached.unwrap_or_else(|| {
        let mut builds = BUILDS.write().unwrap();
        if let Some((_, r)) = builds.iter().find(|(k, _)| *k == key) {
            return (r.ok, r.stderr.clone());
        }
        let mut args = vec!["build".to_string(), "-p".to_string(), target.to_string()];
        if !features.is_empty() {
            args.push("--features".to_string());
            args.push(features.join(","));
        }
        let out = Proc::new("cargo")
            .args(&args)
            .output()
            .expect("spawn cargo build");
        let result = BuildResult {
            ok: out.status.success(),
            stderr: out.stderr,
        };
        let pair = (result.ok, result.stderr.clone());
        builds.push((key.clone(), result));
        pair
    }); // guard dropped here -- never panic while holding the lock.

    let features_suffix = if features.is_empty() {
        String::new()
    } else {
        format!(" --features {}", features.join(","))
    };
    assert!(
        ok,
        "cargo build -p {target}{features_suffix} failed:\n{}",
        String::from_utf8_lossy(&stderr)
    );
}
