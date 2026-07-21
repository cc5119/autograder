//! The `binary` `Evaluator`: the student repo builds a **binary** (target
//! named `[assignment].id`); the trusted judge -- instructor-authored
//! integration tests -- spawns it as a child and asserts on its observable
//! behavior (stdout/files/exit code), never on anything the student's own
//! process self-reports.
//!
//! Mirrors `library`'s shape (harness as a sibling package under
//! `repo_root`, `workdir = repo_root`), but since `workspace` is bin-only
//! there's no Cargo dependency edge to it (a plain `[dependencies]` entry
//! only links a library target), so `build` names both packages explicitly
//! (`-p <harness_package> -p <id>`) while `run` scopes to just
//! `-p <harness_package>` -- which is what keeps a same-named decoy test in
//! the student's own crate from ever executing (`grade` trusts every test
//! an `eval` reports with no name allowlist; see
//! `attic/same-name-test-target-repro`).
//!
//! How the harness locates the built binary (`env!("CARGO_BIN_EXE_<name>")`
//! doesn't work across a dependency edge) is entirely up to the
//! instructor's own code; this module has no opinion and needs none, since
//! its `-p`-scoped build always completes before any test runs.
//!
//! `evaluate` runs three sandboxed stages, not one, to keep hidden judge
//! test content out of any container in which student-authored code
//! executes (see `evaluator::hidden_tests_mounts`'s doc comment for why a
//! read-only mount alone doesn't do that):
//!
//! 1. **build student** -- `cargo build -p <id>` only, mounted via
//!    `hidden_tests_mounts` (harness/tests hidden). A student `build.rs`
//!    runs here with no hidden test source to read.
//! 2. **archive judge** -- `cargo nextest archive -p <harness>`, mounted
//!    via the full `repo_root_mounts` (harness fully visible). No
//!    dependency edge ties the two packages together, so this doesn't
//!    even need `<id>` to already be built -- only compiles the judge's
//!    own test binaries, so the real hidden tests being readable here is
//!    harmless.
//! 3. **run** -- `cargo nextest run --archive-file`, again via
//!    `hidden_tests_mounts`. This is where the judge actually spawns the
//!    compiled student binary as a child process, but the archive
//!    contains only compiled test binaries -- there's no hidden test
//!    source anywhere in this container to read regardless. `<id>`'s own
//!    source stays visible here (never shadowed, only ever
//!    `harness/tests` is): it was never confidential, and the
//!    instructor's own harness code may reasonably want to rebuild/locate
//!    it (see this module's doc comment on `CARGO_BIN_EXE_`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::exec::sandbox::{Mount, Sandbox, SandboxLimits, SandboxOutcome, SandboxSpec};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus,
};
use crate::spec::Spec;

use super::library::parse_junit_report;
use super::{Evaluator, build_sandbox_limits, run_sandbox_limits, write_nextest_config};

const VENDOR_DIR_NAME: &str = "vendor";

pub struct Binary<S> {
    sandbox: S,
    package_dir: PathBuf,
    build_limits: SandboxLimits,
    run_limits: SandboxLimits,
    /// `[assignment].harness` -- names both the harness's sibling directory
    /// and its own `[package].name`, which must agree. Cloned once at
    /// construction so `evaluate` can pass `-p <harness_package>`.
    harness_package: String,
}

