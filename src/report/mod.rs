pub mod ci;
pub mod csv;
pub mod json;

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::model::Grade;

/// Emits reports from grades. v1 impls: `JsonReporter`, `CsvReporter`.
/// `ci::CiReport` (public-tests-only, no scores) renders a raw
/// `EvaluationResult` directly rather than a `Grade`, so it isn't a
/// `Reporter` impl — see its doc comment for why.
pub trait Reporter {
    fn report(&self, grades: &[Grade]) -> Result<()>;
}

/// Writes `contents` to `out`, or stdout if `out` is `None`.
fn write_output(out: &Option<PathBuf>, contents: &str) -> Result<()> {
    match out {
        Some(path) => write_file(path, contents),
        None => {
            print!("{contents}");
            Ok(())
        }
    }
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    crate::fs::write(path, contents)
}
