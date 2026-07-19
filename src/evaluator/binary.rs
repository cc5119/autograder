//! The `binary` `Evaluator`: the student repo builds a **binary** (target
//! named `[assignment].id`); the trusted judge -- instructor-authored
//! integration tests -- spawns it as a child and asserts on its observable
//! behavior (stdout/files/exit code), never on anything the student's own
//! process self-reports.
//!
//! Unlike `library`, there is no separate driver crate: the judge tests
//! live directly inside `[assignment].id`'s own package (`tests/*.rs`),
//! both in the private repo (every test) and the published starter
//! (`publish` derives just the public subset). A plain `cargo build`
//! produces the binary and `cargo nextest run` builds+runs `tests/` as
//! part of the *same* package, which is what lets nextest locate the
//! built binary via `env!("CARGO_BIN_EXE_<name>")` with no extra wiring --
//! that env var only populates for a package's own binaries, never across
//! a dependency edge, which is why there's no harness crate here the way
//! there is for `library`. Build and run both happen directly in
//! `ctx.workspace`, so `Prepare`'s `.cargo/config.toml` is discovered by
//! Cargo's ordinary directory-based lookup -- no `--config` flags needed,
//! unlike `library`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus,
};
use crate::sandbox::{Mount, MountMode, Sandbox, SandboxLimits, SandboxOutcome, SandboxSpec};
use crate::spec::{ScoredTest, Spec};

use super::library::parse_junit_report;
use super::{Evaluator, build_sandbox_limits, run_sandbox_limits};

const VENDOR_DIR_NAME: &str = "vendor";

pub struct Binary<S> {
    sandbox: S,
    package_dir: PathBuf,
    build_limits: SandboxLimits,
    run_limits: SandboxLimits,
    tests: Vec<ScoredTest>,
}

impl<S: Sandbox> Binary<S> {
    pub fn new(spec: &Spec, package_dir: impl Into<PathBuf>, sandbox: S) -> Result<Self> {
        let package_dir = package_dir.into();
        let tests_dir = package_dir.join(&spec.assignment.id).join("tests");
        if !tests_dir.is_dir() {
            return Err(Error::InvalidSpec(format!(
                "binary assignment missing {} -- the judge must be real, \
                 instructor-authored integration tests (no default is generated)",
                tests_dir.display()
            )));
        }
        Ok(Self {
            sandbox,
            package_dir,
            build_limits: build_sandbox_limits(&spec.limits.build, &spec.limits.run),
            run_limits: run_sandbox_limits(&spec.limits.run),
            tests: spec.scoring.tests.clone(),
        })
    }

    fn vendor_dir(&self) -> PathBuf {
        self.package_dir.join(VENDOR_DIR_NAME)
    }

    fn offline_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        if self.vendor_dir().is_dir() {
            env.insert("CARGO_NET_OFFLINE".to_string(), "true".to_string());
        }
        env
    }

    fn mounts(&self, workspace: &std::path::Path) -> Vec<Mount> {
        let mut mounts = vec![Mount {
            host_path: workspace.to_path_buf(),
            container_path: workspace.to_path_buf(),
            mode: MountMode::ReadWrite,
        }];
        let vendor_dir = self.vendor_dir();
        if vendor_dir.is_dir() {
            mounts.push(Mount {
                host_path: vendor_dir.clone(),
                container_path: vendor_dir,
                mode: MountMode::ReadOnly,
            });
        }
        mounts
    }
}

impl<S: Sandbox> Evaluator for Binary<S> {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
        let workspace = &ctx.workspace;
        let env = self.offline_env();
        let mounts = self.mounts(workspace);

        let mut build_spec = SandboxSpec::new("cargo", self.build_limits.clone());
        build_spec.args = vec!["build".into(), "--offline".into()];
        build_spec.workdir = Some(workspace.clone());
        build_spec.env = env.clone();
        build_spec.mounts = mounts.clone();

