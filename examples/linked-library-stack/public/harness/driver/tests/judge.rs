//! Public-tier judge: only the public test (design §11.4 — hidden tests
//! never ship to students). Otherwise identical protocol/session code to
//! ../../../../instructor/harness/driver/tests/judge.rs; the two harnesses
//! are independent copies by design (§5) so keeping them in sync is the
//! instructor's responsibility (open question §18.6).

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
