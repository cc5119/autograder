pub mod csv;
pub mod json;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::Grade;

/// Emits reports from grades. v1 impls: `JsonReporter`, `CsvReporter`.
/// A `CiReporter` (public-tests-only, no scores) lands in M3.
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
    std::fs::write(path, contents).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}
