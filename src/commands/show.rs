//! `autograder show`: prints one submission's persisted fetch record (if
//! any) and latest evaluate run (if any). Read-only -- never re-runs Fetch
//! or Evaluate, just renders what's already on disk under `.fetch/` and
//! `.eval/` next to the submission checkout.

use std::fmt::Write as _;
use std::path::Path;

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::json::read_json;
use crate::id::StudentId;
use crate::model::{EvalStatus, EvaluationResult, RunStatus, TestStatus};
use crate::submissions::{FetchRecord, FetchStatus, SubmissionDate, read_fetch_record};

const EVAL_DIR: &str = ".eval";

pub fn run(submission: &Path, verbose: bool) -> Result<()> {
    let id = student_id(submission)?;
    let submissions_dir = submission.parent().ok_or_else(|| {
        Error::Other(format!(
            "{} has no parent directory to look for .fetch/.eval records in",
            submission.display()
        ))
    })?;

    let mut out = String::new();
    let _ = writeln!(out, "{id}  ({})", submission.display());

    if let Some(record) = read_fetch_record(submissions_dir, &id)? {
        render_fetch(&mut out, &record);
    }

    match latest_eval(submissions_dir, &id)? {
        Some((eval, run_count)) => render_eval(&mut out, &eval, run_count, verbose),
        None => {
            let _ = writeln!(out, "\nEvaluate\n  not yet evaluated");
        }
    }

    print!("{out}");
    Ok(())
}

fn student_id(submission: &Path) -> Result<StudentId> {
    submission
        .file_name()
        .and_then(|s| s.to_str())
        .map(StudentId::new)
        .ok_or_else(|| Error::Other(format!("{} is not a valid submission path", submission.display())))
}

fn render_fetch(out: &mut String, record: &FetchRecord) {
    let _ = writeln!(out, "\nFetch");
    let _ = writeln!(out, "  fetched at     {}", record.fetched_at);
    match record.status {
        FetchStatus::Failed => {
            let _ = writeln!(out, "  status         failed");
            if let Some(message) = &record.message {
                let _ = writeln!(out, "  message        {message}");
            }
        }
        FetchStatus::Ok => {
            if let Some(commit) = &record.graded_commit {
                let _ = writeln!(out, "  commit         {commit}");
            }
            if let Some(date) = &record.submission_date {
                render_submission_date(out, date);
            }
        }
    }
}

fn render_submission_date(out: &mut String, date: &SubmissionDate) {
    let blessed = matches!(date, SubmissionDate::Blessed { .. });
    match date.trusted_submitted_at() {
        Some(ts) if blessed => {
            let _ = writeln!(out, "  submitted at   {ts}  (blessed)");
        }
        Some(ts) => {
            let _ = writeln!(out, "  submitted at   {ts}");
        }
        None => {
            let _ = writeln!(out, "  submitted at   unknown (no verified timestamp)");
        }
    }
}

/// The most recently persisted `EvaluationResult` for one submission under
/// `<submissions>/.eval/<id>/`, alongside the total number of runs on
/// record -- mirrors `commands::grade`'s `latest_evals`, but for a single
/// submission and keeping the count around for the caller to note.
fn latest_eval(submissions_dir: &Path, id: &StudentId) -> Result<Option<(EvaluationResult, usize)>> {
    let dir = submissions_dir.join(EVAL_DIR).join(id.as_str());
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut runs: Vec<_> = fs::read_dir_entries(&dir)?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".eval.json"))
        .collect();
    runs.sort();
    let Some(latest) = runs.last() else {
        return Ok(None);
    };
    Ok(Some((read_json(latest)?, runs.len())))
}

fn render_eval(out: &mut String, eval: &EvaluationResult, run_count: usize, verbose: bool) {
    let runs_note = if run_count > 1 {
        format!(", {run_count} runs on record")
    } else {
        String::new()
    };
    let _ = writeln!(out, "\nEvaluate  (run {}{runs_note})", eval.run_id);

    match &eval.status {
        EvalStatus::BuildFailed(status) => {
            let _ = writeln!(out, "  status         build failed ({})", status.label());
            if verbose
                && let Some(errors) = &eval.diagnostics.compiler_errors
            {
                let _ = writeln!(out, "\n{errors}");
            }
            return;
        }
        EvalStatus::Ran(status) if *status != RunStatus::Ok => {
            let _ = writeln!(out, "  status         run failed ({})", status.label());
            if verbose
                && let Some(excerpt) = &eval.diagnostics.stderr_excerpt
            {
                let _ = writeln!(out, "\n{excerpt}");
            }
            return;
        }
        EvalStatus::Ran(RunStatus::Ok) => {
            let _ = writeln!(out, "  status         ok");
        }
        EvalStatus::Ran(_) => unreachable!("handled by the RunStatus::Ok arm above"),
    }

    if let Some(wall_clock_ms) = eval.wall_clock_ms {
        let _ = writeln!(out, "  wall clock     {wall_clock_ms}ms");
    }
    let _ = writeln!(out);

    let name_width = eval.tests.iter().map(|t| t.name.len()).max().unwrap_or(0);
    let mut passed = 0;
    for test in &eval.tests {
        let mark = if test.status == TestStatus::Pass {
            passed += 1;
            '\u{2713}'
        } else {
            '\u{2717}'
        };
        let annotation = match (test.status, test.reported_score) {
            (TestStatus::Pass, Some(score)) => format!("score={score}"),
            (TestStatus::Pass, None) => String::new(),
            (status, _) => test_status_label(status).to_string(),
        };
        let _ = writeln!(
            out,
            "  {mark} {:<name_width$}  {:<10}{:>6}ms",
            test.name, annotation, test.duration_ms
        );
        if verbose
            && let Some(message) = &test.message
        {
            let _ = writeln!(out, "      {message}");
        }
    }
    let _ = writeln!(out, "\n  {passed}/{} tests passed", eval.tests.len());
}

fn test_status_label(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Pass => "pass",
        TestStatus::Fail => "fail",
        TestStatus::Timeout => "timeout",
        TestStatus::Oom => "out of memory",
        TestStatus::Error => "error",
    }
}
