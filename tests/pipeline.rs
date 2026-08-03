//! Integration tests for `autograder::pipeline::evaluate_batch`. Only
//! touches public API (`evaluate_batch`, `evaluator::StubEvaluator`,
//! `pipeline::grade::grade`) -- `checkout_rules`/`package_rules`/
//! `terminal_eval` are private and have no dedicated tests of their own;
//! their behavior is only observable through `evaluate_batch` itself, which
//! is exactly what's exercised here. Submissions dirs are built by hand
//! (`common::write`) rather than run through `fetch_batch` -- fetching
//! itself (real git, no fake-fetch seam) has its own coverage in
//! `src/submissions/mod.rs`, and evaluate never reads a fetch record at
//! all. Scoring (`pipeline::grade::grade`) is `autograder grade`'s job now,
//! not `evaluate_batch`'s -- tests that used to assert on a `Grade` call it
//! directly on the persisted eval, mirroring what `autograder grade` itself
//! does.

use autograder::exec::json::read_json;
use autograder::exec::sandbox::ProcessStatus;
use autograder::model::{
    BuildStatus, EvalStatus, EvaluationResult, JobContext, TestOutcome, TestResult, TestStatus,
};
use autograder::pipeline::evaluate_batch;
use autograder::pipeline::evaluator::{Evaluator, StubEvaluator};
use autograder::spec::Spec;

use crate::common::write;

/// Written to `assignment_dir/Cargo.lock` in every test that calls
/// `evaluate_batch` -- `lock::verify` runs before any submission is
/// touched, regardless of what a given test's `Cargo.toml` declares.
const LOCK_TOML: &str = "version = 4\n\n[[package]]\nname = \"hw3\"\nversion = \"0.1.0\"\n";

fn spec_toml() -> String {
    format!(
        r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{}"


[sandbox]
image = "autograder-base:1.86.0"

[build-limits]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256
max-output-bytes = "1MiB"

[scoring]
formula = "sum"
base = 0.0
"#,
        autograder::deps::cargo_lock::sha256_hex(LOCK_TOML)
    )
}

fn passing_test(name: &str) -> TestResult {
    TestResult {
        name: name.into(),
        status: TestStatus::Pass,
        duration_ms: 1,
        message: None,
        reported_score: None,
    }
}

/// The most recent `.eval/<github_user>/*.eval.json` under
/// `submissions_dir`, for asserting `evaluate_batch` actually persisted
/// what it returned.
fn persisted_evals(submissions_dir: &std::path::Path, github_user: &str) -> Vec<EvaluationResult> {
    let dir = submissions_dir.join(".eval").join(github_user);
    if !dir.is_dir() {
        return Vec::new();
    }
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| read_json(&e.unwrap().path()).unwrap())
        .collect()
}

#[test]
fn evaluate_batch_runs_end_to_end_over_a_flat_submissions_dir() {
    let assignment_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();

    write(
        &assignment_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"hw3\"]\n",
    );
    write(&assignment_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &submissions_dir.path().join("alice/hw3/src/lib.rs"),
        "// student code",
    );

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };

    let evals = evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        assignment_dir.path(),
        &spec,
    )
    .unwrap();

    assert_eq!(evals.len(), 1);
    assert_eq!(evals[0].github_user, "alice");
    let grade = autograder::pipeline::grade::grade(&evals[0], &spec.scoring);
    assert_eq!(grade.score(), Some(1.0));

    let persisted = persisted_evals(submissions_dir.path(), "alice");
    assert_eq!(persisted.len(), 1);

    // Flat: no `checkout/` nesting under the submission's own dir.
    assert!(
        submissions_dir
            .path()
            .join("alice/hw3/src/lib.rs")
            .is_file()
    );
}

#[test]
fn evaluate_batch_reports_build_failed_when_the_checkout_has_no_id_directory() {
    let assignment_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();

    // No `hw3/` under `alice/` -- the checkout has no crate matching
    // `[assignment].id`.
    write(
        &submissions_dir.path().join("alice/src/lib.rs"),
        "// student code",
    );
    write(&assignment_dir.path().join("Cargo.lock"), LOCK_TOML);

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };

    let evals = evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        assignment_dir.path(),
        &spec,
    )
    .unwrap();

    assert_eq!(evals.len(), 1);
    assert!(matches!(
        evals[0].status,
        EvalStatus::BuildFailed(BuildStatus::Failed(_))
    ));
}

