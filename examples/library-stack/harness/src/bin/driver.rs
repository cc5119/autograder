//! The trusted driver: reads operations one per line from stdin, calls the
//! corresponding student `Stack` API, and writes one response per line to
//! stdout. No assertions live here -- `tests/judge.rs` supplies the whole
//! op sequence up front (via `autograder-test`) and checks the resulting
//! sequence of responses once this process exits. Protocol, one op per
//! line:
//!
//! ```text
//! push <i64>   -> ok
//! pop          -> some <i64> | none
//! peek         -> some <i64> | none
//! len          -> <usize>
//! ```

use std::io::{self, BufRead, Write};

use library_stack::Stack;

fn main() {
    let mut stack = Stack::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line.expect("read stdin");
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let op = parts.next().unwrap_or("");

        let response = match op {
            "push" => {
                let value: i64 = parts
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("push requires an integer argument");
                stack.push(value);
                "ok".to_string()
            }
            "pop" => match stack.pop() {
                Some(v) => format!("some {v}"),
                None => "none".to_string(),
            },
            "peek" => match stack.peek() {
                Some(v) => format!("some {v}"),
                None => "none".to_string(),
            },
            "len" => stack.len().to_string(),
            other => format!("error: unknown op {other}"),
        };

        writeln!(stdout, "{response}").expect("write stdout");
        stdout.flush().expect("flush stdout");
    }
}
