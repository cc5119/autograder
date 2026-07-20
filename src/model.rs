use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::id::{AssignmentId, RunId, StudentId};

/// Per-job context threaded through the pipeline stages.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Sum of every `autograder: score=<f64>` line this test
    /// printed to stdout (see `crate::pipeline::grade`); `None` if it printed none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_score: Option<f64>,
}

/// Terminal status of a pipeline stage (fetch/build/run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Ok,
    BuildFailed,
    Timeout,
    Oom,
    DisallowedDependency,
    FetchFailed,
    HarnessError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageReport {
    pub status: StageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warnings: Option<u32>,
}

impl StageReport {
    pub fn ok() -> Self {
        Self {
            status: StageStatus::Ok,
            duration_ms: None,
            warnings: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiler_errors: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageReports {
    pub fetch: StageReport,
    pub build: StageReport,
    pub run: StageReport,
}

/// The sole contract between untrusted execution and scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub schema_version: u32,
    pub assignment_id: AssignmentId,
    pub student_id: StudentId,
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graded_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructor_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_harness_commit: Option<String>,
    pub stages: StageReports,
    pub tests: Vec<TestResult>,
    #[serde(default)]
    pub resource_usage: ResourceUsage,
    #[serde(default)]
    pub diagnostics: Diagnostics,
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
    /// Set when a manual `overrides.toml` entry replaced the
    /// policy-computed score for this student -- recorded here, never by
    /// mutating the persisted raw `EvaluationResult`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_reason: Option<String>,
    /// Set when a late-penalty policy docked this student's score; the
    /// percentage actually deducted (already reflected in `score`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_penalty_percent: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            assignment_id: AssignmentId::new("hw3"),
            student_id: StudentId::new("alice"),
            run_id: RunId::new("2026-07-17T18-03-00Z-ab12"),
            graded_commit: Some("a1b2c3d".into()),
            instructor_commit: Some("f9e8d7".into()),
            public_harness_commit: Some("c0ffee".into()),
            stages: StageReports {
                fetch: StageReport::ok(),
                build: StageReport {
                    status: StageStatus::Ok,
                    duration_ms: Some(8123),
                    warnings: Some(3),
                },
                run: StageReport {
                    status: StageStatus::Ok,
                    duration_ms: Some(420),
                    warnings: None,
                },
            },
            tests: vec![
                TestResult {
                    name: "insert_basic".into(),
                    status: TestStatus::Pass,
                    duration_ms: Some(5),
                    message: None,
                    reported_score: Some(0.83),
                },
                TestResult {
                    name: "balance_adv".into(),
                    status: TestStatus::Fail,
                    duration_ms: Some(9),
                    message: Some("assertion failed: height <= 2*log2(n)".into()),
                    reported_score: None,
                },
                TestResult {
                    name: "delete_edge".into(),
                    status: TestStatus::Timeout,
                    duration_ms: None,
                    message: None,
                    reported_score: None,
                },
            ],
            resource_usage: ResourceUsage {
                peak_memory_bytes: Some(41231872),
                cpu_ms: Some(380),
            },
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
