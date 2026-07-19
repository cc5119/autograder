pub mod binary;
pub mod library;

use std::path::Path;

use crate::error::Result;
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports, TestResult,
};
use crate::sandbox::SandboxLimits;
use crate::spec::{BuildLimits, RunLimits};

/// Shared by both evaluators: `[limits.build]` carries no `max-output-bytes`
/// of its own, so compiler output reuses the run stage's cap.
pub(crate) fn build_sandbox_limits(build: &BuildLimits, run: &RunLimits) -> SandboxLimits {
    SandboxLimits {
        wall_clock: build.wall_clock.0,
        cpus: build.cpus,
        memory_bytes: build.memory.0,
        pids: build.pids,
        max_output_bytes: run.max_output_bytes.0,
    }
}

pub(crate) fn run_sandbox_limits(run: &RunLimits) -> SandboxLimits {
    SandboxLimits {
        wall_clock: run.wall_clock.0,
        cpus: run.cpus,
        memory_bytes: run.memory.0,
        pids: run.pids,
        max_output_bytes: run.max_output_bytes.0,
    }
}

/// Writes `dir/.config/nextest.toml` with `store-success-output = true`, so
/// a *passing* test's stdout still reaches the JUnit report's
/// `<system-out>` -- required to see `autograder: score=` lines from tests
/// that pass with partial credit, not just failing ones. Written by the
/// trusted evaluator itself (like `prepare`'s `.cargo/config.toml`), never
/// checked into the student- or instructor-authored harness, so grading
/// never depends on either remembering to set it.
pub(crate) fn write_nextest_config(dir: &Path) -> Result<()> {
    let config_dir = dir.join(".config");
    crate::fs::create_dir_all(&config_dir)?;
    crate::fs::write(
        &config_dir.join("nextest.toml"),
        "[profile.default]\nstore-success-output = true\n",
    )
}

/// Turns a prepared workspace into a raw evaluation result. Real impls
/// (`Library`, `Binary`) launch a trusted judge process against a
/// `Sandbox`.
pub trait Evaluator {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult>;
}

/// Emits a well-formed `EvaluationResult` without executing any student
/// code, so the Fetch -> Prepare -> Evaluate -> Grade -> Report chain can
/// be wired and tested end-to-end before a real sandboxed evaluator lands.
/// Reports exactly the `TestResult`s it's given, verbatim.
pub struct StubEvaluator {
    pub tests: Vec<TestResult>,
}

impl Evaluator for StubEvaluator {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
        let tests = self.tests.clone();

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
            tests,
            resource_usage: ResourceUsage::default(),
            diagnostics: Diagnostics::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TestStatus;
    use std::path::PathBuf;

    #[test]
    fn stub_evaluator_reports_the_given_tests_verbatim() {
        let evaluator = StubEvaluator {
            tests: vec![TestResult {
                name: "insert_basic".into(),
                status: TestStatus::Pass,
                duration_ms: Some(1),
                message: None,
                reported_score: None,
            }],
        };
        let ctx = JobContext {
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            workspace: PathBuf::from("/tmp/does-not-matter"),
            driver_dir: PathBuf::from("/tmp/does-not-matter-either"),
        };

        let eval = evaluator.evaluate(&ctx).unwrap();
        assert_eq!(eval.tests.len(), 1);
        assert_eq!(eval.tests[0].status, TestStatus::Pass);
        assert_eq!(eval.stages.build.status, crate::model::StageStatus::Ok);
    }
}
