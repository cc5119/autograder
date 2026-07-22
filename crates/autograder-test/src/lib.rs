mod isolate;
mod local;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as Proc, Stdio};
use std::time::{Duration, Instant};

pub fn bin(name: impl Into<String>) -> Command {
    Command {
        target: name.into(),
        args: Vec::new(),
        stdin: None,
        env: BTreeMap::new(),
        cwd: None,
        memory_bytes: None,
        cpu_time: None,
        wall_time: Duration::from_secs(10),
    }
}

pub struct Command {
    target: String,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    memory_bytes: Option<u64>,
    cpu_time: Option<Duration>,
    wall_time: Duration,
}

impl Command {
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for a in it {
            self.args.push(a.into());
        }
        self
    }

    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }

    pub fn cwd(mut self, p: impl Into<PathBuf>) -> Self {
        self.cwd = Some(p.into());
        self
    }

    pub fn memory_mib(mut self, mib: u64) -> Self {
        self.memory_bytes = Some(mib * 1024 * 1024);
        self
    }

    pub fn cpu_time_secs(mut self, s: u64) -> Self {
        self.cpu_time = Some(Duration::from_secs(s));
        self
    }

    pub fn wall_time_secs(mut self, s: u64) -> Self {
        self.wall_time = Duration::from_secs(s);
        self
    }

    pub fn run(self) -> Outcome {
        if grading() {
            isolate::run(self)
        } else {
            local::run(self)
        }
    }
}

pub enum Status {
    Ok,
    TimedOut,
    MemoryExceeded,
    Signaled(i32),
    Failed,
}

pub struct Outcome {
    pub status: Status,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall_time: Duration,
    pub cpu_time: Option<Duration>,
    pub peak_memory_bytes: Option<u64>,
}

impl Outcome {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    pub fn success(&self) -> bool {
        matches!(self.status, Status::Ok) && self.exit_code == Some(0)
    }

    pub fn timed_out(&self) -> bool {
        matches!(self.status, Status::TimedOut)
    }

    pub fn oom(&self) -> bool {
        matches!(self.status, Status::MemoryExceeded)
    }
}

fn grading() -> bool {
    std::env::var("AUTOGRADER_SANDBOX").as_deref() == Ok("isolate")
}

pub(crate) fn resolve_target_binary(target: &str) -> PathBuf {
    target_dir().join("debug").join(target)
}

fn target_dir() -> PathBuf {
    let out = Proc::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .expect("cargo metadata");
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("parse cargo metadata");
    PathBuf::from(meta["target_directory"].as_str().expect("target_directory"))
}

pub(crate) struct Ran {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall: Duration,
}

pub(crate) fn run_process(mut cmd: Proc, stdin: Option<Vec<u8>>) -> Ran {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn");
    let writer = stdin.map(|data| {
        let mut sink = child.stdin.take().expect("stdin pipe");
        std::thread::spawn(move || {
            let _ = sink.write_all(&data);
        })
    });
    let out = child.wait_with_output().expect("wait");
    if let Some(w) = writer {
        let _ = w.join();
    }

    Ran {
        exit_code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
        wall: start.elapsed(),
    }
}
