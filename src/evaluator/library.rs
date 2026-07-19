//! The `library` `Evaluator`: builds the trusted **driver** crate (depends
//! on the student library via a plain path dependency) and runs the
//! instructor-authored **judge** under `cargo nextest`, both under a
//! `Sandbox`. The judge lives permanently at `harness/`, never generated or
//! rewritten by this tool; this module just drives `cargo build` then
//! `cargo nextest run` and turns the JUnit report into `TestResult`s.
//!
//! `harness/Cargo.toml` declares a plain `<id> = { path = "../<id>" }`
//! dependency, no `[patch.crates-io]`, because whoever assembles the job
//! always positions the code under test at that exact sibling location.
//! `harness/` is overlaid into `ctx.driver_dir`, a *fresh, per-job* sibling
//! of `ctx.workspace` (never built in place): freshness matters because
//! Cargo rewrites `Cargo.lock` whenever the path-dependency's contents
//! change, so sharing one `harness/` across jobs would race different
//! students' builds against the same lockfile/target dir. Since
//! `driver_dir` and `workspace` don't share an ancestor, Cargo's
//! directory-based config discovery can't find `workspace`'s offline
//! vendoring config from `driver_dir` -- this evaluator passes the
//! equivalent `[source]` override as `--config` flags directly instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus, TestResult, TestStatus,
};
use crate::sandbox::{Mount, MountMode, Sandbox, SandboxLimits, SandboxOutcome, SandboxSpec};
use crate::spec::Spec;
use crate::vendor;

use super::{Evaluator, build_sandbox_limits, run_sandbox_limits, write_nextest_config};

/// The relative path (from the assignment package dir) `cargo vendor`
/// writes to; mirrors `vendor::prefetch`'s output layout.
const VENDOR_DIR_NAME: &str = "vendor";

pub struct Library<S> {
    sandbox: S,
    package_dir: PathBuf,
    build_limits: SandboxLimits,
    run_limits: SandboxLimits,
}

impl<S: Sandbox> Library<S> {
    pub fn new(spec: &Spec, package_dir: impl Into<PathBuf>, sandbox: S) -> Result<Self> {
        let package_dir = package_dir.into();
        let harness_manifest = package_dir.join("harness/Cargo.toml");
        if !harness_manifest.is_file() {
            return Err(Error::InvalidSpec(format!(
                "library assignment missing {} -- the harness must be a real, \
                 instructor-authored crate (no default is generated)",
                harness_manifest.display()
            )));
        }
        Ok(Self {
            sandbox,
            package_dir,
            build_limits: build_sandbox_limits(&spec.limits.build, &spec.limits.run),
            run_limits: run_sandbox_limits(&spec.limits.run),
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

    fn mounts(&self, workspace: &Path, driver_dir: &Path) -> Vec<Mount> {
        let mut mounts = vec![
            Mount {
                host_path: workspace.to_path_buf(),
                container_path: workspace.to_path_buf(),
                mode: MountMode::ReadOnly,
            },
            Mount {
                host_path: driver_dir.to_path_buf(),
                container_path: driver_dir.to_path_buf(),
                mode: MountMode::ReadWrite,
            },
        ];
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

impl<S: Sandbox> Evaluator for Library<S> {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
        let driver_dir = &ctx.driver_dir;
        let config_args = self.config_args();

        let env = self.offline_env();
        let mounts = self.mounts(&ctx.workspace, driver_dir);

        let mut build_spec = SandboxSpec::new("cargo", self.build_limits.clone());
        build_spec.args = vec!["build".into(), "--offline".into()];
        build_spec.args.extend(config_args.iter().cloned());
        build_spec.workdir = Some(driver_dir.clone());
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

        write_nextest_config(driver_dir)?;

        let mut run_spec = SandboxSpec::new("cargo", self.run_limits.clone());
        run_spec.args = vec!["nextest".into(), "run".into(), "--offline".into()];
        run_spec.args.extend(config_args);
        run_spec.workdir = Some(driver_dir.clone());
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

        let junit_path = driver_dir.join("target/nextest/default/junit.xml");
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
/// failure/error text -- `None` if none were reported, so `crate::grade`
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
    use crate::sandbox::SandboxOutcome;
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

    /// Returns canned outcomes for the build then run invocation, in order.
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
id = "hw3"
name = "Binary search tree"
kind = "library"
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

    fn ctx(workspace: PathBuf, driver_dir: PathBuf) -> JobContext {
        JobContext {
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            workspace,
            driver_dir,
        }
    }

    /// `Library::new` requires a real `harness/Cargo.toml` to exist.
    fn write_harness_manifest(package_dir: &std::path::Path) {
        std::fs::create_dir_all(package_dir.join("harness")).unwrap();
        std::fs::write(
            package_dir.join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nhw3 = { path = \"../hw3\" }\n",
        )
        .unwrap();
    }

    #[test]
    fn new_errors_clearly_when_the_harness_is_missing() {
        let package_dir = tempfile::tempdir().unwrap();

        let result = Library::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![]));

        assert!(matches!(result, Err(Error::InvalidSpec(_))));
    }

    #[test]
    fn config_args_has_no_vendored_source_override_without_a_prefetched_vendor_dir() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let evaluator =
            Library::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![])).unwrap();

        assert!(evaluator.config_args().is_empty());
    }

    #[test]
    fn config_args_points_at_the_vendor_dir_once_prefetched() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        std::fs::create_dir_all(package_dir.path().join("vendor")).unwrap();
        let evaluator =
            Library::new(&spec(), package_dir.path(), ScriptedSandbox::new(vec![])).unwrap();

        let args = evaluator.config_args();

        assert!(
            args.iter()
                .any(|a| a.starts_with("source.vendored-sources.directory="))
        );
        assert!(!args.iter().any(|a| a.starts_with("patch.crates-io")));
    }

