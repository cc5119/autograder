pub mod binary_harness;
pub mod linked_library;

use crate::error::Result;
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    TestResult, TestStatus,
};
use crate::sandbox::SandboxLimits;
use crate::spec::{BuildLimits, RunLimits, ScoredTest};

/// Shared by both evaluators: `[limits.build]` carries no `max-output-bytes`
/// of its own (design §5.3), so compiler output reuses the run stage's cap.
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

/// Turns a prepared workspace into a raw evaluation result. Real impls
/// (`LinkedLibrary`, `BinaryHarness`, M2/M4) launch a trusted judge process
/// against a `Sandbox`; that seam is added in M2 (design §4, §9).
pub trait Evaluator {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult>;
}

/// Emits a well-formed `EvaluationResult` without executing any student
/// code, so the Fetch -> Prepare -> Evaluate -> Grade -> Report chain can
/// be wired and tested end-to-end before a real sandboxed evaluator lands.
/// Every scored test is reported as passing.
pub struct StubEvaluator {
    pub tests: Vec<ScoredTest>,
}

impl Evaluator for StubEvaluator {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
        let tests = self
            .tests
            .iter()
            .map(|t| TestResult {
                name: t.name.clone(),
                visibility: t.visibility,
                status: TestStatus::Pass,
                duration_ms: Some(1),
                message: None,
            })
            .collect();

        Ok(EvaluationResult {
            schema_version: 1,
            tier: ctx.tier,
            assignment_id: ctx.assignment_id.clone(),
            student_id: ctx.student_id.clone(),
            run_id: ctx.run_id.clone(),
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
    use crate::model::{TestVisibility, Tier};
    use std::path::PathBuf;

    #[test]
    fn stub_evaluator_reports_every_scored_test_as_passing() {
        let evaluator = StubEvaluator {
            tests: vec![ScoredTest {
                name: "insert_basic".into(),
                visibility: TestVisibility::Public,
                points: Some(10.0),
            }],
        };
        let ctx = JobContext {
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            tier: Tier::Authoritative,
            workspace: PathBuf::from("/tmp/does-not-matter"),
            driver_dir: PathBuf::from("/tmp/does-not-matter-either"),
        };

        let eval = evaluator.evaluate(&ctx).unwrap();
        assert_eq!(eval.tests.len(), 1);
        assert_eq!(eval.tests[0].status, TestStatus::Pass);
        assert_eq!(eval.stages.build.status, crate::model::StageStatus::Ok);
    }
}
