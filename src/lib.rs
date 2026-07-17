pub mod cli;
pub mod config;
pub mod error;
pub mod evaluator;
pub mod fetch;
pub mod grade;
pub mod manifest_check;
pub mod model;
pub mod pipeline;
pub mod prepare;
pub mod report;
pub mod sandbox;
pub mod source;
pub mod spec;
pub mod store;
pub mod vendor;

use cli::{Command, ReportFormat};
pub use config::Config;
pub use error::{Error, Result};

use evaluator::linked_library::LinkedLibrary;
use evaluator::Evaluator;
use grade::{DefaultGrader, Grader};
use report::{Reporter, csv::CsvReporter, json::JsonReporter};
use sandbox::ContainerSandbox;
use source::Submissions;
use spec::{AssignmentKind, Spec};
use store::Store;

pub fn dispatch(command: Command, config: &Config) -> Result<()> {
    match command {
        Command::Prefetch { assignment } => run_prefetch(&assignment),
        Command::Grade {
            assignment,
            submissions,
            jobs: _,
            as_of: _,
        } => run_grade(&assignment, &submissions, config),
        Command::Ci { .. } => Err(Error::NotImplemented("ci")),
        Command::Regrade {
            assignment_id,
            assignment,
        } => run_regrade(&assignment_id, &assignment, config),
        Command::Report {
            assignment_id,
            format,
            out,
        } => run_report(&assignment_id, format, out, config),
        Command::Scaffold { .. } => Err(Error::NotImplemented("scaffold")),
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
    config: &Config,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let evaluator = build_evaluator(&spec, assignment, config)?;
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
/// a `ContainerSandbox` (design §10). `binary-harness` lands in M4 (step
/// 22). Live grading needs podman + the assignment's base image + nextest
/// inside it — **[deferred: needs podman]** here; the orchestration and
/// junit parsing this drives are unit-tested in `evaluator::linked_library`.
fn build_evaluator(
    spec: &Spec,
    package_dir: &std::path::Path,
    config: &Config,
) -> Result<Box<dyn Evaluator>> {
    match spec.assignment.kind {
        AssignmentKind::LinkedLibrary => {
            let base_image = format!("autograder-base:{}", spec.toolchain.channel);
            let sandbox = ContainerSandbox::new(base_image, config.seccomp_profile.clone());
            Ok(Box::new(LinkedLibrary::new(spec, package_dir, sandbox)?))
        }
        AssignmentKind::BinaryHarness => Err(Error::NotImplemented(
            "binary-harness evaluator (lands in M4)",
        )),
    }
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
