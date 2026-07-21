//! The trusted judge (the `binary`-kind evaluator, `evaluator::binary`).
//! Lives in its own `harness` package, a sibling of the reference
//! solution's `fizzbuzz` package -- same shape as `library-stack`'s
//! `harness`, just with no Cargo dependency edge to the crate under test
//! (`fizzbuzz` is bin-only; a plain `[dependencies]` entry can only link a
//! *library* target). At grading time, `pipeline::grade_batch` copies this
//! whole file onto the submission being graded (overlaying whatever
//! public-only subset the student's own checkout had); for `ci`, nothing
//! needs copying, since `autograder init` already baked the public-only
//! subset directly into the student's own starter when it built it (see
//! `evaluator::binary`'s module doc comment).
//!
//! No student code is linked into *this* test binary -- it only spawns the
//! compiled `fizzbuzz` binary as a child process and asserts purely on its
//! observable stdout, never on anything the binary "reports" about itself.
//! `env!("CARGO_BIN_EXE_fizzbuzz")` can't locate it here (that only
//! resolves within a test's *own* package, never across a dependency
//! edge -- part of why there's no dependency edge to `fizzbuzz` at all);
//! instead, `student_binary` below walks up from this crate's own
//! `CARGO_MANIFEST_DIR` to the workspace root's shared `target/` dir,
//! where an ordinary `cargo build`/`cargo nextest run` at the workspace
//! root always puts every member's build output, `fizzbuzz`'s binary
//! included. `autograder` has no opinion on this and needs none -- a
//! `build.rs`, the `escargot` crate, or anything else that locates the
//! built binary would work exactly as well.
//!
//! `fizzbuzz_1_to_15` is unmarked, so it ships to students as the public
//! test (the classic case); `fizzbuzz_multiples_of_15_and_zero` is gated
//! `#[cfg(not(feature = "student"))]`, so `publish` drops it --
//! adversarial in the sense the design calls for: a solution that
//! special-cases exactly the first 15 lines (matching the public test)
//! still fails the n=0 (no output) and n=30 (another multiple-of-15 line,
//! plus a full second pass through the fizz/buzz cycle) cases.

use std::path::PathBuf;
use std::process::Command;

fn student_binary() -> PathBuf {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("harness has a parent (the workspace root)");
    // `cargo build` (unlike `cargo test`, which only builds `fizzbuzz` far
    // enough to satisfy the workspace graph, never leaving the usual
    // `target/debug/fizzbuzz` convenience copy behind since nothing
    // depends on it) reliably produces that copy -- cheap/no-op if it's
    // already up to date, so this is safe to call from every test.
    let status = Command::new("cargo")
        .args(["build", "-p", "fizzbuzz"])
        .current_dir(repo_root)
        .status()
        .expect("run cargo build -p fizzbuzz");
    assert!(status.success(), "cargo build -p fizzbuzz failed");
    repo_root.join("target/debug/fizzbuzz")
}

fn run(n: &str) -> String {
    let output = Command::new(student_binary())
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
