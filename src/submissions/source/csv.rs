use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::error::{Error, Result};
use crate::id::GithubUser;

const KNOWN_COLUMNS: &[&str] = &["github_user"];

/// One roster row.
#[derive(Debug, Clone)]
pub struct RosterEntry {
    /// The student's GitHub handle: what their fork is matched by, and the
    /// name their checkout dir gets (see [`crate::id::GithubUser`]).
    pub github_user: GithubUser,
    /// Every other column, in the order it appeared in the roster CSV.
    pub metadata: IndexMap<String, String>,
}

/// A CSV roster: `github_user,student_id,email,section,...`. Columns
/// beyond the required `github_user` are carried into `RosterEntry::metadata`,
/// in header order, and land verbatim in the fetch record. No repo column --
/// submissions are found by listing the upstream repo's forks and matching
/// `github_user` against each fork's owner (see
/// [`crate::submissions::forks`]).
pub struct CsvRoster {
    path: PathBuf,
}

impl CsvRoster {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn roster(&self) -> Result<Vec<RosterEntry>> {
        read_roster(&self.path)
    }
}

fn read_roster(path: &Path) -> Result<Vec<RosterEntry>> {
    let mut reader = csv::Reader::from_path(path).map_err(|source| Error::Csv {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    let headers = reader
        .headers()
        .map_err(|source| Error::Csv {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?
        .clone();

    let mut entries = Vec::new();
    for result in reader.records() {
        let record = result.map_err(|source| Error::Csv {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;

        let mut github_user = None;
        let mut metadata = IndexMap::new();

        for (header, value) in headers.iter().zip(record.iter()) {
            match header {
                "github_user" => github_user = non_empty(value),
                other if !KNOWN_COLUMNS.contains(&other) => {
                    metadata.insert(other.to_string(), value.to_string());
                }
                _ => {}
            }
        }

        let github_user = github_user.ok_or_else(|| missing(path, "github_user", &record))?;

        entries.push(RosterEntry {
            github_user: GithubUser::new(github_user),
            metadata,
        });
    }

    Ok(entries)
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// One message for both "no such column" and "the cell is blank" -- the
/// fix is the same either way, and the row is quoted so it's findable.
fn missing(path: &Path, column: &str, record: &csv::StringRecord) -> Error {
    Error::InvalidSpec(format!(
        "roster {path:?} is missing a {column} for row {:?}",
        record.iter().collect::<Vec<_>>().join(",")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_a_roster_with_extra_columns_as_metadata() {
        let file = tempfile_with_contents(
            "github_user,student_id,email,section\n\
             alice-gh,A12345678,alice@x.edu,A\n\
             bob-gh,A87654321,bob@x.edu,B\n",
        );
        let entries = read_roster(file.path()).unwrap();

        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].github_user, "alice-gh");
        assert_eq!(
            entries[0].metadata.get("student_id"),
            Some(&"A12345678".to_string())
        );
        assert_eq!(
            entries[0].metadata.get("email"),
            Some(&"alice@x.edu".to_string())
        );
        assert_eq!(entries[0].metadata.get("section"), Some(&"A".to_string()));
        assert_eq!(
            entries[0].metadata.keys().collect::<Vec<_>>(),
            vec!["student_id", "email", "section"]
        );

        assert_eq!(entries[1].github_user, "bob-gh");
    }

    #[test]
    fn a_missing_github_user_column_is_an_error() {
        let file = tempfile_with_contents("student_id,email\nA12345678,alice@x.edu\n");
        let err = read_roster(file.path()).unwrap_err().to_string();
        assert!(err.contains("github_user"), "{err}");
    }

    fn tempfile_with_contents(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }
}
