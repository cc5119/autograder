use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::model::Grade;

use super::{Reporter, write_output};

/// Gradebook CSV: `student_id,score,max,status`.
pub struct CsvReporter {
    /// Destination file; `None` writes to stdout.
    pub out: Option<PathBuf>,
}

pub fn render(grades: &[Grade]) -> Result<String> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer
        .write_record(["student_id", "score", "max", "status"])
        .map_err(|source| Error::Csv {
            path: PathBuf::from("<gradebook>"),
            source: Box::new(source),
        })?;
    for grade in grades {
        writer
            .write_record([
                grade.student_id.to_string(),
                grade.score.to_string(),
                grade.max.map(|m| m.to_string()).unwrap_or_default(),
                grade.status.clone(),
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

impl Reporter for CsvReporter {
    fn report(&self, grades: &[Grade]) -> Result<()> {
        write_output(&self.out, &render(grades)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_gradebook_header_and_rows() {
        let grades = vec![
            Grade {
                student_id: "alice".into(),
                score: 10.0,
                max: Some(30.0),
                status: "fail".into(),
                failing_tests: vec!["balance_adversarial".into()],
            },
            Grade {
                student_id: "bob".into(),
                score: 30.0,
                max: Some(30.0),
                status: "pass".into(),
                failing_tests: vec![],
            },
        ];

        let csv = render(&grades).unwrap();
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("student_id,score,max,status"));
        assert_eq!(lines.next(), Some("alice,10,30,fail"));
        assert_eq!(lines.next(), Some("bob,30,30,pass"));
    }
}
