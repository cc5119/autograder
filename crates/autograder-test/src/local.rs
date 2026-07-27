use std::process::Command as Proc;
use std::sync::RwLock;

use crate::{Command, Outcome, Status, cargo_bin_exe, resolve_target_binary, run_process};

pub(crate) fn run(cmd: Command) -> Outcome {
    ensure_built(&cmd.target);
    let bin = resolve_target_binary(&cmd.target);
    let mut proc = Proc::new(&bin);
    proc.args(&cmd.args);
    if let Some(c) = &cmd.cwd {
        proc.current_dir(c);
    }
    for (k, v) in &cmd.env {
        proc.env(k, v);
    }

    let r = run_process(proc, cmd.stdin);
    let status = if r.exit_code == Some(0) {
        Status::Ok
    } else {
        Status::Failed
    };
    Outcome {
        status,
        exit_code: r.exit_code,
        stdout: r.stdout,
        stderr: r.stderr,
        wall_time: r.wall,
        cpu_time: None,
        peak_memory_bytes: None,
    }
}

struct BuildResult {
    ok: bool,
    stderr: Vec<u8>,
}

/// Builds `target` at most once per test binary, even across concurrently
/// running tests -- `cargo test` runs tests in threads, so without this
/// every test spawning the same target would race its own `cargo build`.
/// No-op if `cargo_bin_exe` already found it (cargo built it as a
/// prerequisite of this test binary, so it's already current). Panics on
/// build failure, once per caller, rather than silently proceeding to spawn
/// a stale or missing binary.
fn ensure_built(target: &str) {
    if cargo_bin_exe(target).is_some() {
        return;
    }

    // A handful of distinct targets tops (one harness realistically spawns
    // `driver` and/or one student binary) -- a linear scan is cheaper than
    // a HashMap here and needs no lazy/random-seeded initializer, so this
    // can be a plain const `static`.
    static BUILDS: RwLock<Vec<(String, BuildResult)>> = RwLock::new(Vec::new());

    // Fast path: a shared read lock, cheap and contention-free among
    // readers -- what every call hits once `target` has been built.
    let cached = BUILDS
        .read()
        .unwrap()
        .iter()
        .find(|(t, _)| t == target)
        .map(|(_, r)| (r.ok, r.stderr.clone()));

    // Slow path: only the first caller for a given target takes the
    // exclusive write lock and actually builds; re-check first in case
    // another thread already raced past the read-lock miss and built it.
    let (ok, stderr) = cached.unwrap_or_else(|| {
        let mut builds = BUILDS.write().unwrap();
        if let Some((_, r)) = builds.iter().find(|(t, _)| t == target) {
            return (r.ok, r.stderr.clone());
        }
        let out = Proc::new("cargo")
            .args(["build", "-p", target])
            .output()
            .expect("spawn cargo build");
        let result = BuildResult {
            ok: out.status.success(),
            stderr: out.stderr,
        };
        let pair = (result.ok, result.stderr.clone());
        builds.push((target.to_string(), result));
        pair
    }); // guard dropped here -- never panic while holding the lock.

    assert!(
        ok,
        "cargo build -p {target} failed:\n{}",
        String::from_utf8_lossy(&stderr)
    );
}
