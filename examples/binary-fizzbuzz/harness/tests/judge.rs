//! The trusted judge. Lives in its own `harness` package, a sibling of the
//! reference solution's `fizzbuzz` package -- same shape as
//! `library-stack`'s `harness`, just with no Cargo dependency edge to the
//! crate under test (`fizzbuzz` is bin-only; a plain `[dependencies]` entry
//! can only link a *library* target). At grading time, `pipeline::grade_batch`
//! copies this whole file onto the submission being graded (overlaying
//! whatever public-only subset the student's own checkout had); for `ci`,
//! nothing needs copying, since `autograder init` already baked the
//! public-only subset directly into the student's own starter when it
//! built it.
//!
//! No student code is linked into *this* test binary -- it only spawns the
//! compiled `fizzbuzz` binary as a child process (via `autograder-test`,
//! which locates it in the shared `target/` dir and runs it under `isolate`
//! for per-test resource limits when grading for real) and asserts purely
//! on its observable stdout, never on anything the binary "reports" about
//! itself.
//!
//! `fizzbuzz_1_to_15` is unmarked, so it ships to students as the public
//! test (the classic case); `fizzbuzz_multiples_of_15_and_zero` is gated
//! `#[cfg(not(feature = "student"))]`, so `publish` drops it --
//! adversarial in the sense the design calls for: a solution that
//! special-cases exactly the first 15 lines (matching the public test)
//! still fails the n=0 (no output) and n=30 (another multiple-of-15 line,
//! plus a full second pass through the fizz/buzz cycle) cases.

fn run(n: &str) -> String {
    let outcome = autograder_test::bin("fizzbuzz").arg(n).run();
    assert!(
        outcome.success(),
        "fizzbuzz exited non-zero for n={n}: {}",
        outcome.stderr_str()
    );
    outcome.stdout_str()
}

#[test]
fn fizzbuzz_1_to_15() {
    let output = run("15");
    let expected = "1\n2\nFizz\n4\nBuzz\nFizz\n7\n8\nFizz\nBuzz\n11\nFizz\n13\n14\nFizzBuzz\n";
    assert_eq!(output, expected);
}

#[cfg(not(feature = "student"))]
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
