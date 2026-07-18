use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A single student's submission as reported by a `SubmissionsSource`,
/// generic over the fetchable type `F`, which implements `Fetchable`
/// itself. A `SubmissionsSource<F>`'s submissions can only be fetched
/// through that same `F`'s `Fetchable` impl, so it is a compile error to
/// hand a `CsvRoster`'s (`GitRepo`-fetching) submissions to code that only
/// knows how to fetch a `LocalPath` — `fetchable` is a typed value, not an
/// untyped string whose meaning depends on which code happens to read it.
#[derive(Debug, Clone)]
pub struct Submission<F> {
    pub student_id: String,
    pub fetchable: F,
    pub metadata: BTreeMap<String, String>,
}

/// A `Fetchable` for a path on disk to copy wholesale into the job
/// workspace. Produced by `DirectorySource`.
#[derive(Debug, Clone)]
pub struct LocalPath(pub PathBuf);

/// A `Fetchable` for a future `GitHubFetcher` (M6): a clone URL plus an
/// optional pinned ref/branch override. Produced by `CsvRoster`.
#[derive(Debug, Clone)]
pub struct GitRepo {
    pub url: String,
    pub r#ref: Option<String>,
}

/// Which tier a run belongs to: instructor-authoritative or student-facing CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Authoritative,
    Ci,
}

/// Per-job context threaded through the pipeline stages.
#[derive(Debug, Clone)]
pub struct JobContext {
    pub assignment_id: String,
    pub student_id: String,
    pub run_id: String,
    pub tier: Tier,
    /// The student's checkout (fetch destination for `grade`; the current
    /// directory for `ci`). Never written into by an evaluator — only read,
    /// as the target of a patched-in dependency.
    pub workspace: PathBuf,
    /// A separate, fresh-per-job scratch directory where a `library`
    /// driver crate is overlaid and built — a *sibling* of `workspace`, not
    /// nested inside it, so nothing an evaluator builds ever lands inside
    /// the student's own checkout. Unused by `binary`, which builds
    /// the student's own binary directly in `workspace`.
    pub driver_dir: PathBuf,
}

/// Visibility of a test: public tests also run in the CI tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestVisibility {
    Public,
    Private,
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
    pub visibility: TestVisibility,
    pub status: TestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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

/// The sole contract between untrusted execution and scoring (design §12).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub schema_version: u32,
    pub tier: Tier,
    pub assignment_id: String,
    pub student_id: String,
    pub run_id: String,
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

/// Which named test in the failing/passing breakdown, for reporters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grade {
    pub student_id: String,
    pub score: f64,
    pub max: f64,
    pub status: String,
    #[serde(default)]
    pub failing_tests: Vec<String>,
    /// Set when a manual `overrides.toml` entry replaced the policy-computed
    /// score for this student (design §14: recorded on the `Grade`, never by
    /// mutating the persisted raw `EvaluationResult`). See `overrides.rs`.
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
            tier: Tier::Authoritative,
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "2026-07-17T18-03-00Z-ab12".into(),
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
                    visibility: TestVisibility::Public,
                    status: TestStatus::Pass,
                    duration_ms: Some(5),
                    message: None,
                },
                TestResult {
                    name: "balance_adv".into(),
                    visibility: TestVisibility::Private,
                    status: TestStatus::Fail,
                    duration_ms: Some(9),
                    message: Some("assertion failed: height <= 2*log2(n)".into()),
                },
                TestResult {
                    name: "delete_edge".into(),
                    visibility: TestVisibility::Public,
                    status: TestStatus::Timeout,
                    duration_ms: None,
                    message: None,
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
