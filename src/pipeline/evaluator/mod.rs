pub mod nextest;

use std::path::Path;

use crate::error::Result;
use crate::exec::sandbox::{Mount, MountMode, SandboxLimits};
use crate::model::{Diagnostics, EvalStatus, EvaluationResult, JobContext, RunStatus, TestResult};
use crate::spec::BuildLimits;

/// Applied to the two build stages only -- the run stage gets no
/// `SandboxLimits` at all (see `Profile::IsolateRun`), so there's no second
/// tier to map here.
pub(crate) fn sandbox_limits(limits: &BuildLimits) -> SandboxLimits {
    SandboxLimits {
        wall_clock: limits.wall_clock.0,
        cpus: limits.cpus,
        memory_bytes: limits.memory.0,
        pids: limits.pids,
        max_output_bytes: limits.max_output_bytes.0,
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
/// Read-only mounting stops writes, not reads: a `build.rs` executing here
/// could otherwise just read hidden tests off disk while compiling. Only
/// stage 1 (building `<id>`) needs this, since `<id>`'s own `build.rs` is
/// the thing that could read it; stage 2 and the run stage use plain
/// [`repo_root_mounts`] instead.
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

/// Turns a prepared workspace into a raw evaluation result. The real impl
/// (`nextest::Nextest`) launches a trusted judge process against a
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
            submission_id: ctx.submission_id,
            run_id: ctx.run_id,
            graded_commit: None,
            instructor_commit: None,
            status: EvalStatus::Ran(RunStatus::Ok),
            tests,
            cpu_ms: None,
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
                duration_ms: 1,
                message: None,
                reported_score: None,
            }],
        };
        let ctx = JobContext {
            assignment_id: "hw3".into(),
            submission_id: "alice".into(),
            run_id: "run-1".into(),
            workspace: PathBuf::from("/tmp/does-not-matter"),
        };

        let eval = evaluator.evaluate(&ctx).unwrap();
        assert_eq!(eval.tests.len(), 1);
        assert_eq!(eval.tests[0].status, TestStatus::Pass);
        assert!(matches!(
            eval.status,
            crate::model::EvalStatus::Ran(crate::model::RunStatus::Ok)
        ));
    }
}
