//! The `linked-library` `Evaluator` (design §9.1): builds the trusted
//! **driver** crate (path-depends on the student library) and runs the
//! instructor-authored **judge** — process-per-session under `cargo
//! nextest` — under a `Sandbox`. Both build and run stages happen inside
//! the sandbox with `[limits.build]` / `[limits.run]` respectively.
//!
//! This module is deliberately agnostic to the op-sequence protocol a given
//! assignment's judge speaks to its driver: that's instructor-authored test
//! code overlaid from `harness/driver` and `harness/tests` (or wherever the
//! instructor puts their `#[test]` functions) during Prepare. This module's
//! job is mechanical: (1) make sure the driver crate has a working path
//! dependency on the student package, (2) drive `cargo build` then `cargo
//! nextest run` through the sandbox with the right limits/offline env, (3)
//! turn nextest's JUnit report into `TestResult`s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{
    Diagnostics, EvaluationResult, JobContext, ResourceUsage, StageReport, StageReports,
    StageStatus, TestResult, TestStatus, TestVisibility,
};
use crate::sandbox::{Mount, MountMode, Sandbox, SandboxLimits, SandboxOutcome, SandboxSpec};
use crate::spec::{BuildLimits, RunLimits, ScoredTest, Spec};

use super::Evaluator;

/// The relative path (from the assignment package dir) `cargo vendor`
/// writes to; mirrors `vendor::prefetch`'s output layout.
const VENDOR_DIR_NAME: &str = "vendor";

pub struct LinkedLibrary<S> {
    sandbox: S,
    package_dir: PathBuf,
    student_package_name: String,
    build_limits: SandboxLimits,
    run_limits: SandboxLimits,
    tests: Vec<ScoredTest>,
}

