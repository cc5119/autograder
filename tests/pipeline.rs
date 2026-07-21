//! Integration tests for `autograder::pipeline::grade_batch`. Only touches
//! public API (`grade_batch`, `Store`, `Overrides`, `evaluator::StubEvaluator`,
//! `fetch::fetch_batch`) -- `checkout_rules`/`package_rules`/`terminal_eval`
//! are private and have no dedicated tests of their own; their behavior is
//! only observable through `grade_batch` itself, which is exactly what's
//! exercised here.

use autograder::error::Result;
use autograder::id::{AssignmentId, StudentId};
use autograder::model::{
    EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports, TestResult, TestStatus,
};
use autograder::pipeline::evaluator::{Evaluator, StubEvaluator};
use autograder::pipeline::grade_batch;
use autograder::pipeline::overrides::{ManualOverride, Overrides};
use autograder::spec::Spec;
use autograder::store::Store;
use autograder::submissions::source::SubmissionsSource;
use autograder::submissions::{LocalPath, Submission};

struct FixedSource(Vec<Submission<LocalPath>>);
impl SubmissionsSource<LocalPath> for FixedSource {
    fn submissions(&self) -> Result<Vec<Submission<LocalPath>>> {
        Ok(self.0.clone())
    }
}

/// `grade_batch` only reads what a prior fetch left behind, so every test
/// needs one run first.
fn fetch_first(source: &FixedSource, work_dir: &std::path::Path) {
    let deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
        .parse()
        .unwrap();
    autograder::submissions::fetch_batch(source, work_dir, &deadline).unwrap();
}

fn write(path: &std::path::Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Written to `package_dir/Cargo.lock` in every test that calls
/// `grade_batch` -- `prepare` (via `Cargo.lock`'s hash) needs it there
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

[limits.build]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256

[limits.run]
cpu-time = "5s"
wall-clock = "10s"
cpus = 1
memory = "512MiB"
pids = 128
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
fn grade_batch_runs_end_to_end_over_a_directory_submission() {
    let package_dir = tempfile::tempdir().unwrap();
    let submission_src = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    write(
        &package_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"hw3\"]\n",
    );
    write(&package_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &submission_src.path().join("hw3/src/lib.rs"),
        "// student code",
    );

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let source = FixedSource(vec![Submission {
        student_id: "alice".into(),
        fetchable: LocalPath(submission_src.path().to_path_buf()),
        metadata: Default::default(),
    }]);
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());

    fetch_first(&source, work_dir.path());
    let grades = grade_batch(
        &source,
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
        &Overrides::default(),
    )
    .unwrap();

    assert_eq!(grades.len(), 1);
    assert_eq!(grades[0].student_id, "alice");
    assert_eq!(grades[0].score, 1.0);
    assert_eq!(grades[0].max, None);

    let persisted = store.latest_evals(AssignmentId::new("hw3")).unwrap();
    assert_eq!(persisted.len(), 1);
    let persisted_grades = store.latest_grades(AssignmentId::new("hw3")).unwrap();
    assert_eq!(persisted_grades.len(), 1);

    assert!(
        work_dir
            .path()
            .join("alice/checkout/hw3/src/lib.rs")
            .is_file()
    );
    assert!(!work_dir.path().join("alice/build").exists());
}

#[test]
fn grade_batch_reports_fetch_failed_when_the_checkout_has_no_id_directory() {
    let package_dir = tempfile::tempdir().unwrap();
    let submission_src = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    write(&submission_src.path().join("src/lib.rs"), "// student code");

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let source = FixedSource(vec![Submission {
        student_id: "alice".into(),
        fetchable: LocalPath(submission_src.path().to_path_buf()),
        metadata: Default::default(),
    }]);
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());

    fetch_first(&source, work_dir.path());
    let grades = grade_batch(
        &source,
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
        &Overrides::default(),
    )
    .unwrap();

    assert_eq!(grades.len(), 1);
    assert_eq!(grades[0].status, "FetchFailed");
    assert!(!work_dir.path().join("alice/build").exists());
}

/// Records the sorted file names in `harness/tests/` (a sibling of
/// `ctx.workspace`) at evaluate-time, before `grade_batch` deletes the
/// scratch dir.
struct CapturingEvaluator<'a> {
    seen: &'a std::sync::Mutex<Option<Vec<String>>>,
}

impl Evaluator for CapturingEvaluator<'_> {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
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
            submission_date: None,
        })
    }
}

