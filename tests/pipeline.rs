//! Integration tests for `autograder::pipeline::evaluate_batch`. Only
//! touches public API (`evaluate_batch`, `Store`, `evaluator::StubEvaluator`,
//! `pipeline::grade::grade`, `overrides::apply`) -- `checkout_rules`/
//! `package_rules`/`terminal_eval` are private and have no dedicated tests
//! of their own; their behavior is only observable through `evaluate_batch`
//! itself, which is exactly what's exercised here. Submissions dirs are
//! built by hand (`common::write`/`common::write_fetch_record`) rather than
//! run through `fetch_batch` -- fetching itself (real git, no fake-fetch
//! seam) has its own coverage in `src/submissions/mod.rs`. Scoring
//! (`pipeline::grade::grade`/`overrides::apply`) is `autograder grade`'s job
//! now, not `evaluate_batch`'s -- tests that used to assert on a `Grade`
//! call those functions directly on the persisted eval, mirroring what
//! `autograder grade` itself does.

use autograder::id::AssignmentId;
use autograder::model::{
    EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports, StageStatus,
    TestResult, TestStatus,
};
use autograder::pipeline::evaluate_batch;
use autograder::pipeline::evaluator::{Evaluator, StubEvaluator};
use autograder::pipeline::overrides::{ManualOverride, Overrides};
use autograder::spec::Spec;
use autograder::store::Store;

use crate::common::{ok_fetch_record, write, write_fetch_record};

/// Written to `package_dir/Cargo.lock` in every test that calls
/// `evaluate_batch` -- `prepare` (via `Cargo.lock`'s hash) needs it there
/// regardless of what a given test's `Cargo.toml` actually declares.
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
        duration_ms: Some(1),
        message: None,
        reported_score: None,
    }
}

#[test]
fn evaluate_batch_runs_end_to_end_over_a_flat_submissions_dir() {
    let package_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    write(
        &package_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"hw3\"]\n",
    );
    write(&package_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &submissions_dir.path().join("alice/hw3/src/lib.rs"),
        "// student code",
    );
    write_fetch_record(submissions_dir.path(), "alice", &ok_fetch_record());

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());

    let evals = evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
    )
    .unwrap();

    assert_eq!(evals.len(), 1);
    assert_eq!(evals[0].student_id, "alice");
    let grade = autograder::pipeline::grade::grade(&evals[0], &spec.scoring);
    assert_eq!(grade.score, 1.0);
    assert_eq!(grade.max, None);

    let persisted = store.latest_evals(AssignmentId::new("hw3")).unwrap();
    assert_eq!(persisted.len(), 1);

    // Flat: no `checkout/` nesting under the student's own dir.
    assert!(
        submissions_dir
            .path()
            .join("alice/hw3/src/lib.rs")
            .is_file()
    );
    assert!(!work_dir.path().join("alice/build").exists());
}

#[test]
fn evaluate_batch_reports_fetch_failed_when_the_checkout_has_no_id_directory() {
    let package_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    // No `hw3/` under `alice/` -- the fetch itself succeeded, but the
    // checkout has no crate matching `[assignment].id`.
    write(&submissions_dir.path().join("alice/src/lib.rs"), "// student code");
    write_fetch_record(submissions_dir.path(), "alice", &ok_fetch_record());

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());

    let evals = evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
    )
    .unwrap();

    assert_eq!(evals.len(), 1);
    assert_eq!(evals[0].stages.fetch.status, StageStatus::FetchFailed);
    assert!(!work_dir.path().join("alice/build").exists());
}

/// Records the sorted file names in `harness/tests/` (a sibling of
/// `ctx.workspace`) at evaluate-time, before `evaluate_batch` deletes the
/// scratch dir.
struct CapturingEvaluator<'a> {
    seen: &'a std::sync::Mutex<Option<Vec<String>>>,
}

