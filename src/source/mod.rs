pub mod csv;

use std::path::Path;

use crate::error::{Error, Result};
use crate::fetch::DirectorySource;
use crate::model::Submission;

/// Where submissions come from, generic over the fetchable type `F` its
/// submissions carry. Impls: `CsvRoster` (`SubmissionsSource<GitRepo>`),
/// `DirectorySource` (`SubmissionsSource<LocalPath>`).
pub trait SubmissionsSource<F> {
    fn submissions(&self) -> Result<Vec<Submission<F>>>;
}

pub use csv::CsvRoster;

/// The `--submissions` path resolved to a concrete, correctly-typed
/// `SubmissionsSource`. There is no separate flag to pick the kind -- it's
/// inferred from what's on disk: a directory means one subdirectory per
/// student (`DirectorySource`, fetchable `LocalPath`); a file means a
/// roster CSV (`CsvRoster`, fetchable `GitRepo`).
pub enum Submissions {
    Directory(DirectorySource),
    Csv(CsvRoster),
}

impl Submissions {
    pub fn open(path: &Path) -> Result<Self> {
        if path.is_dir() {
            Ok(Submissions::Directory(DirectorySource::new(path)))
        } else if path.is_file() {
            Ok(Submissions::Csv(CsvRoster::new(path)))
        } else {
            Err(Error::InvalidSpec(format!(
                "--submissions path {} does not exist",
                path.display()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_path_resolves_to_directory_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("alice")).unwrap();

        let submissions = match Submissions::open(root.path()).unwrap() {
            Submissions::Directory(source) => source.submissions().unwrap(),
            Submissions::Csv(_) => panic!("expected a directory source"),
        };

        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].student_id, "alice");
    }

    #[test]
    fn a_file_path_resolves_to_csv_roster() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "student_id,repo_url,ref\nalice,https://example/alice.git,\n",
        )
        .unwrap();

        let submissions = match Submissions::open(file.path()).unwrap() {
            Submissions::Csv(source) => source.submissions().unwrap(),
            Submissions::Directory(_) => panic!("expected a csv source"),
        };

        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].student_id, "alice");
        assert_eq!(submissions[0].fetchable.url, "https://example/alice.git");
    }

    #[test]
    fn a_nonexistent_path_is_a_clear_error() {
        match Submissions::open(Path::new("/nonexistent/for/sure")) {
            Err(Error::InvalidSpec(_)) => {}
            Ok(_) => panic!("expected InvalidSpec for a nonexistent path"),
            Err(other) => panic!("expected InvalidSpec, got {other:?}"),
        }
    }
}
