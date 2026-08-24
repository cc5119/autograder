use std::path::PathBuf;

use indexmap::IndexMap;

use crate::error::{Error, Result};
use crate::model::Grade;

/// Gradebook CSV: `github_user,Nota,<roster metadata columns...>`. `Nota`
/// is rounded to one decimal, the precision a grade is reported in, and is
/// blank when the build failed or the run left no readable results.
/// Metadata columns are the union of every row's keys, in the order each
/// key first appears -- when every fetch record carries the same roster
/// columns, that reproduces the roster's own column order. A row missing a
/// column another row has gets a blank cell for it.
pub fn render(rows: &[(&Grade, &IndexMap<String, String>)]) -> Result<String> {
    let mut columns: Vec<&str> = Vec::new();
    for (_, metadata) in rows {
        for key in metadata.keys() {
            if !columns.contains(&key.as_str()) {
                columns.push(key.as_str());
            }
        }
    }

    let mut writer = csv::Writer::from_writer(Vec::new());
    let mut header = vec!["github_user".to_string(), "Nota".to_string()];
    header.extend(columns.iter().map(|c| c.to_string()));
    writer.write_record(&header).map_err(|source| Error::Csv {
        path: PathBuf::from("<gradebook>"),
        source: Box::new(source),
    })?;
    for (grade, metadata) in rows {
        let mut record = vec![
            grade.github_user.to_string(),
            grade
                .score()
                .map(|s| format!("{s:.1}"))
                .unwrap_or_default(),
        ];
        record.extend(
            columns
                .iter()
                .map(|c| metadata.get(*c).cloned().unwrap_or_default()),
        );
        writer.write_record(&record).map_err(|source| Error::Csv {
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
        let alice = Grade {
            github_user: "alice".into(),
            outcome: crate::model::GradeOutcome::Scored {
                score: 10.0,
                points: 3.0,
                passed: 3,
                total: 3,
            },
        };
        let bob = Grade {
            github_user: "bob".into(),
            outcome: crate::model::GradeOutcome::Unscored {
                reason: "build failed: exited (101)".to_string(),
            },
        };
        let empty = IndexMap::new();

        let csv = render(&[(&alice, &empty), (&bob, &empty)]).unwrap();
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("github_user,Nota"));
        assert_eq!(lines.next(), Some("alice,10.0"));
        assert_eq!(lines.next(), Some("bob,"));
    }

    #[test]
    fn metadata_columns_follow_first_seen_order_and_blank_when_missing() {
        let alice = Grade {
            github_user: "alice".into(),
            outcome: crate::model::GradeOutcome::Scored {
                score: 10.0,
                points: 3.0,
                passed: 3,
                total: 3,
            },
        };
        let bob = Grade {
            github_user: "bob".into(),
            outcome: crate::model::GradeOutcome::Scored {
                score: 5.0,
                points: 1.0,
                passed: 1,
                total: 3,
            },
        };
        let alice_metadata: IndexMap<String, String> = [
            ("student_id".to_string(), "A123".to_string()),
            ("section".to_string(), "A".to_string()),
        ]
        .into_iter()
        .collect();
        // bob is missing `section` but has an extra `email` field.
        let bob_metadata: IndexMap<String, String> = [
            ("student_id".to_string(), "A456".to_string()),
            ("email".to_string(), "bob@x.edu".to_string()),
        ]
        .into_iter()
        .collect();

        let csv = render(&[(&alice, &alice_metadata), (&bob, &bob_metadata)]).unwrap();
        let mut lines = csv.lines();
        assert_eq!(
            lines.next(),
            Some("github_user,Nota,student_id,section,email")
        );
        assert_eq!(lines.next(), Some("alice,10.0,A123,A,"));
        assert_eq!(lines.next(), Some("bob,5.0,A456,,bob@x.edu"));
    }
}
