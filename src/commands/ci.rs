use crate::deps::vendor;
use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::overlay::{self, Context};
use crate::id::GithubUser;
use crate::model::JobContext;
use crate::pipeline;
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
    let vendor_scratch = fs::temp_dir()?;
    let vendor_dir = vendor_scratch.path().join("vendor");
    let lockfile_mismatch = match vendor::vendor(assignment_dir, &vendor_dir, &spec) {
        Ok(()) => None,
        Err(Error::InvalidSpec(message)) => Some(message),
        Err(other) => return Err(other),
    };

    let eval = if lockfile_mismatch.is_none() {
        let evaluator = super::build_evaluator(&spec, assignment_dir, &vendor_dir, local_sandbox)?;

        let build_scratch = fs::temp_dir()?;
        let ctx = JobContext {
            assignment_id: spec.assignment.id,
            github_user: GithubUser::new("local"),
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
        lockfile_mismatch: lockfile_mismatch.as_deref(),
    };
    print!("{}", report.render());

    if !report.passed() {
        std::process::exit(1);
    }
    Ok(())
}