/// Confirms a submission's checkout can never reach `harness/`, even if it
/// ships its own same-named `harness/` directory at its root, hoping to
/// overwrite the trusted judge: the checkout side of the overlay only ever
/// globs `{id}/**`, so `harness/` in the build dir always comes from the
/// trusted `package_dir`, never the student.
#[test]
fn grade_batch_never_lets_the_submission_checkout_reach_the_harness_package() {
    let package_dir = tempfile::tempdir().unwrap();
    let submission_src = tempfile::tempdir().unwrap();
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
        &submission_src.path().join("wc/src/main.rs"),
        "fn main() {}\n",
    );
    // A malicious submission tries to ship its own `harness/`, hoping to
    // overwrite the trusted judge.
    write(
        &submission_src.path().join("harness/tests/decoy.rs"),
        "#[test]\nfn fake_pass() { assert!(true); }\n",
    );

    let toml = spec_toml()
        .replace("kind = \"library\"", "kind = \"binary\"")
        .replace("id = \"hw3\"", "id = \"wc\"");
    let spec: Spec = toml::from_str(&toml).unwrap();
    let source = FixedSource(vec![Submission {
        student_id: "alice".into(),
        fetchable: LocalPath(submission_src.path().to_path_buf()),
        metadata: Default::default(),
    }]);
    let seen = std::sync::Mutex::new(None);
    let evaluator = CapturingEvaluator { seen: &seen };
    let store = Store::new(store_dir.path());

    fetch_first(&source, work_dir.path());
    grade_batch(
        &source,
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
        &Overrides::default(),
    )
    .unwrap();

    assert_eq!(
        seen.lock().unwrap().take(),
        Some(vec!["judge.rs".to_string()])
    );
}

#[test]
fn grade_batch_scores_zero_for_a_disallowed_dependency_without_running_the_evaluator() {
    let package_dir = tempfile::tempdir().unwrap();
    let submission_src = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    write(
        &package_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"hw3\"]\n",
    );
    write(&package_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &submission_src.path().join("hw3/src/lib.rs"),
        "// student code",
    );
    write(
        &submission_src.path().join("hw3/Cargo.toml"),
        "[package]\nname = \"bst\"\nversion = \"0.1.0\"\n\n[dependencies]\ntokio = \"1\"\n",
    );

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let source = FixedSource(vec![Submission {
        student_id: "alice".into(),
        fetchable: LocalPath(submission_src.path().to_path_buf()),
        metadata: Default::default(),
    }]);
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());

    fetch_first(&source, work_dir.path());
    let grades = grade_batch(
        &source,
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
        &Overrides::default(),
    )
    .unwrap();

    assert_eq!(grades.len(), 1);
    assert_eq!(grades[0].score, 0.0);
    assert_eq!(grades[0].status, "DisallowedDependency");

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
fn grade_batch_handles_fetch_failure_without_aborting_the_batch() {
    let package_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let source = FixedSource(vec![Submission {
        student_id: "ghost".into(),
        fetchable: LocalPath("/nonexistent/path".into()),
        metadata: Default::default(),
    }]);
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());

    fetch_first(&source, work_dir.path());
    let grades = grade_batch(
        &source,
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
        &Overrides::default(),
    )
    .unwrap();

    assert_eq!(grades.len(), 1);
    assert_eq!(grades[0].score, 0.0);
    assert_eq!(grades[0].status, "FetchFailed");
}

#[test]
fn grade_batch_applies_a_manual_override_after_grading() {
    let package_dir = tempfile::tempdir().unwrap();
    let submission_src = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();

    write(
        &package_dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"hw3\"]\n",
    );
    write(&package_dir.path().join("Cargo.lock"), LOCK_TOML);
    write(
        &submission_src.path().join("hw3/src/lib.rs"),
        "// student code",
    );

    let spec: Spec = toml::from_str(&spec_toml()).unwrap();
    let source = FixedSource(vec![Submission {
        student_id: "alice".into(),
        fetchable: LocalPath(submission_src.path().to_path_buf()),
        metadata: Default::default(),
    }]);
    let evaluator = StubEvaluator {
        tests: vec![passing_test("insert_basic")],
    };
    let store = Store::new(store_dir.path());
    let overrides = Overrides {
        manual: std::collections::BTreeMap::from([(
            StudentId::new("alice"),
            ManualOverride {
                score: 3.0,
                status: Some("manual-review".into()),
                reason: "Partial credit for a documented edge case".into(),
            },
        )]),
        late: Default::default(),
    };

    fetch_first(&source, work_dir.path());
    let grades = grade_batch(
        &source,
        &evaluator,
        package_dir.path(),
        &spec,
        work_dir.path(),
        &store,
        &overrides,
    )
    .unwrap();

    assert_eq!(grades.len(), 1);
    assert_eq!(grades[0].score, 3.0);
    assert_eq!(grades[0].status, "manual-review");
    assert!(grades[0].override_reason.is_some());

    let persisted = store.latest_evals(AssignmentId::new("hw3")).unwrap();
    assert_eq!(persisted[0].tests[0].status, TestStatus::Pass);
}
