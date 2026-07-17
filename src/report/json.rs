use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::model::Grade;

use super::{Reporter, write_output};

/// Machine-readable per-student report: scores, max, status, and which
/// tests failed.
pub struct JsonReporter {
    /// Destination file; `None` writes to stdout.
    pub out: Option<PathBuf>,
}

pub fn render(grades: &[Grade]) -> Result<String> {
    serde_json::to_string_pretty(grades).map_err(|source| Error::Other(source.to_string()))
}

impl Reporter for JsonReporter {
    fn report(&self, grades: &[Grade]) -> Result<()> {
        write_output(&self.out, &render(grades)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_grades_with_failing_tests() {
        let grades = vec![Grade {
            student_id: "alice".into(),
            score: 10.0,
            max: 30.0,
            status: "fail".into(),
            failing_tests: vec!["balance_adversarial".into()],
        }];

        let json = render(&grades).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed[0]["student_id"], "alice");
        assert_eq!(parsed[0]["score"], 10.0);
        assert_eq!(parsed[0]["max"], 30.0);
        assert_eq!(parsed[0]["status"], "fail");
        assert_eq!(parsed[0]["failing_tests"][0], "balance_adversarial");
    }
}
