use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::model::StageStatus;
use crate::spec::Spec;
use crate::submissions;
use crate::submissions::source::Submissions;

pub fn run(
    assignment: &Path,
    submissions_path: &Path,
    as_of: Option<jiff::Zoned>,
    config: &Config,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let deadline = as_of.unwrap_or_else(|| spec.assignment.deadline.clone());
    let work_dir = config.storage_dir.join(".work");

    let records = match Submissions::open(submissions_path)? {
        Submissions::Directory(source) => submissions::fetch_batch(&source, &work_dir, &deadline)?,
        Submissions::Csv(source) => submissions::fetch_batch(&source, &work_dir, &deadline)?,
    };

    for (student_id, record) in &records {
        if record.status == StageStatus::Ok {
            tracing::info!(
                %student_id,
                graded_commit = record.graded_commit.as_deref().unwrap_or(""),
                "fetched"
            );
        } else {
            tracing::warn!(
                %student_id,
                message = record.message.as_deref().unwrap_or(""),
                "fetch failed"
            );
        }
    }
    Ok(())
}