    #[test]
    fn build_failure_short_circuits_before_running_nextest() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let workspace = tempfile::tempdir().unwrap();
        let driver_dir = tempfile::tempdir().unwrap();

        let sandbox = ScriptedSandbox::new(vec![failed_outcome()]);
        let evaluator = Library::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator
            .evaluate(&ctx(
                workspace.path().to_path_buf(),
                driver_dir.path().to_path_buf(),
            ))
            .unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::BuildFailed);
        assert!(eval.diagnostics.compiler_errors.unwrap().contains("E0433"));
        assert!(eval.tests.is_empty());
    }

    #[test]
    fn missing_junit_report_after_a_successful_run_is_a_harness_error() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let workspace = tempfile::tempdir().unwrap();
        let driver_dir = tempfile::tempdir().unwrap();

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome()]);
        let evaluator = Library::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator
            .evaluate(&ctx(
                workspace.path().to_path_buf(),
                driver_dir.path().to_path_buf(),
            ))
            .unwrap();

        assert_eq!(eval.stages.run.status, StageStatus::HarnessError);
    }

    #[test]
    fn a_junit_report_on_disk_is_parsed_into_the_eval_result() {
        let package_dir = tempfile::tempdir().unwrap();
        write_harness_manifest(package_dir.path());
        let workspace = tempfile::tempdir().unwrap();
        let driver_dir = tempfile::tempdir().unwrap();
        let junit_path = driver_dir.path().join("target/nextest/default/junit.xml");
        std::fs::create_dir_all(junit_path.parent().unwrap()).unwrap();
        std::fs::write(&junit_path, SAMPLE_JUNIT).unwrap();

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome()]);
        let evaluator = Library::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator
            .evaluate(&ctx(
                workspace.path().to_path_buf(),
                driver_dir.path().to_path_buf(),
            ))
            .unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::Ok);
        assert_eq!(eval.stages.run.status, StageStatus::Ok);
        assert_eq!(eval.tests.len(), 3);
    }
}
