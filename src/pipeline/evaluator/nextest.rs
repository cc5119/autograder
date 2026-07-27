//! `Nextest`: the one `Evaluator` for both `library` and `binary`
//! assignments, kind-agnostic.
//!
//! The harness is overlaid fresh per job into `repo_root/<harness>`
//! (`repo_root` = `ctx.workspace`'s parent), never built in place -- sharing
//! one harness dir across jobs would race different students' builds
//! against the same `Cargo.lock`/`target/`, since Cargo rewrites the lock
//! whenever a path dependency's contents change.
//!
//! The judge's `#[test]`s spawn student code as a child (via
//! `autograder-test`, which shells out to `isolate`) rather than calling
//! into it in-process, so `evaluate` never needs to know whether that child
//! is the student's own binary or a harness-owned driver: it builds
//! `-p <id>` and `-p <harness_package>` unconditionally and lets Cargo
//! build whatever targets each package declares.
//!
//! `evaluate` runs three sandboxed stages:
//!
//! 1. **build `<id>`** -- `cargo build -p <id>`, `hidden_tests_mounts`
//!    (harness/tests hidden, so a student `build.rs` can't read it).
//! 2. **build `<harness_package>`** -- a *separate* sandbox call from stage
//!    1, never combined into one `cargo build -p a -p b`: that would force
//!    student and harness compilation to share one mount set. `full_mounts`
//!    (real `harness/tests`): `<id>` is already built and cached from stage
//!    1, so nothing student-authored newly executes here, only the
//!    harness's own trusted code -- and hiding `harness/tests` would break
//!    any harness `Cargo.toml` that declares an explicit `[[test]]` target
//!    path, since Cargo validates declared target paths exist while
//!    loading the manifest, even for a plain `cargo build`.
//! 3. **run** -- `cargo nextest run -p <harness_package>`, directly against
//!    real source (no archive), `repo_root_mounts` (harness/tests visible)
//!    and no `SandboxLimits`. Both are safe now because the only
//!    student-authored code executing here is `isolate`-boxed: it can't see
//!    `harness/tests` regardless of this container's mounts (verified in
//!    `spike/isolate-podman/`), and its resource limits come from
//!    `autograder-test` calls in the harness's own test code, not from
//!    `autograder.toml`.
//!
//! `repo_root` isn't a descendant of `workspace`, so Cargo can't discover
//! `workspace`'s offline vendoring config there -- passed as `--config`
//! flags instead of a `.cargo/config.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::deps::vendor;
use crate::error::{Error, Result};
use crate::exec::sandbox::{Mount, Sandbox, SandboxLimits, SandboxOutcome, SandboxSpec};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus, TestResult, TestStatus,
};
use crate::spec::Spec;

use super::{Evaluator, isolate_run_config, sandbox_limits, write_nextest_config};

/// The relative path (from the assignment package dir) `cargo vendor`
/// writes to; mirrors `vendor::prefetch`'s output layout.
const VENDOR_DIR_NAME: &str = "vendor";

pub struct Nextest<S> {
    sandbox: S,
    package_dir: PathBuf,
    limits: SandboxLimits,
    /// `[assignment].harness` -- names both the harness's sibling directory
    /// and its own `[package].name`, which must agree. Cloned once at
    /// construction so `evaluate` can pass `-p <harness_package>`.
    harness_package: String,
}

