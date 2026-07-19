pub mod cli;
pub mod config;
pub mod error;
pub mod evaluator;
pub mod fetch;
pub mod fs;
pub mod grade;
pub mod init;
pub mod manifest_check;
pub mod model;
pub mod overlay;
pub mod overrides;
pub mod pipeline;
pub mod prepare;
pub mod publish;
pub mod report;
pub mod sandbox;
pub mod source;
pub mod spec;
pub mod store;
pub mod stub;
pub mod template;
pub mod vendor;
pub mod volume;

use cli::{Command, ReportFormat};
pub use config::Config;
pub use error::{Error, Result};

use evaluator::Evaluator;
use evaluator::binary::Binary;
use evaluator::library::Library;
use model::JobContext;
use report::{Reporter, ci::CiReport, csv::CsvReporter, json::JsonReporter};
use sandbox::{ContainerSandbox, LocalSandbox, Sandbox};
use source::Submissions;
use spec::{AssignmentKind, Spec};
use store::Store;

pub fn dispatch(command: Command, config: &Config) -> Result<()> {
    match command {
        Command::Init { dir, kind, id } => run_init(&dir, &id, kind.into()),
        Command::Prefetch { assignment } => run_prefetch(&assignment),
        Command::Fetch {
            assignment,
            submissions,
            as_of,
        } => run_fetch(&assignment, &submissions, as_of.as_deref(), config),
        Command::Grade {
            assignment,
            submissions,
            fetch,
            as_of,
            local_sandbox,
        } => run_grade(
            &assignment,
            &submissions,
            fetch,
            as_of.as_deref(),
            local_sandbox,
            config,
        ),
        Command::Ci { local_sandbox } => run_ci(local_sandbox),
        Command::Regrade {
            assignment_id,
            assignment,
        } => run_regrade(&assignment_id, &assignment, config),
        Command::Report {
            assignment_id,
            format,
            out,
        } => run_report(&assignment_id, format, out, config),
        Command::Publish { assignment, out } => run_publish(&assignment, &out),
    }
}

fn run_init(dir: &std::path::Path, id: &str, kind: AssignmentKind) -> Result<()> {
    let outcome = init::init(dir, id, kind)?;
    tracing::info!(dir = %outcome.dir.display(), "init complete");
    Ok(())
}

fn run_prefetch(assignment: &std::path::Path) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let outcome = vendor::prefetch(assignment, &spec)?;
    tracing::info!(
        vendor_dir = %outcome.vendor_dir.display(),
        cargo_config = %outcome.cargo_config_path.display(),
        "prefetch complete"
    );
    Ok(())
}

/// The deadline used for push-time commit selection: `--as-of` if given
/// (`"<datetime>[<IANA zone>]"`, same format as `[assignment].deadline`),
/// otherwise `[assignment].deadline` itself. A parse failure is an
/// `InvalidSpec`, not a panic or a silent fallback to the spec's deadline
/// -- a typo'd `--as-of` should fail loudly, not quietly grade against the
/// wrong cutoff.
fn resolve_deadline(spec: &Spec, as_of: Option<&str>) -> Result<jiff::Zoned> {
    match as_of {
        Some(raw) => raw.parse::<jiff::Zoned>().map_err(|source| {
            Error::InvalidSpec(format!(
                "--as-of {raw:?} is not a valid zoned timestamp (expected \
                 \"<datetime>[<IANA zone>]\"): {source}"
            ))
        }),
        None => Ok(spec.assignment.deadline.clone()),
    }
}

fn run_fetch(
    assignment: &std::path::Path,
    submissions: &std::path::Path,
    as_of: Option<&str>,
    config: &Config,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let deadline = resolve_deadline(&spec, as_of)?;
    let work_dir = config.storage_dir.join(".work");

    let records = match Submissions::open(submissions)? {
        Submissions::Directory(source) => fetch::fetch_batch(&source, &work_dir, &deadline)?,
        Submissions::Csv(source) => fetch::fetch_batch(&source, &work_dir, &deadline)?,
    };

    for (student_id, record) in &records {
        if record.status == model::StageStatus::Ok {
            tracing::info!(
                student_id,
                graded_commit = record.graded_commit.as_deref().unwrap_or(""),
                "fetched"
            );
        } else {
            tracing::warn!(
                student_id,
                message = record.message.as_deref().unwrap_or(""),
                "fetch failed"
            );
        }
    }
    Ok(())
}

