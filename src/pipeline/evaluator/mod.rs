pub mod binary;
pub mod library;

use std::path::Path;

use crate::error::Result;
use crate::exec::sandbox::{Mount, MountMode, SandboxLimits};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports, TestResult,
};
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
/// that pass with partial credit, not just failing ones. Also enables the
/// JUnit report itself (`[profile.default.junit]`) -- nextest never writes
/// `target/nextest/<profile>/junit.xml` unless a config says to, and every
/// evaluator reads that exact path. Written by the trusted evaluator itself
/// (like `prepare`'s `.cargo/config.toml`), never checked into the
/// student- or instructor-authored harness, so grading never depends on
/// either remembering to set it.
pub(crate) fn write_nextest_config(dir: &Path) -> Result<()> {
    let config_dir = dir.join(".config");
    crate::exec::fs::create_dir_all(&config_dir)?;
    crate::exec::fs::write(
        &config_dir.join("nextest.toml"),
        "[profile.default]\nstore-success-output = true\n\n\
         [profile.default.junit]\npath = \"junit.xml\"\n",
    )
}

/// `repo_root` (containing `workspace`, `harness_dir`, `Cargo.lock` and
/// `target/`) is mounted read-write, since a workspace build's
/// `Cargo.lock`/`target/` land at its root, not under `harness_dir`.
/// `workspace` and `harness_dir` are then each mounted again, more
/// specifically and read-only, which shadows just those subtrees of the
/// broader mount: the student's own submitted source must never be
/// writable inside the sandbox (even though the judge crate and build
/// artifacts around it are), and neither must the instructor's trusted
/// judge -- otherwise student-triggered code (a `build.rs`, a proc macro)
/// that runs before the judge is compiled could overwrite the judge's
/// source and forge its own grade. `vendor_dir`, if it exists on disk, is mounted read-only
/// alongside (both evaluators' offline-vendoring layout).
///
/// Only safe for a container in which no student-authored code ever
/// executes (see [`hidden_tests_mounts`] otherwise) -- `harness_dir/tests`
/// is fully visible here, hidden (non-`keep`) adversarial tests included.
pub(crate) fn repo_root_mounts(
    repo_root: &Path,
    workspace: &Path,
    harness_dir: &Path,
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
        Mount {
            host_path: harness_dir.to_path_buf(),
            container_path: harness_dir.to_path_buf(),
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

/// A shared, permanently-empty host directory to shadow a path a container
/// must see *something* mounted at, but nothing readable -- read-only, and
/// nothing ever writes into it, so bind-mounting the same host path into
/// many concurrent containers is safe.
fn empty_shadow_dir() -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join("autograder-empty-mount");
    crate::exec::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Like [`repo_root_mounts`], except `harness_dir/tests` -- the only
/// confidential part of the harness (`{harness}/src/**` already ships to
/// students unstripped via `publish`; only `{harness}/tests/**`'s
/// non-`keep` items are meant to stay hidden, see `package::publish`'s
/// module doc comment) -- is shadowed by an empty read-only directory.
///
/// Read-only mounting (what `repo_root_mounts` already does) stops writes,
/// not reads: any code that *executes* inside a container with the real
/// `harness/tests` mounted -- a student's `build.rs`, or (for `library`
/// assignments that don't split student code into a separate driver
/// process) the student's own library code running in-process during the
/// judge's tests -- can simply read hidden adversarial tests off disk
/// before/while they run, defeating the point of keeping them hidden. This
/// is for any container stage in which student-authored code executes
/// (the student build, and the archived judge run); [`repo_root_mounts`]
/// remains correct for the stage that only compiles/links the judge, since
/// nothing student-authored runs there.
pub(crate) fn hidden_tests_mounts(
    repo_root: &Path,
    workspace: &Path,
    harness_dir: &Path,
    vendor_dir: &Path,
) -> Result<Vec<Mount>> {
    let mut mounts = repo_root_mounts(repo_root, workspace, harness_dir, vendor_dir);
    mounts.push(Mount {
        host_path: empty_shadow_dir()?,
        container_path: harness_dir.join("tests"),
        mode: MountMode::ReadOnly,
    });
    Ok(mounts)
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
