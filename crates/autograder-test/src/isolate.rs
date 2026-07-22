use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{Command, Outcome, Ran, Status, resolve_target_binary, run_process};

pub(crate) fn run(cmd: Command) -> Outcome {
    let bin = resolve_target_binary(&cmd.target);
    let box_id = slot();
    let box_path = init_box(box_id);
    std::fs::copy(&bin, box_path.join("box").join(&cmd.target)).expect("copy binary into box");

    let meta_path =
        std::env::temp_dir().join(format!("isolate-meta-{}-{}", box_id, std::process::id()));

    let mut argv = vec![
        "--cg".to_string(),
        format!("--box-id={box_id}"),
        "--share-net".to_string(),
        format!("--meta={}", meta_path.display()),
    ];
    if let Some(m) = cmd.memory_bytes {
        argv.push(format!("--cg-mem={}", m / 1024));
    }
    if let Some(t) = cmd.cpu_time {
        argv.push(format!("--time={}", t.as_secs()));
    }
    argv.push(format!("--wall-time={}", cmd.wall_time.as_secs()));
    argv.push("--run".to_string());
    argv.push("--".to_string());
    argv.push(format!("./{}", cmd.target));
    argv.extend(cmd.args.iter().cloned());

    let mut proc = std::process::Command::new("isolate");
    proc.args(&argv);
    for (k, v) in &cmd.env {
        proc.env(k, v);
    }

    let r = run_process(proc, cmd.stdin);
    let meta = read_meta(&meta_path);
    cleanup_box(box_id);

    build_outcome(r, meta)
}

fn slot() -> u32 {
    std::env::var("NEXTEST_TEST_GLOBAL_SLOT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn init_box(id: u32) -> PathBuf {
    let out = std::process::Command::new("isolate")
        .args(["--cg", &format!("--box-id={id}"), "--init"])
        .output()
        .expect("isolate --init");
    assert!(
        out.status.success(),
        "isolate --init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn cleanup_box(id: u32) {
    let _ = std::process::Command::new("isolate")
        .args(["--cg", &format!("--box-id={id}"), "--cleanup"])
        .status();
}

fn read_meta(path: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once(':') {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    let _ = std::fs::remove_file(path);
    map
}

fn build_outcome(r: Ran, meta: BTreeMap<String, String>) -> Outcome {
    let peak = meta
        .get("cg-mem")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|kib| kib * 1024);
    let cpu = meta
        .get("time")
        .and_then(|v| v.parse::<f64>().ok())
        .map(Duration::from_secs_f64);
    let exit_code = meta
        .get("exitcode")
        .and_then(|v| v.parse::<i32>().ok())
        .or(r.exit_code);

    let oom = meta.get("cg-oom-killed").map(|v| v == "1").unwrap_or(false);
    let status = match meta.get("status").map(String::as_str) {
        Some("TO") => Status::TimedOut,
        Some("SG") if oom => Status::MemoryExceeded,
        Some("SG") => Status::Signaled(
            meta.get("exitsig")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        ),
        Some("RE") | Some("XX") => Status::Failed,
        _ if exit_code == Some(0) => Status::Ok,
        _ => Status::Failed,
    };

    Outcome {
        status,
        exit_code,
        stdout: r.stdout,
        stderr: r.stderr,
        wall_time: r.wall,
        cpu_time: cpu,
        peak_memory_bytes: peak,
    }
}
