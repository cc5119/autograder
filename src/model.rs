use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::id::{AssignmentId, RunId, StudentId};

/// Per-job context threaded through the pipeline stages. `student_id`
/// identifies a submission directory (see `StudentId`'s doc comment) --
/// evaluate itself never consults roster data by it, even though the id
/// is a `StudentId`.
#[derive(Debug, Clone)]
pub struct JobContext {
    pub assignment_id: AssignmentId,
    pub student_id: StudentId,
    pub run_id: RunId,
    pub workspace: PathBuf,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Terminal status of the build stage: compiling the submission's own
/// crate, then the harness against it (`pipeline::evaluator::nextest`'s
/// stages 1-2), or a precondition checked before either ever runs (missing
/// crate dir, disallowed dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    Ok,
    Failed,
    Timeout,
    Oom,
    DisallowedDependency,
}

impl BuildStatus {
    pub fn label(self) -> &'static str {
        match self {
            BuildStatus::Ok => "ok",
            BuildStatus::Failed => "build failed",
            BuildStatus::Timeout => "timeout",
            BuildStatus::Oom => "out of memory",
            BuildStatus::DisallowedDependency => "disallowed dependency",
        }
    }
}

/// Terminal status of the run stage: only reachable once the build stage
/// has already succeeded (see `StageReports`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Ok,
    Timeout,
    Oom,
    HarnessError,
}

impl RunStatus {
    pub fn label(self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Timeout => "timeout",
            RunStatus::Oom => "out of memory",
            RunStatus::HarnessError => "harness error",
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

/// The pipeline is strictly sequential -- the run stage only ever happens
/// once the build stage has succeeded -- so this makes the two impossible
/// states from earlier designs unrepresentable: there's no `run: Ok` sitting
/// alongside a failed build (the `Ran` variant only exists once a build
/// succeeded), and no cross-stage status values (`BuildStatus`/`RunStatus`
/// each only contain what's reachable in their own stage).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    BuildFailed(BuildStatus),
    Ran(RunStatus),
}

/// The sole contract between untrusted execution and scoring.
/// `student_id` identifies the submission this ran -- see `JobContext`'s
/// doc comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub assignment_id: AssignmentId,
    pub student_id: StudentId,
    pub run_id: RunId,
    /// The graded submission's own commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graded_commit: Option<String>,
    /// The private assignment repo's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructor_commit: Option<String>,
    pub status: EvalStatus,
    pub tests: Vec<TestResult>,
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
            EvalStatus::Ran(RunStatus::Ok) => {
                let passed = self
                    .tests
                    .iter()
                    .filter(|t| t.status == TestStatus::Pass)
                    .count();
                format!(
                    "{}: ok ({passed}/{} tests passed)",
                    self.student_id,
                    self.tests.len()
                )
            }
            EvalStatus::BuildFailed(status) => {
                format!("{}: {}", self.student_id, status.label())
            }
            EvalStatus::Ran(status) => format!("{}: {}", self.student_id, status.label()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grade {
    pub student_id: StudentId,
    pub score: f64,
    /// The scale's theoretical ceiling, when the scoring formula defines
    /// one (`affine`'s `scale-max`). `sum` is unnormalized and has no
    /// natural ceiling, so this is `None` for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    pub status: String,
    #[serde(default)]
    pub failing_tests: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EvaluationResult {
        EvaluationResult {
            assignment_id: AssignmentId::new("hw3"),
            student_id: StudentId::new("alice"),
            run_id: RunId::new("2026-07-17T18-03-00Z-ab12"),
            graded_commit: Some("a1b2c3d".into()),
            instructor_commit: Some("f9e8d7".into()),
            status: EvalStatus::Ran(RunStatus::Ok),
            tests: vec![
                TestResult {
                    name: "insert_basic".into(),
                    status: TestStatus::Pass,
                    duration_ms: 5,
                    message: None,
                    reported_score: Some(0.83),
                },
                TestResult {
                    name: "balance_adv".into(),
                    status: TestStatus::Fail,
                    duration_ms: 9,
                    message: Some("assertion failed: height <= 2*log2(n)".into()),
                    reported_score: None,
                },
                TestResult {
                    name: "delete_edge".into(),
                    status: TestStatus::Timeout,
                    duration_ms: 0,
                    message: None,
                    reported_score: None,
                },
            ],
            wall_clock_ms: Some(380),
            diagnostics: Diagnostics {
                compiler_errors: None,
                stderr_excerpt: Some("…".into()),
            },
        }
    }

    #[test]
    fn evaluation_result_roundtrips_through_json() {
        let original = sample();
        let json = serde_json::to_string_pretty(&original).unwrap();
        let parsed: EvaluationResult = serde_json::from_str(&json).unwrap();
        let rejson = serde_json::to_string_pretty(&parsed).unwrap();
        assert_eq!(json, rejson);
        assert_eq!(parsed.tests.len(), 3);
        assert_eq!(parsed.tests[1].status, TestStatus::Fail);
    }
}
