use std::path::Path;

use crate::error::Result;
use crate::exec::fs;
use crate::id::GithubUser;
use crate::model::EvaluationResult;
use crate::pipeline::{self, grade};
use crate::report::timing::{DateCheck, Lateness};
use crate::report::{csv, summary};
use crate::spec::Spec;
use crate::submissions::read_fetch_record;

/// Where `evaluate_batch` persisted results, and where this command writes
/// the gradebook -- both alongside the submission checkouts under
/// `submissions_dir` (see `pipeline::evaluate_batch`'s doc comment).
const EVAL_DIR: &str = ".eval";
const GRADES_DIR: &str = ".grades";
const GRADES_FILE: &str = "grades.csv";

/// Computes scores from persisted `EvaluationResult`s (no student code, no
/// evaluator) -- applies `spec.scoring` fresh from disk every time, so
/// editing it always reflects the current policy, never one baked in
/// earlier. Writes a single gradebook CSV to
/// `<submissions>/.grades/grades.csv`.
pub fn run(assignment: &Path, submissions: &Path) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let evals = latest_evals(submissions)?;

    // One read per student, for the roster's extra columns and for when
    // the submission arrived -- an `EvaluationResult` carries no
    // timestamps, so timing (and therefore the late penalty) can only
    // come from the fetch record. Absent (e.g. an eval persisted before a
    // fetch ever ran) leaves both blank and unpenalized.
    let records: Vec<_> = evals
        .iter()
        .map(|eval| read_fetch_record(submissions, &eval.github_user))
        .collect::<Result<_>>()?;

    let grades: Vec<_> = evals
        .iter()
        .zip(records.iter())
        .map(|(eval, record)| {
            let late_by = record.as_ref().and_then(|r| r.late_by());
            grade::grade(eval, &spec.scoring, spec.late_penalty.as_ref(), late_by)
        })
        .collect();

    let metadata: Vec<_> = records
        .iter()
        .map(|record| {
            record
                .as_ref()
                .map(|record| record.metadata.clone())
                .unwrap_or_default()
        })
        .collect();
    let rows: Vec<_> = grades.iter().zip(metadata.iter()).collect();

    let stale = stale(&evals, submissions, assignment, &spec)?;

    let grades_dir = submissions.join(GRADES_DIR);
    let gradebook = grades_dir.join(GRADES_FILE);
    fs::create_dir_all(&grades_dir)?;
    fs::write(&gradebook, csv::render(&rows)?)?;

    let summary_rows: Vec<_> = grades
        .iter()
        .zip(records.iter())
        .map(|(grade, record)| summary::Row {
            grade,
            lateness: Lateness::from_record(record.as_ref()),
            date_check: DateCheck::from_record(record.as_ref()),
            stale: stale.contains(&grade.github_user),
        })
        .collect();
    print!("{}", summary::render(&summary_rows, &gradebook));
    Ok(())
}

/// The most recent persisted `EvaluationResult` per submission under
/// `<submissions>/.eval/`, by run_id sort order.
fn latest_evals(submissions_dir: &Path) -> Result<Vec<EvaluationResult>> {
    let eval_dir = submissions_dir.join(EVAL_DIR);
    if !eval_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut evals = Vec::new();
    for entry in fs::read_dir_entries(&eval_dir)? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(github_user) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(eval) = pipeline::latest_eval(submissions_dir, &GithubUser::new(github_user))? {
            evals.push(eval);
        }
    }
    Ok(evals)
}

/// Which of `evals` were computed from inputs that have since changed --
/// the submission's own content, or the assignment material judging it.
/// Grading them is still what the recorded run says; it just no longer
/// describes what's on disk.
fn stale(
    evals: &[EvaluationResult],
    submissions_dir: &Path,
    assignment_dir: &Path,
    spec: &Spec,
) -> Result<Vec<GithubUser>> {
    let mut stale = Vec::new();
    for eval in evals {
        let checkout_dir = submissions_dir.join(eval.github_user.as_str());
        let current = pipeline::hash::input_hash(&checkout_dir, assignment_dir, spec)?;
        if current != eval.input_hash {
            stale.push(eval.github_user);
        }
    }
    Ok(stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::json::write_json;
    use crate::exec::sandbox::ProcessStatus;
    use crate::model::InputHash;
    use crate::model::{Diagnostics, EvalStatus, TestOutcome, TestResult, TestStatus};

    fn write(path: &std::path::Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn spec_toml(scoring_block: &str) -> String {
        format!(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{sha}"

[sandbox]
image = "autograder-base:1.86.0"

[build-limits]
wall-clock = "30s"
cpus = 1
memory = "512MiB"
pids = 64
max-output-bytes = "64KiB"

{scoring_block}
"#,
            sha = "0".repeat(64)
        )
    }

    fn persisted_eval() -> EvaluationResult {
        EvaluationResult {
            assignment_id: "hw3".into(),
            github_user: "alice".into(),
            run_id: "run-1".into(),
            input_hash: InputHash::new("test"),
            status: EvalStatus::Ran {
                process: ProcessStatus::Exited(0),
                tests: TestOutcome::Tests(vec![
                    TestResult {
                        name: "insert_basic".into(),
                        status: TestStatus::Pass,
                        duration_ms: 0,
                        message: None,
                        reported_score: None,
                    },
                    TestResult {
                        name: "balance_adversarial".into(),
                        status: TestStatus::Fail,
                        duration_ms: 0,
                        message: None,
                        reported_score: None,
                    },
                ]),
            },
            wall_clock_ms: None,
            diagnostics: Diagnostics::default(),
        }
    }

    #[test]
    fn grade_recomputes_scores_from_a_changed_policy_without_reevaluating() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let submissions_dir = tempfile::tempdir().unwrap();

        write(
            &assignment_dir.path().join(crate::spec::SPEC_FILE),
            &spec_toml("[scoring]\nformula = \"sum\"\nbase = 0.0"),
        );
        write_json(
            &submissions_dir.path().join(".eval/alice/run-1.eval.json"),
            &persisted_eval(),
        )
        .unwrap();

        run(assignment_dir.path(), submissions_dir.path()).unwrap();
        let gradebook =
            std::fs::read_to_string(submissions_dir.path().join(".grades/grades.csv")).unwrap();
        // insert_basic passes (1.0 default), balance_adversarial fails (0.0).
        assert!(gradebook.contains("alice,1"));

        write(
            &assignment_dir.path().join(crate::spec::SPEC_FILE),
            &spec_toml(
                "[scoring]\nformula = \"affine\"\nmax-sum = 2.0\nscale-min = 0.0\nscale-max = 10.0",
            ),
        );
        run(assignment_dir.path(), submissions_dir.path()).unwrap();
        let gradebook =
            std::fs::read_to_string(submissions_dir.path().join(".grades/grades.csv")).unwrap();
        assert!(gradebook.contains("alice,5"));
    }
}
