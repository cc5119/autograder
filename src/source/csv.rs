use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{GitRepo, Submission};

use super::SubmissionsSource;

const KNOWN_COLUMNS: &[&str] = &["student_id", "repo_url", "ref"];

/// A `SubmissionsSource<GitRepo>` backed by a CSV roster:
/// `student_id,repo_url,ref,email,section,...` (design §6). Columns beyond
/// `student_id`/`repo_url`/`ref` are carried into `Submission::metadata`.
/// `repo_url`/`ref` become the `GitRepo` fetchable, meant for a future
/// `GitHubFetcher` (M6) — its `Fetchable` impl is a stub until then.
pub struct CsvRoster {
    path: PathBuf,
}

impl CsvRoster {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl SubmissionsSource<GitRepo> for CsvRoster {
    fn submissions(&self) -> Result<Vec<Submission<GitRepo>>> {
        read_roster(&self.path)
    }
}

fn read_roster(path: &Path) -> Result<Vec<Submission<GitRepo>>> {
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

    let mut submissions = Vec::new();
    for result in reader.records() {
        let record = result.map_err(|source| Error::Csv {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;

        let mut student_id = None;
        let mut repo_url = None;
        let mut r#ref = None;
        let mut metadata = BTreeMap::new();

        for (header, value) in headers.iter().zip(record.iter()) {
            match header {
                "student_id" => student_id = Some(value.to_string()),
                "repo_url" => repo_url = Some(value.to_string()),
                "ref" => {
                    if !value.is_empty() {
                        r#ref = Some(value.to_string());
                    }
                }
                other if !KNOWN_COLUMNS.contains(&other) => {
                    metadata.insert(other.to_string(), value.to_string());
                }
                _ => {}
            }
        }

        let student_id = student_id.ok_or_else(|| {
            Error::InvalidSpec(format!("roster {path:?} missing student_id column"))
        })?;
        let repo_url = repo_url.ok_or_else(|| {
            Error::InvalidSpec(format!("roster {path:?} missing repo_url column"))
        })?;

        submissions.push(Submission {
            student_id,
            fetchable: GitRepo {
                url: repo_url,
                r#ref,
            },
            metadata,
        });
    }

    Ok(submissions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_sample_roster_with_extra_columns_as_metadata() {
        let mut file = tempfile_with_contents(
            "student_id,repo_url,ref,email,section\n\
             alice,https://github.com/alice/cse130-hw3.git,,alice@x.edu,A\n\
             bob,https://github.com/bob/cse130-hw3.git,main,bob@x.edu,B\n",
        );
        let submissions = read_roster(file.path()).unwrap();
        file.flush().unwrap();

        assert_eq!(submissions.len(), 2);

        assert_eq!(submissions[0].student_id, "alice");
        assert_eq!(
            submissions[0].fetchable.url,
            "https://github.com/alice/cse130-hw3.git"
        );
        assert_eq!(submissions[0].fetchable.r#ref, None);
        assert_eq!(
            submissions[0].metadata.get("email"),
            Some(&"alice@x.edu".to_string())
        );
        assert_eq!(submissions[0].metadata.get("section"), Some(&"A".to_string()));

        assert_eq!(submissions[1].student_id, "bob");
        assert_eq!(submissions[1].fetchable.r#ref, Some("main".to_string()));
    }

    fn tempfile_with_contents(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file
    }
}
