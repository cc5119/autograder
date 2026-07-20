use std::path::PathBuf;

use crate::cli::ReportFormat;
use crate::config::Config;
use crate::error::Result;
use crate::exec::fs;
use crate::id::AssignmentId;
use crate::model::Grade;
use crate::report::{Reporter, csv::CsvReporter, json::JsonReporter};
use crate::store::Store;

pub fn run(
    assignment_id: AssignmentId,
    format: ReportFormat,
    out: Option<PathBuf>,
    config: &Config,
) -> Result<()> {
    let store = Store::new(&config.storage_dir);
    let grades = store.latest_grades(assignment_id)?;
    match format {
        ReportFormat::Json => JsonReporter { out }.report(&grades),
        ReportFormat::Csv => CsvReporter { out }.report(&grades),
    }
}

pub(crate) fn write_reports(
    assignment_id: AssignmentId,
    grades: &[Grade],
    config: &Config,
) -> Result<()> {
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
