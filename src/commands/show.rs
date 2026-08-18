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
use crate::id::GithubUser;
use crate::model::{EvalStatus, EvaluationResult, InputHash, TestOutcome, TestStatus};
use crate::pipeline;
use crate::render;
use crate::spec::Spec;
use crate::submissions::{FetchRecord, FetchResult, SubmissionDate, read_fetch_record};

const EVAL_DIR: &str = ".eval";

pub fn run(submission: &Path, verbose: bool, assignment: Option<&Path>) -> Result<()> {
    let id = github_user(submission)?;
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
        Some((eval, run_count)) => {
            // Staleness needs both halves of the digest, and the
            // instructor half isn't reachable from a submission path --
            // without `--assignment` there's nothing to compare against,
            // so the line is omitted rather than guessed at.
            let current = match assignment {
                Some(assignment_dir) => {
                    let spec = Spec::load(assignment_dir)?;
                    Some(pipeline::hash::input_hash(
                        submission,
                        assignment_dir,
                        &spec,
                    )?)
                }
                None => None,
            };
            render_eval(&mut out, &eval, run_count, verbose, current);
        }
        None => {
            let _ = writeln!(out, "\n{}\n  not yet evaluated", style("Evaluate").bold());
        }
    }

    print!("{out}");
    Ok(())
}

fn github_user(submission: &Path) -> Result<GithubUser> {
    submission
        .file_name()
        .and_then(|s| s.to_str())
        .map(GithubUser::new)
        .ok_or_else(|| {
            Error::Other(format!(
                "{} is not a valid submission path",
                submission.display()
            ))
        })
}

fn render_fetch(out: &mut String, record: &FetchRecord) {
    let _ = writeln!(out, "\n{}", style("Fetch").bold());
    // The roster's own extra columns, verbatim -- whatever the instructor
    // put there (student id, email, section, ...).
    for (key, value) in &record.metadata {
        let _ = writeln!(out, "  {key:<13}  {value}");
    }
    let _ = writeln!(
        out,
        "  fetched at     {}  ({})",
        render::instant(record.fetched_at),
        render::relative(
            &record.fetched_at.to_zoned(jiff::tz::TimeZone::system()),
            &jiff::Zoned::now()
        )
    );
    if let Some(fork) = record.forks.first() {
        let _ = writeln!(out, "  repo           {}", fork.nwo());
    }
    // Ambiguity survives in the record even though the fetch went ahead.
    if record.forks.len() > 1 {
        let others: Vec<String> = record.forks[1..].iter().map(|f| f.nwo()).collect();
        let _ = writeln!(
            out,
            "  {}      also matched {}",
            style("ambiguous").yellow(),
            others.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "  deadline       {}",
        render::datetime(&record.deadline)
    );
    match &record.result {
        FetchResult::Failed { message } => {
            let _ = writeln!(out, "  status         {}", style("failed").red());
            let _ = writeln!(out, "  message        {message}");
        }
        FetchResult::Ok { submission_date } => {
            match submission_date.graded() {
                Some(commit) => {
                    let _ = writeln!(out, "  commit         {}", commit.sha);
                }
                None => {
                    let _ = writeln!(
                        out,
                        "  commit         {}",
                        style("none -- the fork has no commits").red()
                    );
                }
            }
            render_submission_date(out, record, submission_date);
        }
    }
}

fn render_submission_date(out: &mut String, record: &FetchRecord, date: &SubmissionDate) {
    let note = match date {
        SubmissionDate::Blessed { .. } => style("  (blessed)").to_string(),
        SubmissionDate::Override { reason, .. } => {
            let late = match record.late_by() {
                Some(by) => format!(", {} late", render::duration(by)),
                None => String::new(),
            };
            let reason = match reason {
                Some(reason) => format!(": {reason}"),
                None => String::new(),
            };
            style(format!("  (override{reason}{late})"))
                .yellow()
                .to_string()
        }
        // Late by the deadline on the record, so this needs nothing the
        // record doesn't already carry.
        SubmissionDate::Late(_) => match record.late_by() {
            Some(by) => style(format!("  ({} late)", render::duration(by)))
                .red()
                .to_string(),
            None => String::new(),
        },
        _ => String::new(),
    };

    match date.trusted_submitted_at() {
        Some(ts) => {
            let _ = writeln!(out, "  submitted at   {}{note}", render::instant(ts));
        }
        None if matches!(date, SubmissionDate::Empty) => {}
        None => {
            let _ = writeln!(
                out,
                "  submitted at   unknown (no verified timestamp){note}"
            );
        }
    }
}

/// The most recently persisted `EvaluationResult` for one submission under
/// `<submissions>/.eval/<id>/`, alongside the total number of runs on
/// record -- mirrors `commands::grade`'s `latest_evals`, but for a single
/// submission and keeping the count around for the caller to note.
fn latest_eval(
    submissions_dir: &Path,
    id: &GithubUser,
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

fn render_eval(
    out: &mut String,
    eval: &EvaluationResult,
    run_count: usize,
    verbose: bool,
    current: Option<InputHash>,
) {
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

    if let Some(current) = current
        && current != eval.input_hash
    {
        let _ = writeln!(
            out,
            "  stale          {}",
            style(format!(
                "inputs are {} now, this run graded {} -- re-run evaluate",
                current.short(),
                eval.input_hash.short()
            ))
            .yellow()
        );
    }

    let tests = match &eval.status {
        EvalStatus::BuildFailed(status) => {
            let _ = writeln!(
                out,
                "  status         {}",
                style(format!("build failed ({})", status.label())).red()
            );
            // Failures that never reached the compiler put their
            // explanation in `stderr_excerpt` instead.
            if verbose
                && let Some(detail) = eval
                    .diagnostics
                    .compiler_errors
                    .as_ref()
                    .or(eval.diagnostics.stderr_excerpt.as_ref())
            {
                let _ = writeln!(out, "\n{detail}");
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
