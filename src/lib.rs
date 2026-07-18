pub mod cache;
pub mod cli;
pub mod config;
pub mod error;
pub mod evaluator;
pub mod fetch;
pub mod grade;
pub mod image;
pub mod manifest_check;
pub mod model;
pub mod pipeline;
pub mod prepare;
pub mod publish;
pub mod report;
pub mod sandbox;
pub mod scaffold;
pub mod source;
pub mod spec;
pub mod store;
pub mod stub;
pub mod vendor;
pub mod volume;

use cli::{Command, ReportFormat};
pub use config::Config;
pub use error::{Error, Result};

use evaluator::Evaluator;
use evaluator::binary_harness::BinaryHarness;
use evaluator::linked_library::LinkedLibrary;
use grade::{DefaultGrader, Grader};
use model::{JobContext, Tier};
use report::{Reporter, ci::CiReport, csv::CsvReporter, json::JsonReporter};
use sandbox::{ContainerSandbox, LocalSandbox, Sandbox};
use source::Submissions;
use spec::{AssignmentKind, Spec};
use store::Store;

pub fn dispatch(command: Command, config: &Config) -> Result<()> {
    match command {
        Command::Prefetch { assignment } => run_prefetch(&assignment),
        Command::Grade {
            assignment,
            submissions,
            as_of: _,
            local_sandbox,
        } => run_grade(&assignment, &submissions, local_sandbox, config),
        Command::Ci { harness } => run_ci(&harness),
        Command::Regrade {
            assignment_id,
            assignment,
        } => run_regrade(&assignment_id, &assignment, config),
        Command::Report {
            assignment_id,
            format,
            out,
        } => run_report(&assignment_id, format, out, config),
        Command::Scaffold { assignment, out } => run_scaffold(&assignment, &out),
    }
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

fn run_grade(
    assignment: &std::path::Path,
    submissions: &std::path::Path,
    local_sandbox: bool,
    config: &Config,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let evaluator = if local_sandbox {
        tracing::warn!(
            "grading with --local-sandbox: skipping Podman entirely, running student code as a \
             host process with no container isolation -- for local development/ \
             testing only, never for grading real submissions"
        );
        build_local_evaluator(&spec, assignment)?
    } else {
        build_evaluator(&spec, assignment, config)?
    };
    let store = Store::new(&config.storage_dir);
    let work_dir = config.storage_dir.join(".work");

    // Each Submission's fetchable knows how to fetch itself
    // (fetch::Fetchable), so only the source kind needs picking here. A CSV
    // roster's GitRepo submissions aren't fetchable yet —
    // Fetchable::fetch for GitRepo is a stub until GitHubFetcher lands in
    // M6 (design §7.1) — so that branch fails clearly instead of silently
    // misinterpreting the URL as a path.
    let grades = match Submissions::open(submissions)? {
        Submissions::Directory(source) => pipeline::grade_batch(
            &source,
            evaluator.as_ref(),
            &DefaultGrader,
            assignment,
            &spec,
            &work_dir,
            &store,
        )?,
        Submissions::Csv(_) => {
            return Err(Error::NotImplemented(
                "grading from a CSV roster requires GitHubFetcher (M6); \
                 use a --submissions directory for local/dev runs",
            ));
        }
    };

    write_reports(&spec.assignment.id, &grades, config)
}

/// Picks the `Evaluator` for `spec.assignment.kind` (design §9), wired over
/// a `ContainerSandbox` (design §10). Runs `ContainerSandbox::preflight`
/// once up front so a broken Podman setup fails the whole `grade`
/// invocation with one clear error instead of silently scoring every
/// student `build_failed` — see its doc comment.
fn build_evaluator(
    spec: &Spec,
    package_dir: &std::path::Path,
    config: &Config,
) -> Result<Box<dyn Evaluator>> {
    let base_image = image::base_image_tag(spec);
    let sandbox = ContainerSandbox::new(base_image, config.seccomp_profile.clone());
    sandbox.preflight()?;
    match spec.assignment.kind {
        AssignmentKind::LinkedLibrary => {
            Ok(Box::new(LinkedLibrary::new(spec, package_dir, sandbox)?))
        }
        AssignmentKind::BinaryHarness => {
            Ok(Box::new(BinaryHarness::new(spec, package_dir, sandbox)?))
        }
    }
}

/// Student-facing `ci` entrypoint (design §11): Prepare + Build + Evaluate
/// against the public harness at `harness_dir` (an `.autograder/public/`
/// vendored into the student's own repo), over `LocalSandbox` rather than
/// `ContainerSandbox` — no podman needed, and this is the same reason
/// `LocalSandbox` exists at all (design §11.3). Runs against the current
/// directory: `ci` executes inside the student's own checkout, there is no
/// separate fetch stage. Prints per-test feedback via `CiReport` and exits
/// non-zero when any public test fails, mirroring `autograder ci`'s exit
/// code contract (design §11.1).
fn run_ci(harness_dir: &std::path::Path) -> Result<()> {
    let spec = Spec::load(harness_dir)?;
    let workspace = std::env::current_dir().map_err(|source| Error::Io {
        path: std::path::PathBuf::from("."),
        source,
    })?;
    let run_id = pipeline::generate_run_id();
    // Outside the student's own repo entirely -- never nested inside
    // `workspace` (see `JobContext::driver_dir`'s doc comment), so `ci`
    // never leaves build artifacts in the student's own checkout.
    let driver_dir = std::env::temp_dir().join(format!("autograder-ci-{run_id}"));

    let prepared = prepare::prepare(&workspace, &driver_dir, harness_dir, &spec)?;

    let eval = if prepared.manifest_diagnostics.is_empty() {
        let evaluator = build_local_evaluator(&spec, harness_dir)?;
        let ctx = JobContext {
            assignment_id: spec.assignment.id.clone(),
            student_id: "local".into(),
            run_id,
            tier: Tier::Ci,
            workspace,
            driver_dir,
        };
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

/// Picks the `Evaluator` for `spec.assignment.kind`, same construction as
/// `build_evaluator` but over `LocalSandbox` instead of `ContainerSandbox` —
/// no Podman needed. Used both by the CI tier (design §11.3, always) and by
/// `grade --local-sandbox` (never for real submissions — see that flag's
/// doc comment in `cli.rs`).
fn build_local_evaluator(spec: &Spec, package_dir: &std::path::Path) -> Result<Box<dyn Evaluator>> {
    match spec.assignment.kind {
        AssignmentKind::LinkedLibrary => Ok(Box::new(LinkedLibrary::new(
            spec,
            package_dir,
            LocalSandbox,
        )?)),
        AssignmentKind::BinaryHarness => Ok(Box::new(BinaryHarness::new(
            spec,
            package_dir,
            LocalSandbox,
        )?)),
    }
}

fn run_scaffold(assignment: &std::path::Path, out: &std::path::Path) -> Result<()> {
    let outcome = scaffold::scaffold(assignment, out)?;
    tracing::info!(out_dir = %outcome.out_dir.display(), "scaffold complete");
    Ok(())
}

fn run_regrade(assignment_id: &str, assignment: &std::path::Path, config: &Config) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let store = Store::new(&config.storage_dir);
    let evals = store.latest_evals(assignment_id)?;

    let mut grades = Vec::new();
    for eval in &evals {
        let grade = DefaultGrader.grade(eval, &spec.scoring);
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
    std::fs::create_dir_all(&reports_dir).map_err(|source| Error::Io {
        path: reports_dir.clone(),
        source,
    })?;

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
kind = "linked-library"
deadline = "2026-02-14T23:59:59-08:00"

[toolchain]
channel = "1.86.0"

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
model = "weighted"
[[scoring.tests]]
name = "insert_basic"
visibility = "public"
"#;

    /// Real end-to-end short of `cargo-nextest`, over `LocalSandbox` (no
    /// podman needed, per the ground rules): loads the public spec,
    /// prepares the workspace, builds the driver, and runs it. This host has
    /// no `cargo-nextest` installed, so the run stage never produces a junit
    /// report -- the evaluator correctly reports `HarnessError` rather than
    /// crashing or silently reporting a pass, exactly the deferred boundary
    /// the ground rules describe for M2's podman dependency.
    #[test]
    fn ci_pipeline_runs_prepare_and_evaluate_end_to_end_short_of_nextest() {
        let harness_dir = tempfile::tempdir().unwrap();
        write(
            &harness_dir.path().join(spec::PUBLIC_SPEC_FILE),
            PUBLIC_SPEC,
        );
        write(
            &harness_dir.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nhw3 = \"*\"\n",
        );
        write(
            &harness_dir.path().join("harness/src/main.rs"),
            "fn main() {}\n",
        );

        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join("Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(&workspace.path().join("src/lib.rs"), "pub fn noop() {}\n");

        let driver_dir = tempfile::tempdir().unwrap();
        let spec = Spec::load(harness_dir.path()).unwrap();
        let prepared = prepare::prepare(
            workspace.path(),
            driver_dir.path(),
            harness_dir.path(),
            &spec,
        )
        .unwrap();
        assert!(prepared.manifest_diagnostics.is_empty());

        let evaluator = build_local_evaluator(&spec, harness_dir.path()).unwrap();
        let ctx = JobContext {
            assignment_id: spec.assignment.id.clone(),
            student_id: "local".into(),
            run_id: "run-1".into(),
            tier: Tier::Ci,
            workspace: workspace.path().to_path_buf(),
            driver_dir: driver_dir.path().to_path_buf(),
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
    fn ci_evaluator_selection_builds_binary_harness_when_its_harness_dir_exists() {
        let toml = PUBLIC_SPEC.replace("kind = \"linked-library\"", "kind = \"binary-harness\"");
        let spec: Spec = toml::from_str(&toml).unwrap();

        let harness_dir = tempfile::tempdir().unwrap();
        write(&harness_dir.path().join("harness/tests/judge.rs"), "");
        let result = build_local_evaluator(&spec, harness_dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn ci_evaluator_selection_errors_clearly_when_binary_harness_dir_is_missing() {
        let toml = PUBLIC_SPEC.replace("kind = \"linked-library\"", "kind = \"binary-harness\"");
        let spec: Spec = toml::from_str(&toml).unwrap();

        let harness_dir = tempfile::tempdir().unwrap();
        let result = build_local_evaluator(&spec, harness_dir.path());
        assert!(matches!(result, Err(Error::InvalidSpec(_))));
    }

    /// `grade --local-sandbox` must never touch `ContainerSandbox` (and so
    /// never require Podman) -- regression guard for the flag added so
    /// `grade` still works on a host where Podman isn't usable. Real
    /// end-to-end short of `cargo-nextest`, same as the test above: `run_grade`
    /// completes (an `Ok(())`, not an `Err` about a missing/broken podman
    /// binary) and persists a `HarnessError` grade rather than a `BuildFailed`
    /// one -- `BuildFailed`/podman-preflight errors are exactly what this
    /// flag is meant to route around.
    #[test]
    fn grade_with_local_sandbox_never_requires_podman() {
        let assignment_dir = tempfile::tempdir().unwrap();
        write(
            &assignment_dir.path().join(spec::PUBLIC_SPEC_FILE),
            PUBLIC_SPEC,
        );
        write(
            &assignment_dir.path().join("harness/Cargo.toml"),
            "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nhw3 = \"*\"\n",
        );
        write(
            &assignment_dir.path().join("harness/src/main.rs"),
            "fn main() {}\n",
        );

        let submissions_dir = tempfile::tempdir().unwrap();
        write(
            &submissions_dir.path().join("alice/Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            &submissions_dir.path().join("alice/src/lib.rs"),
            "pub fn noop() {}\n",
        );

        let config = Config {
            storage_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..Config::default()
        };

        run_grade(assignment_dir.path(), submissions_dir.path(), true, &config).unwrap();

        let store = Store::new(&config.storage_dir);
        let grades = store.latest_grades("hw3").unwrap();
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].status, "HarnessError");
    }
}