fn run_grade(
    assignment: &std::path::Path,
    submissions: &std::path::Path,
    do_fetch: bool,
    as_of: Option<&str>,
    local_sandbox: bool,
    config: &Config,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let evaluator = build_evaluator(&spec, assignment, local_sandbox)?;
    let store = Store::new(&config.storage_dir);
    let work_dir = config.storage_dir.join(".work");
    let overrides = overrides::Overrides::load_from_package(assignment)?;

    if do_fetch {
        run_fetch(assignment, submissions, as_of, config)?;
    }

    let grades = match Submissions::open(submissions)? {
        Submissions::Directory(source) => pipeline::grade_batch(
            &source,
            evaluator.as_ref(),
            assignment,
            &spec,
            &work_dir,
            &store,
            &overrides,
        )?,
        Submissions::Csv(source) => pipeline::grade_batch(
            &source,
            evaluator.as_ref(),
            assignment,
            &spec,
            &work_dir,
            &store,
            &overrides,
        )?,
    };

    write_reports(&spec.assignment.id, &grades, config)
}

fn build_evaluator(
    spec: &Spec,
    package_dir: &std::path::Path,
    local_sandbox: bool,
) -> Result<Box<dyn Evaluator>> {
    if local_sandbox {
        tracing::warn!(
            "grading with --local-sandbox: skipping Podman entirely, running student code as a \
             host process with no container isolation -- for local development/ \
             testing only, never for grading real submissions"
        );
        build_evaluator_for(spec, package_dir, LocalSandbox)
    } else {
        let sandbox = ContainerSandbox::new(spec.sandbox.image.clone());
        sandbox.preflight()?;
        build_evaluator_for(spec, package_dir, sandbox)
    }
}

fn build_evaluator_for(
    spec: &Spec,
    package_dir: &std::path::Path,
    sandbox: impl Sandbox + 'static,
) -> Result<Box<dyn Evaluator>> {
    match spec.assignment.kind {
        AssignmentKind::Library => Ok(Box::new(Library::new(spec, package_dir, sandbox)?)),
        AssignmentKind::Binary => Ok(Box::new(Binary::new(spec, package_dir, sandbox)?)),
    }
}

/// Student-facing `ci` entrypoint: Prepare + Build + Evaluate against the
/// public harness, always run from the repo root `publish` produces
/// (current directory). Prints per-test feedback and exits non-zero when
/// any public test fails.
fn run_ci(local_sandbox: bool) -> Result<()> {
    let harness_dir = fs::current_dir()?;
    let spec = Spec::load(&harness_dir)?;
    let workspace = harness_dir.join(&spec.assignment.id);
    let run_id = pipeline::generate_run_id();
    // `library`'s harness is already positioned correctly by `publish` as
    // a sibling of `workspace`. `binary` has no separate driver crate, so
    // `driver_dir` is never read for it; any placeholder satisfies `JobContext`.
    let driver_dir = match spec.assignment.kind {
        AssignmentKind::Library => harness_dir.join("harness"),
        AssignmentKind::Binary => std::env::temp_dir().join(format!("autograder-ci-{run_id}")),
    };
    let ctx = JobContext {
        assignment_id: spec.assignment.id.clone(),
        student_id: "local".into(),
        run_id,
        workspace: workspace.clone(),
        driver_dir,
    };

    let prepared = prepare::prepare(&workspace, &harness_dir, &spec)?;

    let eval = if prepared.manifest_diagnostics.is_empty() {
        let evaluator = build_evaluator(&spec, &harness_dir, local_sandbox)?;
        Some(evaluator.evaluate(&ctx)?)
    } else {
        None
    };

    let report = CiReport {
        eval: eval.as_ref(),
        manifest_diagnostics: &prepared.manifest_diagnostics,
    };
    print!("{}", report.render());

    if !report.passed() {
        std::process::exit(1);
    }
    Ok(())
}

fn run_publish(assignment: &std::path::Path, out: &std::path::Path) -> Result<()> {
    let outcome = publish::publish(assignment, out)?;
    tracing::info!(out_dir = %outcome.out_dir.display(), "publish complete");
    Ok(())
}

/// Re-runs **only** the Grade stage from persisted `EvaluationResult`s (no
/// student code, no evaluator) -- applies `spec.scoring`/`overrides.toml`
/// fresh from disk every time, so editing either always reflects the
/// current policy, never one baked in at `grade` time.
fn run_regrade(assignment_id: &str, assignment: &std::path::Path, config: &Config) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let store = Store::new(&config.storage_dir);
    let evals = store.latest_evals(assignment_id)?;
    let overrides = overrides::Overrides::load_from_package(assignment)?;

    let mut grades = Vec::new();
    for eval in &evals {
        let grade = grade::grade(eval, &spec.scoring);
        let grade = overrides::apply(
            grade,
            &overrides,
            &spec.assignment.deadline,
            spec.scoring.late_penalty.as_ref(),
        );
        store.save_grade(&eval.assignment_id, &eval.run_id, &grade)?;
        grades.push(grade);
    }

    write_reports(assignment_id, &grades, config)
}