impl<S: Sandbox> Binary<S> {
    pub fn new(spec: &Spec, package_dir: impl Into<PathBuf>, sandbox: S) -> Result<Self> {
        let package_dir = package_dir.into();
        let harness_manifest = package_dir
            .join(&spec.assignment.harness)
            .join("Cargo.toml");
        if !harness_manifest.is_file() {
            return Err(Error::InvalidSpec(format!(
                "binary assignment missing {} -- the harness must be a real, \
                 instructor-authored crate (no default is generated)",
                harness_manifest.display()
            )));
        }
        Ok(Self {
            sandbox,
            package_dir,
            build_limits: build_sandbox_limits(&spec.limits.build, &spec.limits.run),
            run_limits: run_sandbox_limits(&spec.limits.run),
            harness_package: spec.assignment.harness.clone(),
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

    fn harness_dir(&self, repo_root: &Path) -> PathBuf {
        repo_root.join(&self.harness_package)
    }

    /// Full visibility, harness/tests included -- only safe for the
    /// archive stage, where nothing student-authored executes (see this
    /// module's doc comment).
    fn full_mounts(&self, repo_root: &Path, workspace: &Path) -> Vec<Mount> {
        super::repo_root_mounts(
            repo_root,
            workspace,
            &self.harness_dir(repo_root),
            &self.vendor_dir(),
        )
    }

    /// harness/tests hidden -- for any stage in which student-authored code
    /// executes (the student build, and the archived judge run, which
    /// spawns the compiled student binary as a child).
    fn hidden_tests_mounts(&self, repo_root: &Path, workspace: &Path) -> Result<Vec<Mount>> {
        super::hidden_tests_mounts(
            repo_root,
            workspace,
            &self.harness_dir(repo_root),
            &self.vendor_dir(),
        )
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

        // Stage 1: build only the student's own crate. harness/tests is
        // hidden (see `hidden_tests_mounts`'s doc comment) -- a student
        // `build.rs` runs here with nothing hidden to read.
        let mut build_spec = SandboxSpec::new("cargo", self.build_limits.clone());
        build_spec.args = vec![
            "build".into(),
            "--offline".into(),
            "-p".into(),
            ctx.assignment_id.to_string(),
        ];
        build_spec.workdir = Some(repo_root.clone());
        build_spec.env = env.clone();
        build_spec.mounts = self.hidden_tests_mounts(&repo_root, &ctx.workspace)?;

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

        // Stage 2: compile + archive the judge, harness fully visible. No
        // dependency edge ties `harness` to `<id>` (see this module's doc
        // comment), so this doesn't execute or even need the student's
        // binary already built -- only the judge's own test binaries are
        // compiled here, so hidden test source being readable is harmless.
        let archive_path = repo_root.join("target/nextest-archive.tar.zst");
        let mut archive_spec = SandboxSpec::new("cargo", self.build_limits.clone());
        archive_spec.args = vec![
            "nextest".into(),
            "archive".into(),
            "--offline".into(),
            "-p".into(),
            self.harness_package.clone(),
            "--archive-file".into(),
            archive_path.display().to_string(),
        ];
        archive_spec.workdir = Some(repo_root.clone());
        archive_spec.env = env.clone();
        archive_spec.mounts = self.full_mounts(&repo_root, &ctx.workspace);

        let archive_outcome = self.sandbox.run(&archive_spec)?;
        if !archive_outcome.succeeded() {
            return Ok(terminal_result(
                ctx,
                Stage::Build,
                build_stage_status(&archive_outcome),
                Diagnostics {
                    compiler_errors: Some(capped_utf8(&archive_outcome.stderr)),
                    stderr_excerpt: None,
                },
            ));
        }

        // Stage 3: run the archived judge. harness/tests hidden again --
        // this is where the judge spawns the compiled student binary as a
        // child, but the archive contains only compiled binaries, so
        // there's no hidden test source anywhere in this container to
        // read regardless.
        let mut run_spec = SandboxSpec::new("cargo", self.run_limits.clone());
        run_spec.args = vec![
            "nextest".into(),
            "run".into(),
            "--archive-file".into(),
            archive_path.display().to_string(),
        ];
        run_spec.workdir = Some(repo_root.clone());
        run_spec.env = env;
        run_spec.mounts = self.hidden_tests_mounts(&repo_root, &ctx.workspace)?;

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
            submission_date: None,
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
        submission_date: None,
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
harness = "harness"
cargo-lock-sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[sandbox]
image = "autograder-base:1.86.0"


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

    fn write_harness_manifest(package_dir: &Path) {
        std::fs::create_dir_all(package_dir.join("harness")).unwrap();
        std::fs::write(
            package_dir.join("harness/Cargo.toml"),
            "[package]\nname = \"harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
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
    fn a_custom_assignment_harness_name_drives_both_the_directory_and_the_p_arg() {
        let package_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(package_dir.path().join("driver")).unwrap();
        std::fs::write(
            package_dir.path().join("driver/Cargo.toml"),
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let toml = r#"
[assignment]
id = "wc"
name = "Word count"
kind = "binary"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "driver"
cargo-lock-sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[sandbox]
image = "autograder-base:1.86.0"


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
        let spec: Spec = toml::from_str(toml).unwrap();
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());

        // Stage 1 (student build) succeeds; stage 2 (archive, scoped to
        // the custom harness package name) fails -- that's the one that
        // should reference "driver".
        let (sandbox, specs) = ScriptedSandbox::spy(vec![ok_outcome(), failed_outcome()]);
        let evaluator = Binary::new(&spec, package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace)).unwrap();

        let specs = specs.lock().unwrap();
        assert!(!specs[0].args.contains(&"driver".to_string()));
        assert!(specs[1].args.contains(&"driver".to_string()));
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

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome(), ok_outcome()]);
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

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome(), ok_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace)).unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::Ok);
        assert_eq!(eval.stages.run.status, StageStatus::Ok);
        assert_eq!(eval.tests.len(), 1);
        assert_eq!(eval.tests[0].status, TestStatus::Pass);
    }

