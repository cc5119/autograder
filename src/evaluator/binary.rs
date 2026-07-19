//! The `binary` `Evaluator`: the student repo builds a **binary** (target
//! named `[assignment].id`); the trusted judge -- instructor-authored
//! integration tests -- spawns it as a child and asserts on its observable
//! behavior (stdout/files/exit code), never on anything the student's own
//! process self-reports.
//!
//! Mirrors `library`'s shape: the judge lives permanently in a separate
//! `harness/` package, a sibling of `workspace` (`[assignment].id`'s own
//! crate) under one shared `repo_root`, never generated or rewritten by
//! this tool. Build and run both happen with `workdir = repo_root`,
//! `-p <harness_package>` (`harness/Cargo.toml`'s own `[package].name`,
//! read once at construction) for `run`, and `-p <harness_package> -p
//! <id>` for `build` -- unlike `library`, there's no Cargo dependency edge
//! from `harness` to `workspace` (a plain `[dependencies]` entry can only
//! link a *library* target, and `workspace` here is bin-only; Cargo's
//! artifact-dependencies feature would fix this but is still nightly-gated
//! as of this writing), so `build` must name both packages explicitly to
//! get `workspace`'s binary built at all. `run`'s `-p <harness_package>`
//! alone is what keeps a same-named decoy test in the student's own crate
//! from ever executing (`grade` trusts every test an `eval` reports with
//! no name allowlist -- see `attic/same-name-test-target-repro` for a
//! working repro of exactly this with no package scoping).
//!
//! How the harness's own test code then locates and invokes the built
//! binary (`env!("CARGO_BIN_EXE_<name>")` only resolves within a test's
//! *own* package, never across a dependency edge, so it can't be used
//! here) is entirely up to the instructor's own code -- a hand-computed
//! path via `env!("CARGO_MANIFEST_DIR")` into the shared `target/` dir, the
//! `escargot` crate, a `build.rs`, whatever. This module has no opinion and
//! needs none: either a bare `cargo test` (a student/instructor working
//! without `autograder` at all) or this evaluator's own `-p`-scoped
//! invocation already guarantees a complete, ordinary build has happened
//! before any test runs, which is all any of those techniques need.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus,
};
use crate::sandbox::{Mount, Sandbox, SandboxLimits, SandboxOutcome, SandboxSpec};
use crate::spec::Spec;

use super::library::parse_junit_report;
use super::{
    Evaluator, build_sandbox_limits, harness_package_name, run_sandbox_limits, write_nextest_config,
};

const VENDOR_DIR_NAME: &str = "vendor";

pub struct Binary<S> {
    sandbox: S,
    package_dir: PathBuf,
    build_limits: SandboxLimits,
    run_limits: SandboxLimits,
    /// `[package].name` from `harness/Cargo.toml` -- see this module's doc
    /// comment for why it isn't assumed to be `"harness"`.
    harness_package: String,
}

impl<S: Sandbox> Binary<S> {
    pub fn new(spec: &Spec, package_dir: impl Into<PathBuf>, sandbox: S) -> Result<Self> {
        let package_dir = package_dir.into();
        let harness_manifest = package_dir.join("harness/Cargo.toml");
        if !harness_manifest.is_file() {
            return Err(Error::InvalidSpec(format!(
                "binary assignment missing {} -- the harness must be a real, \
                 instructor-authored crate (no default is generated)",
                harness_manifest.display()
            )));
        }
        let harness_package = harness_package_name(&harness_manifest)?;
        Ok(Self {
            sandbox,
            package_dir,
            build_limits: build_sandbox_limits(&spec.limits.build, &spec.limits.run),
            run_limits: run_sandbox_limits(&spec.limits.run),
            harness_package,
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

    fn mounts(&self, repo_root: &Path, workspace: &Path) -> Vec<Mount> {
        super::repo_root_mounts(repo_root, workspace, &self.vendor_dir())
    }
}

impl<S: Sandbox> Evaluator for Binary<S> {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
        let repo_root = ctx
            .workspace
            .parent()
            .expect(
                "workspace always has a parent (repo_root); see model.rs's JobContext doc comment",
            )
            .to_path_buf();
        let env = self.offline_env();
        let mounts = self.mounts(&repo_root, &ctx.workspace);

        // `build` names both packages explicitly (no dependency edge ties
        // them together, see this module's doc comment); `run` only ever
        // selects `harness`, so the student's own crate's tests -- if it
        // even has any -- never execute.
        let mut build_spec = SandboxSpec::new("cargo", self.build_limits.clone());
        build_spec.args = vec![
            "build".into(),
            "--offline".into(),
            "-p".into(),
            self.harness_package.clone(),
            "-p".into(),
            ctx.assignment_id.to_string(),
        ];
        build_spec.workdir = Some(repo_root.clone());
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

        write_nextest_config(&repo_root)?;

        let mut run_spec = SandboxSpec::new("cargo", self.run_limits.clone());
        run_spec.args = vec![
            "nextest".into(),
            "run".into(),
            "--offline".into(),
            "-p".into(),
            self.harness_package.clone(),
        ];
        run_spec.workdir = Some(repo_root.clone());
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

        let junit_path = repo_root.join("target/nextest/default/junit.xml");
        let Ok(xml) = std::fs::read_to_string(&junit_path) else {
            return Ok(terminal_result(
                ctx,
                Stage::Run,
                StageStatus::HarnessError,
                run_diagnostics(&run_outcome),
            ));
        };
        let tests = parse_junit_report(&xml)?;
        let diagnostics = run_diagnostics(&run_outcome);

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
        assignment_id: ctx.assignment_id,
        student_id: ctx.student_id,
        run_id: ctx.run_id,
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
        specs: std::sync::Arc<Mutex<Vec<SandboxSpec>>>,
    }

    impl ScriptedSandbox {
        fn new(outcomes: Vec<SandboxOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                specs: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn spy(outcomes: Vec<SandboxOutcome>) -> (Self, std::sync::Arc<Mutex<Vec<SandboxSpec>>>) {
            let sandbox = Self::new(outcomes);
            let specs = sandbox.specs.clone();
            (sandbox, specs)
        }
    }

    impl Sandbox for ScriptedSandbox {
        fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome> {
            self.specs.lock().unwrap().push(spec.clone());
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
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"

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
formula = "sum"
base = 0.0
"#;
        toml::from_str(toml).unwrap()
    }

    fn ctx(workspace: PathBuf) -> JobContext {
        JobContext {
            assignment_id: "wc".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            workspace,
        }
    }

    /// `Binary::new` requires a real `harness/Cargo.toml` to exist.
    fn write_harness_manifest(package_dir: &Path) {
        std::fs::create_dir_all(package_dir.join("harness")).unwrap();
        std::fs::write(
            package_dir.join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
    }

    /// `workspace` and `harness/` as real siblings under one `repo_root`,
    /// matching the invariant `Binary::evaluate` relies on in production
    /// (see this module's doc comment).
    fn job_dirs(repo_root: &Path) -> (PathBuf, PathBuf) {
        let workspace = repo_root.join("wc");
        let harness_dir = repo_root.join("harness");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&harness_dir).unwrap();
        (workspace, harness_dir)
    }

    #[test]
    fn new_errors_clearly_when_the_harness_is_missing() {
        let package_dir = tempfile::tempdir().unwrap();

        let result = Binary::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![]));

        assert!(matches!(result, Err(Error::InvalidSpec(_))));
    }

    #[test]
    fn new_errors_clearly_when_the_harness_manifest_has_no_package_name() {
        let package_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(package_dir.path().join("harness")).unwrap();
        std::fs::write(package_dir.path().join("harness/Cargo.toml"), "").unwrap();

        let result = Binary::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![]));

        assert!(matches!(result, Err(Error::InvalidSpec(_))));
    }

