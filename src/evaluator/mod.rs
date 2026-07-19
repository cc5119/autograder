pub mod binary;
pub mod library;

use std::path::Path;

use crate::error::{Error, Result};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports, TestResult,
};
use crate::sandbox::{Mount, MountMode, SandboxLimits};
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

/// Reads `[package].name` out of a harness manifest -- both `Library` and
/// `Binary` need this, since the harness's own crate name is
/// instructor-chosen and not necessarily `"harness"` (mirrors
/// `publish::validate_manifest`'s parse of the student's own manifest).
pub(crate) fn harness_package_name(harness_manifest: &Path) -> Result<String> {
    let contents = crate::fs::read_to_string(harness_manifest)?;
    let value: toml::Value = toml::from_str(&contents).map_err(|source| Error::Toml {
        path: harness_manifest.to_path_buf(),
        source: Box::new(source),
    })?;
    value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            Error::InvalidSpec(format!(
                "{} has no [package].name",
                harness_manifest.display()
            ))
        })
}

/// `repo_root` (containing both `workspace` and `harness/`) is mounted
/// read-write, since a workspace build's `Cargo.lock`/`target/` land at
/// its root, not under `harness/`. `workspace` is then mounted again, more
/// specifically and read-only, which shadows just that subtree of the
/// broader mount -- the student's own submitted source must never be
/// writable inside the sandbox, even though the judge crate and build
/// artifacts around it are. `vendor_dir`, if it exists on disk, is mounted
/// read-only alongside (both evaluators' offline-vendoring layout).
pub(crate) fn repo_root_mounts(
    repo_root: &Path,
    workspace: &Path,
    vendor_dir: &Path,
) -> Vec<Mount> {
    let mut mounts = vec![
        Mount {
            host_path: repo_root.to_path_buf(),
            container_path: repo_root.to_path_buf(),
            mode: MountMode::ReadWrite,
        },
        Mount {
            host_path: workspace.to_path_buf(),
            container_path: workspace.to_path_buf(),
            mode: MountMode::ReadOnly,
        },
    ];
    if vendor_dir.is_dir() {
        mounts.push(Mount {
            host_path: vendor_dir.to_path_buf(),
            container_path: vendor_dir.to_path_buf(),
            mode: MountMode::ReadOnly,
        });
    }
    mounts
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
        };

        let eval = evaluator.evaluate(&ctx).unwrap();
        assert_eq!(eval.tests.len(), 1);
        assert_eq!(eval.tests[0].status, TestStatus::Pass);
        assert_eq!(eval.stages.build.status, crate::model::StageStatus::Ok);
    }
}
