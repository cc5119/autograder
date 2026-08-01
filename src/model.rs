use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::exec::sandbox::ProcessStatus;
use crate::id::{AssignmentId, CommitSha, GithubUser, RunId};

/// Per-job context threaded through the pipeline stages. `github_user`
/// identifies a submission directory (see `GithubUser`'s doc comment) --
/// evaluate itself never consults roster data by it, even though the id
/// is a `GithubUser`.
///
/// `workspace` is the scratch root for this job (what `Nextest` calls
/// `repo_root`), laid out as:
///
/// ```text
/// workspace/
///   Cargo.toml
///   Cargo.lock
///   {assignment_id}/   <- submission_package_dir(): the student's own package
///   {harness}/         <- the trusted judge package
/// ```
#[derive(Debug, Clone)]
pub struct JobContext {
    pub assignment_id: AssignmentId,
    pub github_user: GithubUser,
    pub run_id: RunId,
    pub workspace: PathBuf,
}

impl JobContext {
    pub fn submission_package_dir(&self) -> PathBuf {
        self.workspace.join(self.assignment_id.as_str())
    }
}

/// Verdict for a single test, as observed by the trusted judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Pass,
    Fail,
    Timeout,
    Oom,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub status: TestStatus,
    /// Parsed from the JUnit report's `time` attribute (seconds, converted
    /// to ms); `0` if it was missing or unparseable -- never observed from
    /// real `cargo nextest` output, which always emits a valid `time`.
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Sum of every `autograder: score=<f64>` line this test
    /// printed to stdout (see `crate::pipeline::grade`); `None` if it printed none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_score: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    Failed(ProcessStatus),
    DisallowedDependency,
}

impl BuildStatus {
    pub fn label(self) -> String {
        match self {
            BuildStatus::Failed(status) => status.describe(),
            BuildStatus::DisallowedDependency => "disallowed dependency".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    /// Parsed results are available (individual tests may still have
    /// failed -- that's tracked per-`TestResult`, not here).
    Tests(Vec<TestResult>),
    /// No parseable results; `String` says why (e.g. "no junit.xml
    /// produced", "run process did not complete"). Never treated as a pass.
    Unavailable(String),
}

impl TestOutcome {
    pub fn label(&self) -> String {
        match self {
            TestOutcome::Tests(_) => "ok".to_string(),
            TestOutcome::Unavailable(reason) => reason.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiler_errors: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    BuildFailed(BuildStatus),
    Ran {
        process: ProcessStatus,
        tests: TestOutcome,
    },
}

/// The sole contract between untrusted execution and scoring.
/// `github_user` identifies the submission this ran -- see `JobContext`'s
/// doc comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub assignment_id: AssignmentId,
    pub github_user: GithubUser,
    pub run_id: RunId,
    /// The graded submission's own commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graded_commit: Option<CommitSha>,
    /// The private assignment repo's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructor_commit: Option<CommitSha>,
    pub status: EvalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_ms: Option<u64>,
    #[serde(default)]
    pub diagnostics: Diagnostics,
}

impl EvaluationResult {
    /// One line describing this result, e.g. `"alice: ok (3/3 tests
    /// passed)"` or `"alice: build failed"` -- no trailing newline, so
    /// callers (a plain `println!`, or an `indicatif` progress line) control
    /// that themselves.
    pub fn describe(&self) -> String {
        match &self.status {
            EvalStatus::Ran {
                tests: TestOutcome::Tests(tests),
                ..
            } => {
                let passed = tests
                    .iter()
                    .filter(|t| t.status == TestStatus::Pass)
                    .count();
                format!(
                    "{}: ok ({passed}/{} tests passed)",
                    self.github_user,
                    tests.len()
                )
            }
            EvalStatus::BuildFailed(status) => {
                format!("{}: {}", self.github_user, status.label())
            }
            EvalStatus::Ran { process, tests } => {
                format!(
                    "{}: {} ({})",
                    self.github_user,
                    process.describe(),
                    tests.label()
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grade {
    pub github_user: GithubUser,
    pub outcome: GradeOutcome,
}

/// Whether there was anything to score, and the evidence either way. One
/// enum rather than an `Option<f64>` beside a separate explanation: the
/// same condition decides both, so this is the only place it's decided.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GradeOutcome {
    Scored {
        score: f64,
        passed: usize,
        total: usize,
    },
    /// The build failed or the run left no readable results -- neither
    /// leaves a trustworthy set of tests to score. `reason` says which.
    Unscored { reason: String },
}

impl Grade {
    pub fn score(&self) -> Option<f64> {
        match self.outcome {
            GradeOutcome::Scored { score, .. } => Some(score),
            GradeOutcome::Unscored { .. } => None,
        }
    }
}
