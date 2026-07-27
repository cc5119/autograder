use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::id::{AssignmentId, RunId, StudentId};
use crate::model::{EvaluationResult, Grade};

/// Persists `EvaluationResult`s and `Grade`s as JSON keyed by
/// `{assignment_id}/{student_id}/{run_id}` under the storage dir, so
/// grading/regrading is a fast offline re-computation.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn student_dir(&self, assignment_id: AssignmentId, student_id: StudentId) -> PathBuf {
        self.root
            .join(assignment_id.as_str())
            .join(student_id.as_str())
    }

    pub fn save_eval(&self, eval: &EvaluationResult) -> Result<PathBuf> {
        let dir = self.student_dir(eval.assignment_id, eval.student_id);
        let path = dir.join(format!("{}.eval.json", eval.run_id));
        write_json(&path, eval)?;
        Ok(path)
    }

    pub fn save_grade(
        &self,
        assignment_id: AssignmentId,
        run_id: RunId,
        grade: &Grade,
    ) -> Result<PathBuf> {
        let dir = self.student_dir(assignment_id, grade.student_id);
        let path = dir.join(format!("{run_id}.grade.json"));
        write_json(&path, grade)?;
        Ok(path)
    }

    /// The most recent persisted `EvaluationResult` per student for an
    /// assignment (by run_id sort order), or an empty vec if none exist.
    pub fn latest_evals(&self, assignment_id: AssignmentId) -> Result<Vec<EvaluationResult>> {
        self.latest_per_student(assignment_id, "eval.json")
    }

    /// The most recent persisted `Grade` per student for an assignment.
    pub fn latest_grades(&self, assignment_id: AssignmentId) -> Result<Vec<Grade>> {
        self.latest_per_student(assignment_id, "grade.json")
    }

    fn latest_per_student<T: serde::de::DeserializeOwned>(
        &self,
        assignment_id: AssignmentId,
        suffix: &str,
    ) -> Result<Vec<T>> {
        let assignment_dir = self.root.join(assignment_id.as_str());
        if !assignment_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut results = Vec::new();
        for entry in crate::exec::fs::read_dir_entries(&assignment_dir)? {
            if !entry.path().is_dir() {
                continue;
            }
            let mut runs: Vec<PathBuf> = crate::exec::fs::read_dir(&entry.path())?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.to_string_lossy().ends_with(suffix))
                .collect();
            runs.sort();
            if let Some(latest) = runs.last() {
                results.push(read_json(latest)?);
            }
        }
        Ok(results)
    }
}

pub(crate) fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        crate::exec::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(value).map_err(|e| Error::Other(e.to_string()))?;
    crate::exec::fs::write(path, json)
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let contents = crate::exec::fs::read_to_string(path)?;
    serde_json::from_str(&contents).map_err(|e| Error::Other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Diagnostics, ResourceUsage, StageReport, StageReports};

    fn eval(assignment_id: &str, student_id: &str, run_id: &str) -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            assignment_id: assignment_id.into(),
            student_id: student_id.into(),
            run_id: run_id.into(),
            graded_commit: None,
            instructor_commit: None,
            public_harness_commit: None,
            stages: StageReports {
                fetch: StageReport::ok(),
                build: StageReport::ok(),
                run: StageReport::ok(),
            },
            tests: vec![],
            resource_usage: ResourceUsage::default(),
            diagnostics: Diagnostics::default(),
        }
    }

    #[test]
    fn saves_and_reloads_the_latest_eval_per_student() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());

        store.save_eval(&eval("hw3", "alice", "run-1")).unwrap();
        store.save_eval(&eval("hw3", "alice", "run-2")).unwrap();
        store.save_eval(&eval("hw3", "bob", "run-1")).unwrap();

        let mut latest = store.latest_evals(AssignmentId::new("hw3")).unwrap();
        latest.sort_by(|a, b| a.student_id.cmp(&b.student_id));

        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].student_id, "alice");
        assert_eq!(latest[0].run_id, "run-2");
        assert_eq!(latest[1].student_id, "bob");
    }

    #[test]
    fn missing_assignment_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert!(
            store
                .latest_evals(AssignmentId::new("nope"))
                .unwrap()
                .is_empty()
        );
    }
}