impl<S: Sandbox> Nextest<S> {
    pub fn new(spec: &Spec, package_dir: impl Into<PathBuf>, sandbox: S) -> Result<Self> {
        let package_dir = package_dir.into();
        let harness_manifest = package_dir
            .join(&spec.assignment.harness)
            .join("Cargo.toml");
        if !harness_manifest.is_file() {
            return Err(Error::InvalidSpec(format!(
                "assignment missing {} -- the harness must be a real, \
                 instructor-authored crate (no default is generated)",
                harness_manifest.display()
            )));
        }
        Ok(Self {
            sandbox,
            package_dir,
            limits: sandbox_limits(&spec.limits),
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

    /// For the run stage (see this module's doc comment).
    fn full_mounts(&self, repo_root: &Path, workspace: &Path) -> Vec<Mount> {
        super::repo_root_mounts(
            repo_root,
            workspace,
            &self.harness_dir(repo_root),
            &self.vendor_dir(),
        )
    }

    /// For the two build stages (see this module's doc comment).
    fn hidden_tests_mounts(&self, repo_root: &Path, workspace: &Path) -> Result<Vec<Mount>> {
        super::hidden_tests_mounts(
            repo_root,
            workspace,
            &self.harness_dir(repo_root),
            &self.vendor_dir(),
        )
    }

    /// The offline vendored-source `--config` override, only when the
    /// package has been prefetched (see this module's doc comment for why
    /// `--config` rather than a `.cargo/config.toml` `Prepare` writes).
    fn config_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let vendor_dir = self.vendor_dir();
        if vendor_dir.is_dir() {
            args.push("--config".to_string());
            args.push("source.crates-io.replace-with=\"vendored-sources\"".to_string());
            args.push("--config".to_string());
            args.push(format!(
                "source.vendored-sources.directory=\"{}\"",
                vendor::absolutize(&vendor_dir).display()
            ));
        }
        args
    }
}

impl<S: Sandbox> Evaluator for Nextest<S> {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
        let repo_root = ctx
            .workspace
            .parent()
            .expect(
                "workspace always has a parent (repo_root); see model.rs's JobContext doc comment",
            )
            .to_path_buf();
        let config_args = self.config_args();
        let env = self.offline_env();

        // Stage 1 (see this module's doc comment).
        let mut build_id_spec = SandboxSpec::new("cargo", Some(self.limits.clone()));
        build_id_spec.args = vec![
            "build".into(),
            "--offline".into(),
            "-p".into(),
            ctx.assignment_id.to_string(),
        ];
        build_id_spec.args.extend(config_args.iter().cloned());
        build_id_spec.workdir = Some(repo_root.clone());
        build_id_spec.env = env.clone();
        build_id_spec.mounts = self.hidden_tests_mounts(&repo_root, &ctx.workspace)?;

        let build_id_outcome = self.sandbox.run(&build_id_spec)?;
        if !build_id_outcome.succeeded() {
            return Ok(terminal_result(
                ctx,
                Stage::Build,
                build_stage_status(&build_id_outcome),
                Diagnostics {
                    compiler_errors: Some(capped_utf8(&build_id_outcome.stderr)),
                    stderr_excerpt: None,
                },
            ));
        }

        // Stage 2 (see this module's doc comment).
        let mut build_harness_spec = SandboxSpec::new("cargo", Some(self.limits.clone()));
        build_harness_spec.args = vec![
            "build".into(),
            "--offline".into(),
            "-p".into(),
            self.harness_package.clone(),
        ];
        build_harness_spec.args.extend(config_args.iter().cloned());
        build_harness_spec.workdir = Some(repo_root.clone());
        build_harness_spec.env = env.clone();
        build_harness_spec.mounts = self.full_mounts(&repo_root, &ctx.workspace);

        let build_harness_outcome = self.sandbox.run(&build_harness_spec)?;
        if !build_harness_outcome.succeeded() {
            return Ok(terminal_result(
                ctx,
                Stage::Build,
                build_stage_status(&build_harness_outcome),
                Diagnostics {
                    compiler_errors: Some(capped_utf8(&build_harness_outcome.stderr)),
                    stderr_excerpt: None,
                },
            ));
        }

        write_nextest_config(&repo_root)?;

        // Stage 3 (see this module's doc comment).
        let mut run_spec = SandboxSpec::new("cargo", None);
        run_spec.args = vec![
            "nextest".into(),
            "run".into(),
            "-p".into(),
            self.harness_package.clone(),
        ];
        run_spec.args.extend(config_args);
        run_spec.workdir = Some(repo_root.clone());
        run_spec.env = env;
        run_spec.mounts = self.full_mounts(&repo_root, &ctx.workspace);
        isolate_run_config(&mut run_spec);

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
            // No report means the judge crashed before any session
            // completed -- never treat that as a pass.
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

/// Parses `cargo nextest`'s JUnit XML report into `TestResult`s -- every
/// genuine `#[test]` fn nextest ran, hand-written or `dir-test`-generated,
/// with no pre-declared name table consulted.
pub fn parse_junit_report(xml: &str) -> Result<Vec<TestResult>> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| Error::Other(format!("failed to parse junit report: {e}")))?;

    let mut results = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("testcase")) {
        let name = node.attribute("name").unwrap_or("").to_string();
        let duration_ms = node
            .attribute("time")
            .and_then(|t| t.parse::<f64>().ok())
            .map(|secs| (secs * 1000.0) as u64);

        let failure = node.children().find(|c| c.has_tag_name("failure"));
        let error = node.children().find(|c| c.has_tag_name("error"));
        let (status, message) = if let Some(node) = error {
            (
                TestStatus::Error,
                node.attribute("message").map(String::from),
            )
        } else if let Some(node) = failure {
            (
                TestStatus::Fail,
                node.attribute("message").map(String::from),
            )
        } else {
            (TestStatus::Pass, None)
        };

        let reported_score = reported_score(node);

        results.push(TestResult {
            name,
            status,
            duration_ms,
            message,
            reported_score,
        });
    }

