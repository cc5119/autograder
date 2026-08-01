use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::model::Grade;

/// Gradebook CSV: `github_user,score`. `score` is blank when the build
/// failed or the run left no readable results.
pub fn render(grades: &[Grade]) -> Result<String> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer
        .write_record(["github_user", "score"])
        .map_err(|source| Error::Csv {
            path: PathBuf::from("<gradebook>"),
            source: Box::new(source),
        })?;
    for grade in grades {
        writer
            .write_record([
                grade.github_user.to_string(),
                grade.score().map(|s| s.to_string()).unwrap_or_default(),
            ])
            .map_err(|source| Error::Csv {
                path: PathBuf::from("<gradebook>"),
                source: Box::new(source),
            })?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|e| Error::Other(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| Error::Other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_gradebook_header_and_rows() {
        let grades = vec![
            Grade {
                github_user: "alice".into(),
                outcome: crate::model::GradeOutcome::Scored {
                    score: 10.0,
                    passed: 3,
                    total: 3,
                },
            },
            Grade {
                github_user: "bob".into(),
                outcome: crate::model::GradeOutcome::Unscored {
                    reason: "build failed: exited (101)".to_string(),
                },
            },
        ];

        let csv = render(&grades).unwrap();
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("github_user,score"));
        assert_eq!(lines.next(), Some("alice,10"));
        assert_eq!(lines.next(), Some("bob,"));
    }
}
