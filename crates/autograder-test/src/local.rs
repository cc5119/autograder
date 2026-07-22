use std::process::Command as Proc;

use crate::{Command, Outcome, Status, resolve_target_binary, run_process};

pub(crate) fn run(cmd: Command) -> Outcome {
    let _ = Proc::new("cargo")
        .args(["build", "-p", &cmd.target])
        .status();
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