        let build_outcome = self.sandbox.run(&build_spec)?;
        if !build_outcome.succeeded() {
            return Ok(terminal_result(
                ctx,
                Stage::Build,
                build_stage_status(&build_outcome),
                Diagnostics {
                    compiler_errors: Some(capped_utf8(&build_outcome.stderr)),
                    stderr_excerpt: None,
                },
            ));
        }

        let mut run_spec = SandboxSpec::new("cargo", self.run_limits.clone());
        run_spec.args = vec!["nextest".into(), "run".into(), "--offline".into()];
        run_spec.workdir = Some(workspace.clone());
        run_spec.env = env;
        run_spec.mounts = mounts;

        let run_outcome = self.sandbox.run(&run_spec)?;
        if run_outcome.timed_out {
            return Ok(terminal_result(
                ctx,
                Stage::Run,
                StageStatus::Timeout,
                run_diagnostics(&run_outcome),
            ));
        }
        if run_outcome.oom {
            return Ok(terminal_result(
                ctx,
                Stage::Run,
                StageStatus::Oom,
                run_diagnostics(&run_outcome),
            ));
        }

        let junit_path = workspace.join("target/nextest/default/junit.xml");
        let Ok(xml) = std::fs::read_to_string(&junit_path) else {
            return Ok(terminal_result(
                ctx,
                Stage::Run,
                StageStatus::HarnessError,
                run_diagnostics(&run_outcome),
            ));
        };
        let tests = parse_junit_report(&xml, &self.tests)?;
        let diagnostics = run_diagnostics(&run_outcome);

        Ok(EvaluationResult {
            schema_version: 1,
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
            resource_usage: run_outcome.resource_usage,
            diagnostics,
        })
    }
}

enum Stage {
    Build,
    Run,
}

fn terminal_result(
    ctx: &JobContext,
    stage: Stage,
    status: StageStatus,
    diagnostics: Diagnostics,
) -> EvaluationResult {
    EvaluationResult {
        schema_version: 1,
        assignment_id: ctx.assignment_id.clone(),
        student_id: ctx.student_id.clone(),
        run_id: ctx.run_id.clone(),
        graded_commit: None,
        instructor_commit: None,
        public_harness_commit: None,
        stages: StageReports {
            fetch: StageReport::ok(),
            build: match stage {
                Stage::Build => StageReport {
                    status,
                    duration_ms: None,
                    warnings: None,
                },
                Stage::Run => StageReport::ok(),
            },
            run: match stage {
                Stage::Run => StageReport {
                    status,
                    duration_ms: None,
                    warnings: None,
                },
                Stage::Build => StageReport::ok(),
            },
        },
        tests: Vec::new(),
        resource_usage: ResourceUsage::default(),
        diagnostics,
    }
}

fn build_stage_status(outcome: &SandboxOutcome) -> StageStatus {
    if outcome.timed_out {
        StageStatus::Timeout
    } else if outcome.oom {
        StageStatus::Oom
    } else {
        StageStatus::BuildFailed
    }
}

fn run_diagnostics(outcome: &SandboxOutcome) -> Diagnostics {
    Diagnostics {
        compiler_errors: None,
        stderr_excerpt: Some(capped_utf8(&outcome.stderr)),
    }
}

fn capped_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TestStatus;
    use std::sync::Mutex;

    const SAMPLE_JUNIT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="harness" tests="1" failures="0" errors="0">
    <testcase name="echoes_input" time="0.004"/>
  </testsuite>