    #[test]
    fn build_failure_short_circuits_before_running_nextest() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());

        let sandbox = ScriptedSandbox::new(vec![failed_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace)).unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::BuildFailed);
        assert!(eval.diagnostics.compiler_errors.unwrap().contains("E0433"));
        assert!(eval.tests.is_empty());
    }

    #[test]
    fn missing_junit_report_after_a_successful_run_is_a_harness_error() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace)).unwrap();

        assert_eq!(eval.stages.run.status, StageStatus::HarnessError);
    }

    #[test]
    fn a_junit_report_on_disk_is_parsed_into_the_eval_result() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());
        // Workspace builds land `target/` at the workspace root, not under
        // `harness/` -- see this module's doc comment.
        let junit_path = repo_root.path().join("target/nextest/default/junit.xml");
        std::fs::create_dir_all(junit_path.parent().unwrap()).unwrap();
        std::fs::write(&junit_path, SAMPLE_JUNIT).unwrap();

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace)).unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::Ok);
        assert_eq!(eval.stages.run.status, StageStatus::Ok);
        assert_eq!(eval.tests.len(), 1);
        assert_eq!(eval.tests[0].status, TestStatus::Pass);
    }

    #[test]
    fn build_names_both_packages_and_run_scopes_to_only_the_harness() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());
        let junit_path = repo_root.path().join("target/nextest/default/junit.xml");
        std::fs::create_dir_all(junit_path.parent().unwrap()).unwrap();
        std::fs::write(&junit_path, SAMPLE_JUNIT).unwrap();

        let (sandbox, specs) = ScriptedSandbox::spy(vec![ok_outcome(), ok_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace)).unwrap();

        let specs = specs.lock().unwrap();
        let build_spec = &specs[0];
        let run_spec = &specs[1];
        assert_eq!(build_spec.workdir.as_deref(), Some(repo_root.path()));
        // `write_harness_manifest` names the harness crate "driver", not
        // "harness" -- confirms the real `[package].name` is used, not the
        // directory name.
        assert_eq!(
            build_spec.args,
            vec!["build", "--offline", "-p", "driver", "-p", "wc"]
        );
        assert_eq!(run_spec.workdir.as_deref(), Some(repo_root.path()));
        assert_eq!(
            run_spec.args,
            vec!["nextest", "run", "--offline", "-p", "driver"]
        );
    }

    #[test]
    fn build_and_run_mount_repo_root_read_write_and_workspace_read_only() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let evaluator =
            Binary::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![])).unwrap();

        let repo_root = std::path::Path::new("/tmp/some-repo-root");
        let workspace = repo_root.join("wc");
        let mounts = evaluator.mounts(repo_root, &workspace);

        let repo_root_mount = mounts
            .iter()
            .find(|m| m.container_path == repo_root)
            .unwrap();
        assert_eq!(repo_root_mount.mode, crate::sandbox::MountMode::ReadWrite);

        let workspace_mount = mounts
            .iter()
            .find(|m| m.container_path == workspace)
            .unwrap();
        assert_eq!(workspace_mount.mode, crate::sandbox::MountMode::ReadOnly);
    }
}