impl<S: Sandbox> LinkedLibrary<S> {
    pub fn new(spec: &Spec, package_dir: impl Into<PathBuf>, sandbox: S) -> Result<Self> {
        let student_package_name = spec.student.package_name.clone().ok_or_else(|| {
            Error::InvalidSpec("linked-library assignment missing [student].package-name".into())
        })?;
        Ok(Self {
            sandbox,
            package_dir: package_dir.into(),
            student_package_name,
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

    fn mounts(&self, workspace: &Path) -> Vec<Mount> {
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

impl<S: Sandbox> Evaluator for LinkedLibrary<S> {
    fn evaluate(&self, ctx: &JobContext) -> Result<EvaluationResult> {
        let driver_dir = ctx.workspace.join("driver");
        assemble_driver(&driver_dir, &self.student_package_name)?;

        let env = self.offline_env();
        let mounts = self.mounts(&ctx.workspace);

        let mut build_spec = SandboxSpec::new("cargo", self.build_limits.clone());
        build_spec.args = vec!["build".into(), "--offline".into()];
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

        let mut run_spec = SandboxSpec::new("cargo", self.run_limits.clone());
        run_spec.args = vec!["nextest".into(), "run".into(), "--offline".into()];
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
            // The judge never wrote a report: a crash before any session
            // completed, or a nextest/config mismatch. Never treat missing
            // results as a pass (design §9: every test defaults to fail).
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
        tier: ctx.tier,
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

fn build_sandbox_limits(build: &BuildLimits, run: &RunLimits) -> SandboxLimits {
    SandboxLimits {
        wall_clock: build.wall_clock.0,
        cpus: build.cpus,
        memory_bytes: build.memory.0,
        pids: build.pids,
        // `[limits.build]` carries no `max-output-bytes` of its own (design
        // §5.3); compiler output still needs a cap, so reuse the run
        // stage's.
        max_output_bytes: run.max_output_bytes.0,
    }
}

fn run_sandbox_limits(run: &RunLimits) -> SandboxLimits {
    SandboxLimits {
        wall_clock: run.wall_clock.0,
        cpus: run.cpus,
        memory_bytes: run.memory.0,
        pids: run.pids,
        max_output_bytes: run.max_output_bytes.0,
    }
}

/// Ensures `driver_dir` is a buildable crate that path-depends on the
/// student package. A harness overlay may already have written
/// `driver/Cargo.toml` (and driver/judge source) during Prepare — that
/// instructor-authored content always wins; this only fills in what's
/// missing so authoring a harness never requires hand-computing relative
/// paths.
pub fn assemble_driver(driver_dir: &Path, student_package_name: &str) -> Result<()> {
    std::fs::create_dir_all(driver_dir).map_err(|source| Error::Io {
        path: driver_dir.to_path_buf(),
        source,
    })?;

    let manifest_path = driver_dir.join("Cargo.toml");
    if !manifest_path.exists() {
        let manifest = format!(
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\n{student_package_name} = {{ path = \"..\" }}\n"
        );
        std::fs::write(&manifest_path, manifest).map_err(|source| Error::Io {
            path: manifest_path.clone(),
            source,
        })?;
    }

    let main_path = driver_dir.join("src/main.rs");
    if !main_path.exists() {
        std::fs::create_dir_all(driver_dir.join("src")).map_err(|source| Error::Io {
            path: driver_dir.join("src"),
            source,
        })?;
        std::fs::write(&main_path, "fn main() {}\n").map_err(|source| Error::Io {
            path: main_path.clone(),
            source,
        })?;
    }

    Ok(())
}

/// Parses `cargo nextest`'s JUnit XML report into `TestResult`s. `tests` is
/// the spec's scored-test list, consulted only to recover each test's
/// `visibility` (JUnit doesn't carry it) — matched by exact name first,
/// falling back to the name's last `::`-separated segment (nextest reports
/// `<binary>::<test_fn>`-style names).
pub fn parse_junit_report(xml: &str, tests: &[ScoredTest]) -> Result<Vec<TestResult>> {
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
            (TestStatus::Error, node.attribute("message").map(String::from))
        } else if let Some(node) = failure {
            (TestStatus::Fail, node.attribute("message").map(String::from))
        } else {
            (TestStatus::Pass, None)
        };

        let visibility = visibility_for(&name, tests);

        results.push(TestResult {
            name,
            visibility,
            status,
            duration_ms,
            message,
        });
    }

    Ok(results)
}

fn visibility_for(name: &str, tests: &[ScoredTest]) -> TestVisibility {
    let leaf = name.rsplit("::").next().unwrap_or(name);
    tests
        .iter()
        .find(|t| t.name == name || t.name == leaf)
        .map(|t| t.visibility)
        .unwrap_or(TestVisibility::Public)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tier;
    use crate::sandbox::SandboxOutcome;
    use std::sync::Mutex;

    fn scored(name: &str, visibility: TestVisibility) -> ScoredTest {
        ScoredTest {
            name: name.into(),
            visibility,
            points: Some(10.0),
        }
    }

    #[test]
    fn assemble_driver_writes_a_manifest_with_the_student_path_dependency() {
        let workspace = tempfile::tempdir().unwrap();
        let driver_dir = workspace.path().join("driver");

        assemble_driver(&driver_dir, "bst").unwrap();

        let manifest = std::fs::read_to_string(driver_dir.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("bst = { path = \"..\" }"));
        assert!(driver_dir.join("src/main.rs").exists());
    }

    #[test]
    fn assemble_driver_never_overwrites_an_instructor_supplied_manifest() {
        let workspace = tempfile::tempdir().unwrap();
        let driver_dir = workspace.path().join("driver");
        std::fs::create_dir_all(&driver_dir).unwrap();
        std::fs::write(driver_dir.join("Cargo.toml"), "# custom").unwrap();

        assemble_driver(&driver_dir, "bst").unwrap();

        let manifest = std::fs::read_to_string(driver_dir.join("Cargo.toml")).unwrap();
        assert_eq!(manifest, "# custom");
    }

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
        let tests = vec![
            scored("insert_basic", TestVisibility::Public),
            scored("balance_adversarial", TestVisibility::Private),
            scored("delete_edge", TestVisibility::Public),
        ];

        let results = parse_junit_report(SAMPLE_JUNIT, &tests).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].name, "insert_basic");
        assert_eq!(results[0].status, TestStatus::Pass);
        assert_eq!(results[0].visibility, TestVisibility::Public);

        assert_eq!(results[1].status, TestStatus::Fail);
        assert_eq!(results[1].visibility, TestVisibility::Private);
        assert!(results[1].message.as_ref().unwrap().contains("height"));

        assert_eq!(results[2].status, TestStatus::Error);
    }

    #[test]
    fn unknown_test_name_defaults_to_public_visibility() {
        let results = parse_junit_report(SAMPLE_JUNIT, &[]).unwrap();
        assert!(results.iter().all(|t| t.visibility == TestVisibility::Public));
    }

    /// A `Sandbox` double that returns canned outcomes for the build then
    /// run invocation, in order — lets orchestration (build-then-run,
    /// short-circuit on build failure, junit parsing) be tested without a
    /// live sandbox/nextest.
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
kind = "linked-library"
deadline = "2026-02-14T23:59:59-08:00"

[student]
package-name = "bst"

[toolchain]
channel = "1.86.0"

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
name = "insert_basic"
points = 10
visibility = "public"
"#;
        toml::from_str(toml).unwrap()
    }

    fn ctx(workspace: PathBuf) -> JobContext {
        JobContext {
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            tier: Tier::Authoritative,
            workspace,
        }
    }

    #[test]
    fn build_failure_short_circuits_before_running_nextest() {
        let package_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let sandbox = ScriptedSandbox::new(vec![failed_outcome()]);
        let evaluator = LinkedLibrary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace.path().to_path_buf())).unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::BuildFailed);
        assert!(eval.diagnostics.compiler_errors.unwrap().contains("E0433"));
        assert!(eval.tests.is_empty());
    }

    #[test]
    fn missing_junit_report_after_a_successful_run_is_a_harness_error() {
        let package_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome()]);
        let evaluator = LinkedLibrary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace.path().to_path_buf())).unwrap();

        assert_eq!(eval.stages.run.status, StageStatus::HarnessError);
    }

    #[test]
    fn a_junit_report_on_disk_is_parsed_into_the_eval_result() {
        let package_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let junit_path = workspace
            .path()
            .join("driver/target/nextest/default/junit.xml");
        std::fs::create_dir_all(junit_path.parent().unwrap()).unwrap();
        std::fs::write(&junit_path, SAMPLE_JUNIT).unwrap();

        let sandbox = ScriptedSandbox::new(vec![ok_outcome(), ok_outcome()]);
        let evaluator = LinkedLibrary::new(&spec(), package_dir.path(), sandbox).unwrap();

        let eval = evaluator.evaluate(&ctx(workspace.path().to_path_buf())).unwrap();

        assert_eq!(eval.stages.build.status, StageStatus::Ok);
        assert_eq!(eval.stages.run.status, StageStatus::Ok);
        assert_eq!(eval.tests.len(), 3);
    }
}
