//! The trusted judge (design's `binary`-kind evaluator, `evaluator::binary`):
//! unlike `library-stack`'s harness, this isn't a separate crate -- `Prepare`
//! copies `harness/` directly onto the student's own checkout (see
//! `prepare::Wiring::Binary`), so this file lands at the student's own
//! `tests/judge.rs` and `cargo nextest run` builds+runs it as part of their
//! own crate. That's what makes `env!("CARGO_BIN_EXE_fizzbuzz")` resolve to
//! the student's own built binary with no extra wiring: Cargo sets that env
//! var automatically for any integration test in the same package as the
//! `fizzbuzz` bin target.
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
        output.status.success(), "fizzbuzz exited non-zero for n={n}: {}",
        String::from_utf8_lossy(& output.stderr)
    );
    String::from_utf8(output.stdout).expect("fizzbuzz stdout was not valid utf8")
}
#[test]
fn fizzbuzz_1_to_15() {
    let output = run("15");
    let expected = "1\n2\nFizz\n4\nBuzz\nFizz\n7\n8\nFizz\nBuzz\n11\nFizz\n13\n14\nFizzBuzz\n";
    assert_eq!(output, expected);
}