    Ok(results)
}

/// Sums every `autograder: ... score=<f64>` line found in the testcase's
/// captured stdout (`<system-out>`, only present for a pass when the
/// harness's nextest profile sets `store-success-output = true`) or its
/// failure/error text -- `None` if none were reported, so `crate::pipeline::grade`
/// falls back to the 1.0/0.0 pass/fail default.
fn reported_score(node: roxmltree::Node) -> Option<f64> {
    let mut text = String::new();
    for out in node.children() {
        if !(out.has_tag_name("system-out")
            || out.has_tag_name("failure")
            || out.has_tag_name("error"))
        {
            continue;
        }
        if let Some(t) = out.text() {
            text.push_str(t);
            text.push('\n');
        }
    }
    sum_reported_scores(&text)
}

fn sum_reported_scores(text: &str) -> Option<f64> {
    let mut sum = 0.0;
    let mut found = false;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("autograder:") else {
            continue;
        };
        for token in rest.split_whitespace() {
            if let Some(raw) = token.strip_prefix("score=")
                && let Ok(v) = raw.parse::<f64>()
            {
                sum += v;
                found = true;
            }
        }
    }
    found.then_some(sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::sandbox::SandboxOutcome;
    use std::sync::Mutex;

    const SAMPLE_JUNIT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="driver" tests="3" failures="1" errors="1">
    <testcase name="insert_basic" time="0.012"/>
    <testcase name="balance_adversarial" time="0.031">
      <failure message="assertion failed: height &lt;= 2*log2(n)"/>
    </testcase>
    <testcase name="delete_edge" time="0.005">
      <error message="process exited unexpectedly"/>
    </testcase>
  </testsuite>
</testsuites>
"#;

    #[test]
    fn parses_a_captured_nextest_junit_sample_into_test_results() {
        let results = parse_junit_report(SAMPLE_JUNIT).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].name, "insert_basic");
        assert_eq!(results[0].status, TestStatus::Pass);

        assert_eq!(results[1].status, TestStatus::Fail);
        assert!(results[1].message.as_ref().unwrap().contains("height"));

        assert_eq!(results[2].status, TestStatus::Error);
    }

    #[test]
    fn a_passing_tests_reported_score_is_summed_from_captured_stdout() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<testsuites>
  <testsuite name="driver" tests="1" failures="0" errors="0">
    <testcase name="partial_credit" time="0.004">
      <system-out>autograder: case=a score=0.5
autograder: case=b score=0.25
</system-out>
    </testcase>
  </testsuite>
</testsuites>
"#;
        let results = parse_junit_report(xml).unwrap();
        assert_eq!(results[0].reported_score, Some(0.75));
    }

    #[test]
    fn a_test_with_no_score_line_reports_none() {
        let results = parse_junit_report(SAMPLE_JUNIT).unwrap();
        assert!(results.iter().all(|t| t.reported_score.is_none()));
    }

    /// Returns canned outcomes for each sandboxed call, in order.
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

        /// Like `new`, but also returns a handle to every `SandboxSpec`
        /// this sandbox is run with -- for tests that need to inspect the
        /// argv `Nextest` builds, even though `Nextest::new` consumes the
        /// sandbox by value.
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
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[sandbox]
image = "autograder-base:1.86.0"

[limits]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256
max-output-bytes = "1MiB"

