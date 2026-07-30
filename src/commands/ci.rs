use crate::deps::vendor;
use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::overlay::{self, Context};
use crate::id::StudentId;
use crate::model::JobContext;
use crate::pipeline::{self, manifest_check::ManifestDiagnostic};
use crate::report::ci::CiReport;
use crate::spec::Spec;
use crate::str_map;

/// Student-facing `ci` entrypoint: Prepare + Build + Evaluate against the public harness.
pub fn run(local_sandbox: bool) -> Result<()> {
    let checkout_dir = fs::current_dir()?;
    let spec = Spec::load(&checkout_dir)?;

    // Stands in for the instructor's assignment_dir
    let assignment_scratch = fs::temp_dir()?;
    let assignment_dir = assignment_scratch.path();
    let subs = str_map! {"id" => spec.assignment.id, "harness" => spec.assignment.harness};
    overlay::apply(
        &Context::new(&checkout_dir, subs.clone()),
        assignment_dir,
        &pipeline::checkout_rules(),
    )?;
    overlay::apply(
        &Context::new(&checkout_dir, subs),
        assignment_dir,
        &pipeline::package_rules(),
    )?;

    // `vendor::vendor` checks `Cargo.lock` against the blessed hash itself
    // and refuses to run against a mismatch -- caught here and turned into
    // a same `LockfileMismatch` diagnostic.
    let manifest_diagnostics = match vendor::vendor(assignment_dir, &spec) {
        Ok(_) => Vec::new(),
        Err(Error::InvalidSpec(message)) => vec![ManifestDiagnostic::LockfileMismatch(message)],
        Err(other) => return Err(other),
    };

    let eval = if manifest_diagnostics.is_empty() {
        let evaluator = super::build_evaluator(&spec, assignment_dir, local_sandbox)?;

        let build_scratch = fs::temp_dir()?;
        let ctx = JobContext {
            assignment_id: spec.assignment.id,
            student_id: StudentId::new("local"),
            run_id: pipeline::generate_run_id(),
            workspace: build_scratch.path().to_path_buf(),
        };

        Some(pipeline::evaluate_submission(
            &ctx,
            &checkout_dir,
            assignment_dir,
            &spec,
            evaluator.as_ref(),
        )?)
    } else {
        None
    };

    let report = CiReport {
        eval: eval.as_ref(),
        manifest_diagnostics: &manifest_diagnostics,
    };
    print!("{}", report.render());

    if !report.passed() {
        std::process::exit(1);
    }
    Ok(())
}