fn run_report(
    assignment_id: &str,
    format: ReportFormat,
    out: Option<std::path::PathBuf>,
    config: &Config,
) -> Result<()> {
    let store = Store::new(&config.storage_dir);
    let grades = store.latest_grades(assignment_id)?;
    match format {
        ReportFormat::Json => JsonReporter { out }.report(&grades),
        ReportFormat::Csv => CsvReporter { out }.report(&grades),
    }
}

fn write_reports(assignment_id: &str, grades: &[model::Grade], config: &Config) -> Result<()> {
    let reports_dir = config.storage_dir.join("reports");
    fs::create_dir_all(&reports_dir)?;

    JsonReporter {
        out: Some(reports_dir.join(format!("{assignment_id}.json"))),
    }
    .report(grades)?;
    CsvReporter {
        out: Some(reports_dir.join(format!("{assignment_id}.csv"))),
    }
    .report(grades)
}

#[cfg(test)]
mod ci_tests {
    use super::*;

    fn write(path: &std::path::Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const PUBLIC_SPEC: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"

[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]

[limits.build]
wall-clock = "30s"
cpus = 1
memory = "512MiB"
pids = 64

[limits.run]
cpu-time = "5s"
wall-clock = "10s"
cpus = 1
memory = "256MiB"
pids = 64
max-output-bytes = "64KiB"

[scoring]
formula = "sum"
base = 0.0
"#;

    /// This host has no `cargo-nextest` installed, so the run stage never
    /// produces a junit report -- the evaluator should report
    /// `HarnessError`, not crash or silently report a pass.
    #[test]
    fn ci_pipeline_runs_prepare_and_evaluate_end_to_end_short_of_nextest() {
        let harness_dir = tempfile::tempdir().unwrap();
        write(
            &harness_dir.path().join(spec::PUBLIC_SPEC_FILE),
            PUBLIC_SPEC,
        );
        write(
            &harness_dir.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nhw3 = { path = \"../hw3\" }\n",
        );
        write(
            &harness_dir.path().join("harness/src/main.rs"),
            "fn main() {}\n",
        );

        let workspace = harness_dir.path().join("hw3");
        write(
            &workspace.join("Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&workspace.join("src/lib.rs"), "pub fn noop() {}\n");

        let spec = Spec::load(harness_dir.path()).unwrap();
        let driver_dir = harness_dir.path().join("harness");
        let prepared = prepare::prepare(&workspace, harness_dir.path(), &spec).unwrap();
        assert!(prepared.manifest_diagnostics.is_empty());

        let evaluator = build_evaluator_for(&spec, harness_dir.path(), LocalSandbox).unwrap();
        let ctx = JobContext {
            assignment_id: spec.assignment.id.clone(),
            student_id: "local".into(),
            run_id: "run-1".into(),
            workspace,
            driver_dir,
        };
        let eval = evaluator.evaluate(&ctx).unwrap();

        assert_eq!(eval.stages.build.status, model::StageStatus::Ok);
        assert_eq!(eval.stages.run.status, model::StageStatus::HarnessError);

        let report = CiReport {
            eval: Some(&eval),
            manifest_diagnostics: &prepared.manifest_diagnostics,
        };
        assert!(!report.passed());
    }

    #[test]
    fn ci_evaluator_selection_builds_binary_when_its_harness_dir_exists() {
        let toml = PUBLIC_SPEC.replace("kind = \"library\"", "kind = \"binary\"");
        let spec: Spec = toml::from_str(&toml).unwrap();

        let harness_dir = tempfile::tempdir().unwrap();
        write(&harness_dir.path().join("hw3/tests/judge.rs"), "");
        let result = build_evaluator_for(&spec, harness_dir.path(), LocalSandbox);
        assert!(result.is_ok());
    }

    #[test]
    fn ci_evaluator_selection_errors_clearly_when_binary_dir_is_missing() {
        let toml = PUBLIC_SPEC.replace("kind = \"library\"", "kind = \"binary\"");
        let spec: Spec = toml::from_str(&toml).unwrap();

        let harness_dir = tempfile::tempdir().unwrap();
        let result = build_evaluator_for(&spec, harness_dir.path(), LocalSandbox);
        assert!(matches!(result, Err(Error::InvalidSpec(_))));
    }

    /// `grade --local-sandbox` must never touch `ContainerSandbox` or
    /// require Podman.
    #[test]
    fn grade_with_local_sandbox_never_requires_podman() {
        let assignment_dir = tempfile::tempdir().unwrap();
        write(
            &assignment_dir.path().join(spec::PUBLIC_SPEC_FILE),
            PUBLIC_SPEC,
        );
        write(
            &assignment_dir.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nhw3 = { path = \"../hw3\" }\n",
        );
        write(
            &assignment_dir.path().join("harness/src/main.rs"),
            "fn main() {}\n",
        );

        let submissions_dir = tempfile::tempdir().unwrap();
        write(
            &submissions_dir.path().join("alice/hw3/Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(
            &submissions_dir.path().join("alice/hw3/src/lib.rs"),
            "pub fn noop() {}\n",
        );

        let config = Config {
            storage_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
        };

        run_grade(
            assignment_dir.path(),
            submissions_dir.path(),
            true,
            None,
            true,
            &config,
        )
        .unwrap();

        let store = Store::new(&config.storage_dir);
        let grades = store.latest_grades("hw3").unwrap();
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].status, "HarnessError");
    }
}

#[cfg(test)]
mod regrade_tests {
    use super::*;
    use model::{
        Diagnostics, EvaluationResult, ResourceUsage, StageReport, StageReports, TestResult,
        TestStatus,
    };

    fn write(path: &std::path::Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn spec_toml(scoring_block: &str) -> String {
        format!(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"

[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]

[limits.build]
wall-clock = "30s"
cpus = 1
memory = "512MiB"
pids = 64

[limits.run]
cpu-time = "5s"
wall-clock = "10s"
cpus = 1
memory = "256MiB"
pids = 64
max-output-bytes = "64KiB"

{scoring_block}
"#
        )
    }

    fn persisted_eval() -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            assignment_id: "hw3".into(),
            student_id: "alice".into(),
            run_id: "run-1".into(),
            graded_commit: None,
            instructor_commit: None,
            public_harness_commit: None,
            stages: StageReports {
                fetch: StageReport::ok(),
                build: StageReport::ok(),
                run: StageReport::ok(),
            },
            tests: vec![
                TestResult {
                    name: "insert_basic".into(),
                    status: TestStatus::Pass,
                    duration_ms: None,
                    message: None,
                    reported_score: None,
                },
                TestResult {
                    name: "balance_adversarial".into(),
                    status: TestStatus::Fail,
                    duration_ms: None,
                    message: None,
                    reported_score: None,
                },
            ],
            resource_usage: ResourceUsage::default(),
            diagnostics: Diagnostics::default(),
        }
    }

    #[test]
    fn regrade_recomputes_scores_from_a_changed_policy_without_reevaluating() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage_dir: store_dir.path().to_path_buf(),
        };

        write(
            &assignment_dir.path().join(spec::PRIVATE_SPEC_FILE),
            &spec_toml("[scoring]\nformula = \"sum\"\nbase = 0.0"),
        );
        let store = Store::new(&config.storage_dir);
        store.save_eval(&persisted_eval()).unwrap();

        run_regrade("hw3", assignment_dir.path(), &config).unwrap();
        let grades = store.latest_grades("hw3").unwrap();
        assert_eq!(grades.len(), 1);
        // insert_basic passes (1.0 default), balance_adversarial fails (0.0).
        assert_eq!(grades[0].score, 1.0);
        assert_eq!(grades[0].max, None);

        write(
            &assignment_dir.path().join(spec::PRIVATE_SPEC_FILE),
            &spec_toml(
                "[scoring]\nformula = \"affine\"\nmax-sum = 2.0\nscale-min = 0.0\nscale-max = 10.0",
            ),
        );
        run_regrade("hw3", assignment_dir.path(), &config).unwrap();
        let grades = store.latest_grades("hw3").unwrap();
        assert_eq!(grades[0].score, 5.0);
        assert_eq!(grades[0].max, Some(10.0));
    }

    #[test]
    fn regrade_applies_overrides_toml_on_top_of_the_recomputed_score() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let config = Config {
            storage_dir: store_dir.path().to_path_buf(),
        };

        write(
            &assignment_dir.path().join(spec::PRIVATE_SPEC_FILE),
            &spec_toml("[scoring]\nformula = \"sum\"\nbase = 0.0"),
        );
        write(
            &assignment_dir.path().join(overrides::OVERRIDES_FILE),
            "[manual.alice]\nscore = 25.0\nreason = \"Appeal granted for balance_adversarial\"\n",
        );
        let store = Store::new(&config.storage_dir);
        store.save_eval(&persisted_eval()).unwrap();

        run_regrade("hw3", assignment_dir.path(), &config).unwrap();

        let grades = store.latest_grades("hw3").unwrap();
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].score, 25.0);
        assert_eq!(grades[0].status, "override");
        assert!(grades[0].override_reason.is_some());
    }
}
