use crate::error::Result;
use crate::exec::fs;
use crate::id::SubmissionId;
use crate::model::JobContext;
use crate::pipeline::{self, prepare};
use crate::report::ci::CiReport;
use crate::spec::Spec;

/// Student-facing `ci` entrypoint: Prepare + Build + Evaluate against the
/// public harness.
pub fn run(local_sandbox: bool) -> Result<()> {
    let harness_dir = fs::current_dir()?;
    let spec = Spec::load(&harness_dir)?;
    let workspace = harness_dir.join(spec.assignment.id.as_str());
    let run_id = pipeline::generate_run_id();
    let ctx = JobContext {
        assignment_id: spec.assignment.id,
        submission_id: SubmissionId::new("local"),
        run_id,
        workspace: workspace.clone(),
    };

    let prepared = prepare::prepare(&workspace, &harness_dir, &spec)?;

    let eval = if prepared.manifest_diagnostics.is_empty() {
        let evaluator = super::build_evaluator(&spec, &harness_dir, local_sandbox)?;
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
