//! The trusted judge (design's `binary`-kind evaluator, `evaluator::binary`).
//! Unlike `library-stack`'s harness, there's no separate crate: these tests
//! live directly inside this reference solution's own package, which is
//! what makes `env!("CARGO_BIN_EXE_fizzbuzz")` resolve to whatever binary
//! is actually being tested with no extra wiring -- Cargo only populates
//! that env var for a binary target that's part of the *same* package as
//! the integration test, never across a dependency edge, so a separate
//! harness crate couldn't use it at all. At grading time,
//! `pipeline::grade_batch` copies this whole file onto the submission
//! being graded (overlaying whatever public-only subset the student's own
//! checkout had); for `ci`, nothing needs copying, since `scaffold` already
//! baked the public-only subset directly into the student's own `tests/`
//! when it built the starter (see `scaffold`'s and `evaluator::binary`'s
//! module doc comments).
//!
//! No student code is linked into *this* test binary -- it only spawns the
//! compiled `fizzbuzz` binary as a child process and asserts purely on its
//! observable stdout, never on anything the binary "reports" about itself.
//!
//! `fizzbuzz_1_to_15` is the public test (the classic case, also shipped to
//! students); `fizzbuzz_multiples_of_15_and_zero` is private (hidden) --
//! adversarial in the sense the design calls for (§9): a solution that
//! special-cases exactly the first 15 lines (matching the public test)
//! still fails the n=0 (no output) and n=30 (another multiple-of-15 line,
//! plus a full second pass through the fizz/buzz cycle) cases.

use std::process::Command;

fn run(n: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_fizzbuzz"))
        .arg(n)
        .output()
        .expect("spawn fizzbuzz");
    assert!(
        output.status.success(),
        "fizzbuzz exited non-zero for n={n}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("fizzbuzz stdout was not valid utf8")
}

#[test]
fn fizzbuzz_1_to_15() {
    let output = run("15");
    let expected = "1\n2\nFizz\n4\nBuzz\nFizz\n7\n8\nFizz\nBuzz\n11\nFizz\n13\n14\nFizzBuzz\n";
    assert_eq!(output, expected);
}

#[test]
fn fizzbuzz_multiples_of_15_and_zero() {
    // n=0 -> no lines at all.
    assert_eq!(run("0"), "");

    let output = run("30");
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 30);
    assert_eq!(lines[14], "FizzBuzz"); // line 15
    assert_eq!(lines[28], "29");
    assert_eq!(lines[29], "FizzBuzz"); // line 30
}
