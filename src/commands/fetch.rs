use std::path::Path;

use crate::error::Result;
use crate::model::StageStatus;
use crate::spec::Spec;
use crate::submissions;
use crate::submissions::source::CsvRoster;

pub fn run(assignment: &Path, roster: &Path, out: &Path, as_of: Option<jiff::Zoned>) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let deadline = as_of.unwrap_or_else(|| spec.assignment.deadline.clone());

    let source = CsvRoster::new(roster);
    let records = submissions::fetch_batch(&source, out, &deadline)?;

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
