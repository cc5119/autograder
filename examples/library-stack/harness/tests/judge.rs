//! The trusted judge: no student code is linked into *this* test binary. It
//! only talks to the compiled `driver` binary (which does link the
//! student's `library-stack` crate) across a process boundary -- the whole
//! op sequence goes to the driver's stdin in one shot (via
//! `autograder-test`, which runs it under `isolate` for per-test resource
//! limits when grading for real), and the resulting sequence of responses
//! comes back on stdout once the process exits.
//!
//! `push_pop_basic` is unmarked, so it ships to students as the public
//! test; `many_pushes_then_pops` is gated `#[cfg(not(feature =
//! "student"))]`, so `publish` drops it -- adversarial in the sense the
//! design calls for: it can't be satisfied by a solution that
//! special-cases the couple of ops the public test happens to check.

/// Runs `ops` (one per line) against the driver in one invocation and
/// returns its responses, one per line, in the same order.
fn run(ops: &[&str]) -> Vec<String> {
    let script = ops.join("\n");
    let outcome = autograder_test::bin("driver").stdin(script).run();
    assert!(
        outcome.success(),
        "driver exited non-zero: {}",
        outcome.stderr_str()
    );
    outcome
        .stdout_str()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn push_pop_basic() {
    let responses = run(&["push 1", "push 2", "pop", "pop", "pop"]);
    assert_eq!(responses, ["ok", "ok", "some 2", "some 1", "none"]);
}

#[cfg(not(feature = "student"))]
#[test]
fn many_pushes_then_pops() {
    let mut ops: Vec<String> = (0..1000).map(|i| format!("push {i}")).collect();
    ops.push("len".to_string());
    ops.extend((0..1000).rev().map(|_| "pop".to_string()));
    ops.push("pop".to_string());
    let ops: Vec<&str> = ops.iter().map(String::as_str).collect();

    let responses = run(&ops);

    assert!(responses[..1000].iter().all(|r| r == "ok"));
    assert_eq!(responses[1000], "1000");
    for (i, response) in responses[1001..2001].iter().enumerate() {
        assert_eq!(*response, format!("some {}", 999 - i));
    }
    assert_eq!(responses[2001], "none");
}
