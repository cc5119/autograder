mod isolate;
mod local;
mod reader;

pub use reader::{Reader, Scope};

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

#[macro_export]
macro_rules! score {
    ($score:expr) => {
        let score: f64 = $score;
        println!("autograder: score={score}");
    };
}

/// Builds a [`Command`] for `target`, optionally forwarding a subset of
/// *this* crate's own Cargo features to `target`'s build (only meaningful
/// locally -- see [`Command::features`]) so a harness/target pair sharing a
/// feature name (e.g. `student`) stays in sync without the caller having to
/// spell out `cfg!(feature = "...")` themselves:
///
/// ```ignore
/// autograder_test::cmd!("fizzbuzz", features = ["student"]).arg(n).run()
/// ```
#[macro_export]
macro_rules! cmd {
    ($target:expr, features = [$($name:literal),* $(,)?]) => {{
        let mut enabled: Vec<&'static str> = Vec::new();
        $( if cfg!(feature = $name) { enabled.push($name); } )*
        $crate::Command::new($target).features(enabled)
    }};
    ($target:expr) => {
        $crate::Command::new($target)
    };
}

/// The process's command-line arguments, parsed on demand -- for a
/// `library`-kind driver that takes its inputs positionally:
///
/// ```ignore
/// let mut args = autograder_test::Args::new();
/// let n: usize = args.read();
/// let [lo, hi]: [u32; 2] = args.take_arr();
/// let rest: Vec<String> = args.rest();
/// ```
///
/// Every method panics on a missing or unparsable argument, which is the
/// right shape for a harness: bad arguments are a bug in the test, not a
/// student failure to report.
pub struct Args {
    inner: std::env::Args,
}

impl Default for Args {
    fn default() -> Self {
        Self::new()
    }
}

impl Args {
    pub fn new() -> Self {
        let mut inner = std::env::args();
        // consume the command
        let _ = inner.next();
        Self { inner }
    }

    /// The next argument.
    pub fn read<T: FromStr<Err: Debug>>(&mut self) -> T {
        let arg = self.inner.next().expect("no more arguments");
        arg.parse().unwrap()
    }

    /// Every remaining argument.
    pub fn rest<T: FromStr<Err: Debug>>(self) -> Vec<T> {
        self.inner.map(|s| s.parse().unwrap()).collect()
    }

    /// Exactly `n` arguments, leaving any later ones for the caller.
    pub fn take<T: FromStr<Err: Debug>>(&mut self, n: usize) -> Vec<T> {
        (0..n).map(|_| self.read()).collect()
    }

    /// Exactly `N` arguments, leaving any later ones for the caller.
    pub fn take_arr<const N: usize, T: FromStr<Err: Debug>>(&mut self) -> [T; N] {
        // `from_fn` calls in order of increasing index, so the arguments
        // land in the array in the order they were given.
        std::array::from_fn(|_| self.read())
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
    features: Vec<String>,
}

impl Command {
    pub fn new(target: impl ToString) -> Self {
        Command {
            target: target.to_string(),
            args: Vec::new(),
            stdin: None,
            env: BTreeMap::new(),
            cwd: None,
            memory_bytes: Some(256 * 1024 * 1024),
            cpu_time: None,
            wall_time: Duration::from_secs(3),
            features: Vec::new(),
        }
    }

    /// Cargo features to pass when building `target` locally (`cargo build
    /// -p <target> --features ...`) -- a no-op when grading, since the run
    /// stage never builds anything, only spawns the already-built binary.
    pub fn features<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.features = it.into_iter().map(Into::into).collect();
        self
    }

    pub fn arg(mut self, a: impl ToString) -> Self {
        self.args.push(a.to_string());
        self
    }

    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: ToString,
    {
        for a in it {
            self.args.push(a.to_string());
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
    /// Program ran to completion with this exit code -- covers both a
    /// clean exit and isolate's "RE" (nonzero exit), since either way the
    /// code is real.
    Exited(i32),
    /// Killed by a signal (isolate "SG" without an OOM kill).
    Signaled(i32),
    /// Wall/cpu time limit hit before the program finished (isolate "TO").
    TimedOut,
    /// Killed by the cgroup for exceeding the memory limit (isolate "SG"
    /// with `cg-oom-killed:1`).
    MemoryExceeded,
    /// The sandbox itself failed to run the job -- not something the
    /// student's binary did, and there is no exit code because it may
    /// never have started (isolate "XX"; the string is isolate's own
    /// `message` explaining the failure). Only produced when grading.
    SandboxError(String),
}

impl Status {
    fn describe(&self) -> String {
        match self {
            Status::Exited(code) => format!("exited ({code})"),
            Status::Signaled(sig) => format!("signaled ({sig})"),
            Status::TimedOut => "timed out".to_string(),
            Status::MemoryExceeded => "memory exceeded".to_string(),
            Status::SandboxError(msg) => format!("sandbox error: {msg}"),
        }
    }
}

pub struct Outcome {
    pub status: Status,
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

    /// Some(code) only when the program actually ran to completion.
    pub fn exit_code(&self) -> Option<i32> {
        match self.status {
            Status::Exited(code) => Some(code),
            _ => None,
        }
    }

    pub fn success(&self) -> bool {
        matches!(self.status, Status::Exited(0))
    }

    pub fn timed_out(&self) -> bool {
        matches!(self.status, Status::TimedOut)
    }

    pub fn oom(&self) -> bool {
        matches!(self.status, Status::MemoryExceeded)
    }

    /// A cursor over stdout, tokenized on whitespace and parsed on demand.
    pub fn stdout_reader(&self) -> Reader {
        Reader::new(self.stdout_str())
    }

    /// A cursor over stderr, tokenized on whitespace and parsed on demand.
    pub fn stderr_reader(&self) -> Reader {
        Reader::new(self.stderr_str())
    }

    /// Asserts stdout matches `expected`, ignoring trailing whitespace on
    /// each line and any trailing blank lines. Panics naming the first
    /// line that differs. Returns `self` so the outcome can still be used.
    pub fn assert_stdout(self, expected: impl AsRef<str>) -> Self {
        reader::assert_matches("stdout", &self.stdout_str(), expected.as_ref());
        self
    }

    /// Asserts stderr matches `expected`, with the same leniency as
    /// [`Outcome::assert_stdout`].
    pub fn assert_stderr(self, expected: impl AsRef<str>) -> Self {
        reader::assert_matches("stderr", &self.stderr_str(), expected.as_ref());
        self
    }

    /// Asserts stdout matches `expected` byte for byte -- for a task that
    /// really does grade formatting, trailing newline included.
    pub fn assert_stdout_exact(self, expected: impl AsRef<str>) -> Self {
        reader::assert_matches_exactly("stdout", &self.stdout_str(), expected.as_ref());
        self
    }

    /// Asserts the command succeeded, panicking with the status and stderr
    /// if not. Returns `self` so the outcome can still be used.
    pub fn assert_success(self) -> Self {
        assert!(
            self.success(),
            "command did not succeed: {}\nstderr:\n{}",
            self.status.describe(),
            self.stderr_str(),
        );
        self
    }
}

fn grading() -> bool {
    std::env::var("AUTOGRADER_SANDBOX").is_ok()
}

/// Set by cargo for a same-package bin target (already built, exact path)
/// -- covers e.g. a `library`-kind driver, never a separate-package `<id>`.
pub(crate) fn cargo_bin_exe(target: &str) -> Option<PathBuf> {
    std::env::var(format!("CARGO_BIN_EXE_{target}"))
        .ok()
        .map(PathBuf::from)
}