    #[test]
    fn stage_1_builds_only_the_student_crate_with_harness_tests_hidden() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, harness_dir) = job_dirs(repo_root.path());

        let (sandbox, specs) = ScriptedSandbox::spy(vec![failed_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace)).unwrap();

        let specs = specs.lock().unwrap();
        let build_spec = &specs[0];
        assert_eq!(build_spec.workdir.as_deref(), Some(repo_root.path()));
        assert_eq!(build_spec.args, vec!["build", "--offline", "-p", "wc"]);

        let shadow = build_spec
            .mounts
            .iter()
            .find(|m| m.container_path == harness_dir.join("tests"))
            .expect("stage 1 shadows harness/tests");
        assert_eq!(shadow.mode, crate::exec::sandbox::MountMode::ReadOnly);
        assert_ne!(shadow.host_path, harness_dir.join("tests"));
    }

    #[test]
    fn stage_2_archives_the_harness_package_with_full_visibility_and_no_id_selector() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, harness_dir) = job_dirs(repo_root.path());

        let (sandbox, specs) = ScriptedSandbox::spy(vec![ok_outcome(), failed_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace)).unwrap();

        let specs = specs.lock().unwrap();
        let archive_spec = &specs[1];
        assert_eq!(archive_spec.workdir.as_deref(), Some(repo_root.path()));
        assert_eq!(
            archive_spec.args[..4],
            ["nextest", "archive", "--offline", "-p"]
        );
        assert!(archive_spec.args.contains(&"harness".to_string()));
        // No dependency edge ties `harness` to `<id>` (see this module's
        // doc comment) -- the archive stage never needs to select it.
        assert!(!archive_spec.args.contains(&"wc".to_string()));
        assert!(
            archive_spec
                .mounts
                .iter()
                .all(|m| m.container_path != harness_dir.join("tests"))
        );
    }

    #[test]
    fn stage_3_runs_from_the_archive_with_harness_tests_hidden_again() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, harness_dir) = job_dirs(repo_root.path());
        let junit_path = repo_root.path().join("target/nextest/default/junit.xml");
        std::fs::create_dir_all(junit_path.parent().unwrap()).unwrap();
        std::fs::write(&junit_path, SAMPLE_JUNIT).unwrap();

        let (sandbox, specs) =
            ScriptedSandbox::spy(vec![ok_outcome(), ok_outcome(), ok_outcome()]);
        let evaluator = Binary::new(&spec(), package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace.clone())).unwrap();

        let specs = specs.lock().unwrap();
        let run_spec = &specs[2];
        assert_eq!(run_spec.workdir.as_deref(), Some(repo_root.path()));
        assert!(run_spec.args.contains(&"--archive-file".to_string()));
        assert!(!run_spec.args.contains(&"-p".to_string()));

        let shadow = run_spec
            .mounts
            .iter()
            .find(|m| m.container_path == harness_dir.join("tests"))
            .expect("stage 3 shadows harness/tests");
        assert_eq!(shadow.mode, crate::exec::sandbox::MountMode::ReadOnly);

        // `<id>`'s own source was never confidential, so it stays visible
        // (unshadowed) even in this stage -- unlike `harness/tests`.
        assert!(
            run_spec
                .mounts
                .iter()
                .any(|m| m.container_path == workspace && m.host_path == workspace)
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
        let mounts = evaluator.full_mounts(repo_root, &workspace);

        let repo_root_mount = mounts
            .iter()
            .find(|m| m.container_path == repo_root)
            .unwrap();
        assert_eq!(
            repo_root_mount.mode,
            crate::exec::sandbox::MountMode::ReadWrite
        );

        let workspace_mount = mounts
            .iter()
            .find(|m| m.container_path == workspace)
            .unwrap();
        assert_eq!(
            workspace_mount.mode,
            crate::exec::sandbox::MountMode::ReadOnly
        );
    }
}
