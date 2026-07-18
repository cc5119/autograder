//! The trusted judge (design §9.1): no student code is linked into *this*
//! test binary. It only talks to the compiled `driver` binary (which does
//! link the student's `stack` crate) across a process boundary — a stdin/
//! stdout protocol — and asserts purely on the driver's observable
//! responses. Runs process-per-session under `cargo nextest`: each `#[test]`
//! spawns its own driver process.
//!
//! `push_pop_basic` is the public test (also shipped in
//! `examples/library-stack/public/`); `many_pushes_then_pops` is
//! private (hidden) — it never ships to students, and it's adversarial in
//! the sense the design calls for (§9): it can't be satisfied by a solution
//! that special-cases the couple of ops the public test happens to check.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Session {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_driver"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn driver");
        let stdin = child.stdin.take().expect("driver stdin");
        let stdout = BufReader::new(child.stdout.take().expect("driver stdout"));
        Session {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, op: &str) -> String {
        writeln!(self.stdin, "{op}").expect("write to driver");
        self.stdin.flush().expect("flush driver stdin");
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read from driver");
        line.trim().to_string()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn push_pop_basic() {
    let mut session = Session::start();
    assert_eq!(session.send("push 1"), "ok");
    assert_eq!(session.send("push 2"), "ok");
    assert_eq!(session.send("pop"), "some 2");
    assert_eq!(session.send("pop"), "some 1");
    assert_eq!(session.send("pop"), "none");
}

#[test]
fn many_pushes_then_pops() {
    let mut session = Session::start();
    for i in 0..1000 {
        assert_eq!(session.send(&format!("push {i}")), "ok");
    }
    assert_eq!(session.send("len"), "1000");
    for i in (0..1000).rev() {
        assert_eq!(session.send("pop"), format!("some {i}"));
    }
    assert_eq!(session.send("pop"), "none");
}