[scoring]
formula = "sum"
base = 0.0
"#;
        toml::from_str(toml).unwrap()
    }

    /// `workspace` and `harness/` as real siblings under one `repo_root`,
    /// matching the invariant `Nextest::evaluate` relies on in production
    /// (see this module's doc comment). Returns `(workspace, harness_dir)`
    /// -- callers that don't need `harness_dir` for their own assertions
    /// can ignore it.
    fn job_dirs(repo_root: &std::path::Path) -> (PathBuf, PathBuf) {
        let workspace = repo_root.join("hw3");
        let harness_dir = repo_root.join("harness");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&harness_dir).unwrap();
        (workspace, harness_dir)
    }

    fn ctx(workspace: PathBuf) -> JobContext {
        JobContext {
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            workspace,
        }
    }

    fn write_harness_manifest(package_dir: &std::path::Path) {
        std::fs::create_dir_all(package_dir.join("harness")).unwrap();
        std::fs::write(
            package_dir.join("harness/Cargo.toml"),
            "[package]\nname = \"harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [[bin]]\nname = \"driver\"\npath = \"src/bin/driver.rs\"\n\n\
             [dependencies]\nhw3 = { path = \"../hw3\" }\n",
        )
        .unwrap();
    }

    #[test]
    fn new_errors_clearly_when_the_harness_is_missing() {
        let package_dir = tempfile::tempdir().unwrap();

        let result = Nextest::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![]));

        assert!(matches!(result, Err(Error::InvalidSpec(_))));
    }

    #[test]
    fn config_args_has_no_vendored_source_override_without_a_prefetched_vendor_dir() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let evaluator =
            Nextest::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![])).unwrap();

        assert!(evaluator.config_args().is_empty());
    }

    #[test]
    fn config_args_points_at_the_vendor_dir_once_prefetched() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        std::fs::create_dir_all(package_dir.path().join("vendor")).unwrap();
        let evaluator =
            Nextest::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![])).unwrap();

        let args = evaluator.config_args();

        assert!(
            args.iter()
                .any(|a| a.starts_with("source.vendored-sources.directory="))
        );
        assert!(!args.iter().any(|a| a.starts_with("patch.crates-io")));
    }

    #[test]
    fn stage_1_build_failure_short_circuits_before_stage_2() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());

        let (sandbox, specs) = ScriptedSandbox::spy(vec![failed_outcome()]);
        let evaluator = Nextest::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace)).unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::BuildFailed);
        assert!(eval.diagnostics.compiler_errors.unwrap().contains("E0433"));
        assert!(eval.tests.is_empty());
        assert_eq!(specs.lock().unwrap().len(), 1);
    }

    #[test]
    fn stage_2_build_failure_short_circuits_before_stage_3() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());

        let (sandbox, specs) = ScriptedSandbox::spy(vec![ok_outcome(), failed_outcome()]);
        let evaluator = Nextest::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace)).unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::BuildFailed);
        assert!(eval.tests.is_empty());
        assert_eq!(specs.lock().unwrap().len(), 2);
    }

    #[test]
    fn missing_junit_report_after_a_successful_run_is_a_harness_error() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());

        // build id, build harness, run -- all three stages succeed, but no
        // junit report is on disk afterwards.
        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome(), ok_outcome()]);
        let evaluator = Nextest::new(&spec(), package_dir.path(), sandbox).unwrap();

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
        let evaluator = Nextest::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace)).unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::Ok);
        assert_eq!(eval.stages.run.status, StageStatus::Ok);
        assert_eq!(eval.tests.len(), 3);
    }

    #[test]
    fn stage_1_builds_only_id_with_harness_tests_hidden_and_build_limits() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, harness_dir) = job_dirs(repo_root.path());

        let (sandbox, specs) = ScriptedSandbox::spy(vec![failed_outcome()]);
        let evaluator = Nextest::new(&spec(), package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace)).unwrap();

        let specs = specs.lock().unwrap();
        let build_spec = &specs[0];
        assert_eq!(build_spec.workdir.as_deref(), Some(repo_root.path()));
        assert_eq!(build_spec.args[..4], ["build", "--offline", "-p", "hw3"]);
        assert!(!build_spec.args.contains(&"harness".to_string()));
        assert!(!build_spec.args.contains(&"--test".to_string()));
        assert!(build_spec.limits.is_some());

        let shadow = build_spec
            .mounts
            .iter()
            .find(|m| m.container_path == harness_dir.join("tests"))
            .expect("stage 1 shadows harness/tests");
        assert_eq!(shadow.mode, crate::exec::sandbox::MountMode::ReadOnly);
        assert_ne!(shadow.host_path, harness_dir.join("tests"));
    }

    #[test]
    fn stage_2_builds_only_harness_in_its_own_sandbox_call_with_harness_tests_visible() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, harness_dir) = job_dirs(repo_root.path());

        let (sandbox, specs) = ScriptedSandbox::spy(vec![ok_outcome(), failed_outcome()]);
        let evaluator = Nextest::new(&spec(), package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace)).unwrap();

        let specs = specs.lock().unwrap();
        let build_spec = &specs[1];
        assert_eq!(
            build_spec.args[..4],
            ["build", "--offline", "-p", "harness"]
        );
        // Never combined with stage 1's `-p hw3` into one invocation.
        assert!(!build_spec.args.contains(&"hw3".to_string()));
        assert!(build_spec.limits.is_some());

        // Real harness/tests, not shadowed -- `<id>` is already built by
        // stage 1, so nothing student-authored newly executes here (see
        // this module's doc comment).
        assert!(
            build_spec
                .mounts
                .iter()
                .all(|m| m.container_path != harness_dir.join("tests"))
        );
    }

    #[test]
    fn stage_3_runs_direct_from_source_with_harness_tests_visible_and_no_limits() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, harness_dir) = job_dirs(repo_root.path());
        let junit_path = repo_root.path().join("target/nextest/default/junit.xml");
        std::fs::create_dir_all(junit_path.parent().unwrap()).unwrap();
        std::fs::write(&junit_path, SAMPLE_JUNIT).unwrap();

        let (sandbox, specs) =
            ScriptedSandbox::spy(vec![ok_outcome(), ok_outcome(), ok_outcome()]);
        let evaluator = Nextest::new(&spec(), package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace)).unwrap();

        let specs = specs.lock().unwrap();
        let run_spec = &specs[2];
        assert_eq!(run_spec.program, "cargo");
        assert!(!run_spec.args.contains(&"--archive-file".to_string()));
        assert_eq!(run_spec.args[..4], ["nextest", "run", "-p", "harness"]);
        assert!(run_spec.limits.is_none());

        // harness/tests is real here, not shadowed -- safe per the isolate
        // filesystem-confinement spike (see this module's doc comment).
        assert!(
            run_spec
                .mounts
                .iter()
                .all(|m| m.container_path != harness_dir.join("tests"))
        );

        assert!(run_spec.cgroupns_private);
        assert!(run_spec.cap_add.contains(&"SYS_ADMIN".to_string()));
    }

    #[test]
    fn a_custom_assignment_harness_name_drives_both_the_directory_and_the_p_args() {
        let package_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(package_dir.path().join("judge")).unwrap();
        std::fs::write(
            package_dir.path().join("judge/Cargo.toml"),
            "[package]\nname = \"judge\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [[bin]]\nname = \"driver\"\npath = \"src/bin/driver.rs\"\n\n\
             [dependencies]\nhw3 = { path = \"../hw3\" }\n",
        )
        .unwrap();
        let toml = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "judge"
cargo-lock-sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[sandbox]
image = "autograder-base:1.86.0"

[limits]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256
max-output-bytes = "1MiB"

[scoring]
formula = "sum"
base = 0.0
"#;
        let spec: Spec = toml::from_str(toml).unwrap();
        let repo_root = tempfile::tempdir().unwrap();
        let (workspace, _harness_dir) = job_dirs(repo_root.path());

        // Stage 1 (build id) succeeds; stage 2 (build harness, scoped to
        // the custom harness package name) fails -- that's the one that
        // should reference "judge".
        let (sandbox, specs) = ScriptedSandbox::spy(vec![ok_outcome(), failed_outcome()]);
        let evaluator = Nextest::new(&spec, package_dir.path(), sandbox).unwrap();
        evaluator.evaluate(&ctx(workspace)).unwrap();

        let specs = specs.lock().unwrap();
        assert!(!specs[0].args.contains(&"judge".to_string()));
        assert!(specs[1].args.contains(&"judge".to_string()));
    }
}