impl Evaluator for CapturingEvaluator<'_> {
    fn evaluate(&self, ctx: &JobContext) -> autograder::error::Result<EvaluationResult> {
        let harness_tests = ctx.workspace.parent().unwrap().join("harness/tests");
        let mut names: Vec<String> = std::fs::read_dir(harness_tests)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        *self.seen.lock().unwrap() = Some(names);

        Ok(EvaluationResult {
            schema_version: 1,
            assignment_id: ctx.assignment_id,
            student_id: ctx.student_id,
            run_id: ctx.run_id,
            graded_commit: None,
            instructor_commit: None,
            public_harness_commit: None,
            stages: StageReports {
                fetch: StageReport::ok(),
                build: StageReport::ok(),
                run: StageReport::ok(),
            },
            tests: Vec::new(),
            resource_usage: ResourceUsage::default(),
            diagnostics: Default::default(),
        })
    }
}

/// Confirms a submission's checkout can never reach `harness/`, even if it
/// ships its own same-named `harness/` directory at its root, hoping to
/// overwrite the trusted judge: the checkout side of the overlay only ever
/// globs `{id}/**`, so `harness/` in the build dir always comes from the
/// trusted `package_dir`, never the student.
#[test]
fn evaluate_batch_never_lets_the_submission_checkout_reach_the_harness_package() {
    let package_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    write(
        &package_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"harness\", \"wc\"]\n",
    );
    write(&package_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &package_dir.path().join("harness/Cargo.toml"),
        "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    write(
        &package_dir.path().join("harness/tests/judge.rs"),
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
    write_fetch_record(submissions_dir.path(), "alice", &ok_fetch_record());

    let toml = spec_toml()
        .replace("kind = \"library\"", "kind = \"binary\"")
        .replace("id = \"hw3\"", "id = \"wc\"");
    let spec: Spec = toml::from_str(&toml).unwrap();
    let seen = std::sync::Mutex::new(None);
    let evaluator = CapturingEvaluator { seen: &seen };
    let store = Store::new(store_dir.path());

    evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
    )
    .unwrap();

    assert_eq!(
        seen.lock().unwrap().take(),
        Some(vec!["judge.rs".to_string()])
    );
}

#[test]
fn evaluate_batch_scores_zero_for_a_disallowed_dependency_without_running_the_evaluator() {
    let package_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    write(
        &package_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"hw3\"]\n",
    );
    write(&package_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &submissions_dir.path().join("alice/hw3/src/lib.rs"),
        "// student code",
    );
    write(
        &submissions_dir.path().join("alice/hw3/Cargo.toml"),
        "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\ntokio = \"1\"\n",
    );
    write_fetch_record(submissions_dir.path(), "alice", &ok_fetch_record());

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());

    let evals = evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
    )
    .unwrap();

    assert_eq!(evals.len(), 1);
    let grade = autograder::pipeline::grade::grade(&evals[0], &spec.scoring);
    assert_eq!(grade.score, 0.0);
    assert_eq!(grade.status, "DisallowedDependency");

    let persisted = store.latest_evals(AssignmentId::new("hw3")).unwrap();
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

#[test]
fn grade_applies_a_manual_override_after_evaluate_persists_the_eval() {
    let package_dir = tempfile::tempdir().unwrap();
    let submissions_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    write(
        &package_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"hw3\"]\n",
    );
    write(&package_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &submissions_dir.path().join("alice/hw3/src/lib.rs"),
        "// student code",
    );
    write_fetch_record(submissions_dir.path(), "alice", &ok_fetch_record());

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());
    let overrides = Overrides {
        manual: std::collections::BTreeMap::from([(
            "alice".into(),
            ManualOverride {
                score: 3.0,
                status: Some("manual-review".into()),
                reason: "Partial credit for a documented edge case".into(),
            },
        )]),
        late: Default::default(),
    };

    let evals = evaluate_batch(
        submissions_dir.path(),
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
    )
    .unwrap();

    let grade = autograder::pipeline::grade::grade(&evals[0], &spec.scoring);
    let grade = autograder::pipeline::overrides::apply(
        grade,
        &overrides,
        &spec.assignment.deadline,
        spec.scoring.late_penalty.as_ref(),
        None,
    );

    assert_eq!(grade.score, 3.0);
    assert_eq!(grade.status, "manual-review");
    assert!(grade.override_reason.is_some());

    let persisted = store.latest_evals(AssignmentId::new("hw3")).unwrap();
    assert_eq!(persisted[0].tests[0].status, TestStatus::Pass);
}
