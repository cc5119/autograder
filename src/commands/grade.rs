use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::pipeline::{self, overrides};
use crate::spec::Spec;
use crate::store::Store;
use crate::submissions::source::Submissions;

pub fn run(
    assignment: &Path,
    submissions: &Path,
    do_fetch: bool,
    as_of: Option<jiff::Zoned>,
    local_sandbox: bool,
    config: &Config,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let evaluator = super::build_evaluator(&spec, assignment, local_sandbox)?;
    let store = Store::new(&config.storage_dir);
    let work_dir = config.storage_dir.join(".work");
    let overrides = overrides::Overrides::load_from_package(assignment)?;

    if do_fetch {
        super::fetch::run(assignment, submissions, as_of, config)?;
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

    super::report::write_reports(spec.assignment.id, &grades, config)
}