</testsuites>
"#;

    struct ScriptedSandbox {
        outcomes: Mutex<Vec<SandboxOutcome>>,
    }

    impl ScriptedSandbox {
        fn new(outcomes: Vec<SandboxOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
            }
        }
    }

    impl Sandbox for ScriptedSandbox {
        fn run(&self, _spec: &SandboxSpec) -> Result<SandboxOutcome> {
            Ok(self.outcomes.lock().unwrap().remove(0))
        }
    }

    fn ok_outcome() -> SandboxOutcome {
        SandboxOutcome {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
            oom: false,
            resource_usage: ResourceUsage::default(),
        }
    }

    fn failed_outcome() -> SandboxOutcome {
        SandboxOutcome {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"error[E0433]: failed to resolve".to_vec(),
            timed_out: false,
            oom: false,
            resource_usage: ResourceUsage::default(),
        }
    }

    fn spec() -> Spec {
        let toml = r#"
[assignment]
id = "wc"
name = "Word count"
kind = "binary"
deadline = "2026-02-14T23:59:59-08:00"

[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]

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
model = "weighted"

[[scoring.tests]]
name = "echoes_input"
points = 10
visibility = "public"
"#;
        toml::from_str(toml).unwrap()
    }

    fn ctx(workspace: PathBuf) -> JobContext {
        JobContext {
            assignment_id: "wc".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            workspace,
            driver_dir: PathBuf::from("/tmp/unused-for-binary"),
        }
    }

    fn write_judge_tests(package_dir: &std::path::Path) {
        std::fs::create_dir_all(package_dir.join("wc/tests")).unwrap();
        std::fs::write(
            package_dir.join("wc/tests/judge.rs"),
            "#[test]\nfn echoes_input() {}\n",
        )
        .unwrap();
    }

    #[test]
    fn new_errors_clearly_when_the_judge_tests_are_missing() {
        let package_dir = tempfile::tempdir().unwrap();

        let result = Binary::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![]));

        assert!(matches!(result, Err(Error::InvalidSpec(_))));
    }

    #[test]
    fn build_failure_short_circuits_before_running_nextest() {
        let package_dir = tempfile::tempdir().unwrap();
        write_judge_tests(package_dir.path());
        let workspace = tempfile::tempdir().unwrap();

        let sandbox = ScriptedSandbox::new(vec![failed_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator
            .evaluate(&ctx(workspace.path().to_path_buf()))
            .unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::BuildFailed);
        assert!(eval.diagnostics.compiler_errors.unwrap().contains("E0433"));
        assert!(eval.tests.is_empty());
    }

    #[test]
    fn missing_junit_report_after_a_successful_run_is_a_harness_error() {
        let package_dir = tempfile::tempdir().unwrap();
        write_judge_tests(package_dir.path());
        let workspace = tempfile::tempdir().unwrap();

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator
            .evaluate(&ctx(workspace.path().to_path_buf()))
            .unwrap();

        assert_eq!(eval.stages.run.status, StageStatus::HarnessError);
    }

    #[test]
    fn a_junit_report_on_disk_is_parsed_into_the_eval_result() {
        let package_dir = tempfile::tempdir().unwrap();
        write_judge_tests(package_dir.path());
        let workspace = tempfile::tempdir().unwrap();
        let junit_path = workspace.path().join("target/nextest/default/junit.xml");
        std::fs::create_dir_all(junit_path.parent().unwrap()).unwrap();
        std::fs::write(&junit_path, SAMPLE_JUNIT).unwrap();

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator
            .evaluate(&ctx(workspace.path().to_path_buf()))
            .unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::Ok);
        assert_eq!(eval.stages.run.status, StageStatus::Ok);
        assert_eq!(eval.tests.len(), 1);
        assert_eq!(eval.tests[0].status, TestStatus::Pass);
    }

    #[test]
    fn build_and_run_mount_the_workspace_read_write_not_read_only() {
        let package_dir = tempfile::tempdir().unwrap();
        write_judge_tests(package_dir.path());
        let evaluator =
            Binary::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![])).unwrap();

        let mounts = evaluator.mounts(std::path::Path::new("/tmp/some-workspace"));
        let workspace_mount = mounts
            .iter()
            .find(|m| m.container_path == std::path::Path::new("/tmp/some-workspace"))
            .unwrap();
        assert_eq!(workspace_mount.mode, MountMode::ReadWrite);
    }
}
