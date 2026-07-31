//! `autograder show`: prints one submission's persisted fetch record (if
//! any) and latest evaluate run (if any). Read-only -- never re-runs Fetch
//! or Evaluate, just renders what's already on disk under `.fetch/` and
//! `.eval/` next to the submission checkout.

use std::fmt::Write as _;
use std::path::Path;

use console::style;

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::json::read_json;
use crate::id::StudentId;
use crate::model::{EvalStatus, EvaluationResult, TestOutcome, TestStatus};
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
            let _ = writeln!(out, "\n{}\n  not yet evaluated", style("Evaluate").bold());
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
        .ok_or_else(|| {
            Error::Other(format!(
                "{} is not a valid submission path",
                submission.display()
            ))
        })
}

fn render_fetch(out: &mut String, record: &FetchRecord) {
    let _ = writeln!(out, "\n{}", style("Fetch").bold());
    let _ = writeln!(out, "  fetched at     {}", record.fetched_at);
    match record.status {
        FetchStatus::Failed => {
            let _ = writeln!(out, "  status         {}", style("failed").red());
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
fn latest_eval(
    submissions_dir: &Path,
    id: &StudentId,
) -> Result<Option<(EvaluationResult, usize)>> {
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
    let _ = writeln!(
        out,
        "\n{}  (run {}{runs_note})",
        style("Evaluate").bold(),
        eval.run_id
    );

    let tests = match &eval.status {
        EvalStatus::BuildFailed(status) => {
            let _ = writeln!(
                out,
                "  status         {}",
                style(format!("build failed ({})", status.label())).red()
            );
            if verbose && let Some(errors) = &eval.diagnostics.compiler_errors {
                let _ = writeln!(out, "\n{errors}");
            }
            return;
        }
        EvalStatus::Ran { process, tests } => {
            let _ = writeln!(out, "  process        {}", process.describe());
            match tests {
                TestOutcome::Unavailable(reason) => {
                    let _ = writeln!(
                        out,
                        "  tests          {}",
                        style(format!("unavailable ({reason})")).red()
                    );
                    if verbose && let Some(excerpt) = &eval.diagnostics.stderr_excerpt {
                        let _ = writeln!(out, "\n{excerpt}");
                    }
                    return;
                }
                TestOutcome::Tests(tests) => {
                    let _ = writeln!(out, "  tests          {}", style("ok").green());
                    tests
                }
            }
        }
    };

    if let Some(wall_clock_ms) = eval.wall_clock_ms {
        let _ = writeln!(out, "  wall clock     {wall_clock_ms}ms");
    }
    let _ = writeln!(out);

    let name_width = tests.iter().map(|t| t.name.len()).max().unwrap_or(0);
    let mut sorted_tests: Vec<_> = tests.iter().collect();
    sorted_tests.sort_by(|a, b| a.name.cmp(&b.name));

    let mut passed = 0;
    for test in sorted_tests {
        let mark = if test.status == TestStatus::Pass {
            passed += 1;
            style("\u{2713}").green().to_string()
        } else {
            style("\u{2717}").red().to_string()
        };
        // Padded to a fixed *visible* width by hand, not `{:<10}` -- that
        // format spec counts the ANSI escape bytes `style()` wraps a
        // failing annotation in, so it would under-pad once colored.
        let plain_annotation = match (test.status, test.reported_score) {
            (TestStatus::Pass, Some(score)) => format!("score={score}"),
            (TestStatus::Pass, None) => String::new(),
            (status, _) => test_status_label(status).to_string(),
        };
        let annotation_padding =
            " ".repeat(10usize.saturating_sub(plain_annotation.chars().count()));
        let annotation = if test.status == TestStatus::Pass {
            plain_annotation
        } else {
            style(plain_annotation).red().to_string()
        };
        let _ = writeln!(
            out,
            "  {mark} {:<name_width$}  {annotation}{annotation_padding}{:>6}ms",
            test.name, test.duration_ms
        );
        if verbose && let Some(message) = &test.message {
            for line in message.lines() {
                let _ = writeln!(out, "      \u{2502} {}", style(line).dim());
            }
            let _ = writeln!(out);
        }
    }
    let _ = writeln!(
        out,
        "\n  {}",
        style(format!("{passed}/{} tests passed", tests.len())).bold()
    );
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