/// Records the sorted file names in `harness/tests/` (under `ctx.workspace`
/// -- see `JobContext`'s doc comment for the layout) at evaluate-time,
/// before `evaluate_batch` deletes the scratch dir.
struct CapturingEvaluator<'a> {
    seen: &'a std::sync::Mutex<Option<Vec<String>>>,
}

impl Evaluator for CapturingEvaluator<'_> {
    fn evaluate(&self, ctx: &JobContext) -> autograder::error::Result<EvaluationResult> {
        let harness_tests = ctx.workspace.join("harness/tests");
        let mut names: Vec<String> = std::fs::read_dir(harness_tests)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        *self.seen.lock().unwrap() = Some(names);

        Ok(EvaluationResult {
            assignment_id: ctx.assignment_id,
            github_user: ctx.github_user,
            run_id: ctx.run_id,
            graded_commit: None,
            instructor_commit: None,
            status: EvalStatus::Ran {
                process: ProcessStatus::Exited(0),
                tests: TestOutcome::Tests(Vec::new()),
            },
            wall_clock_ms: None,
            diagnostics: Default::default(),
        })
    }
}

/// Confirms a submission's checkout can never reach `harness/`, even if it
/// ships its own same-named `harness/` directory at its root, hoping to
/// overwrite the trusted judge: the checkout side of the overlay only ever
/// globs `{id}/**`, so `harness/` in the build dir always comes from the
/// trusted `assignment_dir`, never the student.
#[test]
fn evaluate_batch_never_lets_the_submission_checkout_reach_the_harness_package() {
    let assignment_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();

    write(
        &assignment_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"harness\", \"wc\"]\n",
    );
    write(&assignment_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &assignment_dir.path().join("harness/Cargo.toml"),
        "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    write(
        &assignment_dir.path().join("harness/tests/judge.rs"),
        "#[test]\nfn counts_words() {}\n",
    );

    write(
        &submissions_dir.path().join("alice/wc/src/main.rs"),
        "fn main() {}\n",
    );
    // A malicious submission tries to ship its own `harness/`, hoping to
    // overwrite the trusted judge.
    write(
        &submissions_dir.path().join("alice/harness/tests/decoy.rs"),
        "#[test]\nfn fake_pass() { assert!(true); }\n",
    );

    let toml = spec_toml()
        .replace("kind = \"library\"", "kind = \"binary\"")
        .replace("id = \"hw3\"", "id = \"wc\"");
    let spec: Spec = toml::from_str(&toml).unwrap();
    let seen = std::sync::Mutex::new(None);
    let evaluator = CapturingEvaluator { seen: &seen };

    evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        assignment_dir.path(),
        &spec,
    )
    .unwrap();

    assert_eq!(
        seen.lock().unwrap().take(),
        Some(vec!["judge.rs".to_string()])
    );
}

#[test]
fn evaluate_batch_scores_zero_for_a_disallowed_dependency_without_running_the_evaluator() {
    let assignment_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();

    write(
        &assignment_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"hw3\"]\n",
    );
    write(&assignment_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &submissions_dir.path().join("alice/hw3/src/lib.rs"),
        "// student code",
    );
    write(
        &submissions_dir.path().join("alice/hw3/Cargo.toml"),
        "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\ntokio = \"1\"\n",
    );

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };

    let evals = evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        assignment_dir.path(),
        &spec,
    )
    .unwrap();

    assert_eq!(evals.len(), 1);
    let grade = autograder::pipeline::grade::grade(&evals[0], &spec.scoring);
    assert_eq!(grade.score(), None);

    let persisted = persisted_evals(submissions_dir.path(), "alice");
    assert_eq!(persisted.len(), 1);
    assert!(
        persisted[0]
            .diagnostics
            .stderr_excerpt
            .as_deref()
            .unwrap()
            .contains("tokio")
    );
}
