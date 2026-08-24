//! The report `autograder grade` prints to the terminal. The gradebook
//! CSV it writes alongside is [`super::csv`] -- this is the same data,
//! ordered and aggregated for reading rather than for import.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::path::Path;

use console::style;

use crate::model::{Grade, GradeOutcome};
use crate::render;
use crate::report::timing::{DateCheck, Lateness, Side};

/// One line of the listing: a score, plus everything known about it that
/// isn't the score. Assembled by `commands::grade`, which is the only
/// place that has both the evaluation and the fetch record in hand.
pub struct Row<'a> {
    pub grade: &'a Grade,
    pub lateness: Lateness,
    /// `None` when there was no commit to check -- see
    /// [`DateCheck::from_record`].
    pub date_check: Option<DateCheck>,
    /// This score's inputs changed since the run it was graded from: it's
    /// still what that run produced, it just isn't a score for what's on
    /// disk now.
    pub stale: bool,
}

/// Highest score first, so the scores worth a second look -- and the
/// unscored rows after them -- land next to the summary at the bottom,
/// which is what stays on screen after a class-sized listing scrolls.
pub fn render(rows: &[Row<'_>], gradebook: &Path) -> String {
    let mut s = String::new();

    if rows.is_empty() {
        let _ = writeln!(
            s,
            "\nNo evaluation results to grade. Run {} first.\n",
            style("autograder evaluate").bold()
        );
        return s;
    }

    let _ = writeln!(
        s,
        "\nGraded {} submissions  {}  {}\n",
        rows.len(),
        style("->").dim(),
        gradebook.display()
    );

    let mut rows: Vec<&Row> = rows.iter().collect();
    rows.sort_by(|a, b| {
        match (a.grade.score(), b.grade.score()) {
            (Some(x), Some(y)) => y.total_cmp(&x),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
        // Ties broken by name, so re-running doesn't reshuffle the list.
        .then_with(|| a.grade.github_user.cmp(&b.grade.github_user))
    });

    let width = rows
        .iter()
        .map(|r| r.grade.github_user.as_str().len())
        .max()
        .unwrap_or(0);

    for row in &rows {
        let grade = row.grade;
        let id = grade.github_user.as_str();
        let note = notes(row);
        match &grade.outcome {
            GradeOutcome::Scored {
                score,
                points,
                passed,
                total,
            } => {
                let tests = format!("{passed}/{total} tests");
                let tests = if passed == total {
                    tests
                } else {
                    style(tests).yellow().to_string()
                };
                let _ = writeln!(
                    s,
                    "  {id:<width$}  {score:>6.1}  {points:>5.1} pts    {tests}{note}"
                );
            }
            GradeOutcome::Unscored { reason } => {
                // Padded before it's styled: the escape codes count toward
                // a format width and would eat the alignment.
                let _ = writeln!(
                    s,
                    "  {id:<width$}  {}    {}{note}",
                    style(format!("{:>6}", "--")).red(),
                    style(reason).red()
                );
            }
        }
    }

    let scores: Vec<f64> = rows.iter().filter_map(|r| r.grade.score()).collect();
    let unscored = rows.len() - scores.len();

    let _ = writeln!(s);
    if unscored == 0 {
        let _ = writeln!(s, "  {} scored", scores.len());
    } else {
        let _ = writeln!(
            s,
            "  {} scored, {}",
            scores.len(),
            style(format!("{unscored} without a score")).red()
        );
    }
    if let Some(stats) = distribution(&scores) {
        let _ = writeln!(s, "  {stats}");
    }
    let late = rows
        .iter()
        .filter(|r| matches!(r.lateness, Lateness::Late(_)))
        .count();
    if late > 0 {
        let _ = writeln!(
            s,
            "  {}",
            style(format!("{late} submitted after the deadline")).red()
        );
    }
    // Counted, not listed: which ones they are is on their own lines, and
    // the point of the count is to say whether to go looking at all.
    let discrepancies = rows
        .iter()
        .filter(|r| !matches!(r.date_check, None | Some(DateCheck::Verified)))
        .count();
    if discrepancies > 0 {
        let _ = writeln!(
            s,
            "  {}",
            style(format!(
                "{discrepancies} with a submission date GitHub's push history doesn't confirm"
            ))
            .yellow()
        );
    }
    let stale = rows.iter().filter(|r| r.stale).count();
    if stale > 0 {
        let _ = writeln!(
            s,
            "  {}",
            style(format!(
                "{stale} graded from an evaluation older than the current content -- re-run autograder evaluate",
            ))
            .yellow()
        );
    }
    let _ = writeln!(s);
    s
}

/// Every annotation this row earned, in one string ready to append to the
/// line -- each already padded and styled, since a `{:<width$}` count of
/// styled text would include the escape codes.
///
/// A row can earn several: a late submission whose dates disagree is
/// exactly the row worth seeing both facts about.
fn notes(row: &Row<'_>) -> String {
    let mut notes = String::new();

    if let Lateness::Late(by) = row.lateness {
        let _ = write!(
            notes,
            "    {}",
            style(format!("{} late", render::duration(by))).red()
        );
    }

    match &row.date_check {
        None | Some(DateCheck::Verified) => {}
        Some(DateCheck::Unverified) => {
            let _ = write!(notes, "    {}", style("unverified date").yellow());
        }
        Some(DateCheck::Straddles {
            commit_date,
            push_event,
        }) => {
            let phrase = |side: &Side| match side {
                Side::OnTime => "on time",
                Side::Late => "late",
            };
            let _ = write!(
                notes,
                "    {}",
                style(format!(
                    "committed {}, pushed {}",
                    phrase(commit_date),
                    phrase(push_event)
                ))
                .yellow()
            );
        }
        Some(DateCheck::CommitAfterPush(by)) => {
            let _ = write!(
                notes,
                "    {}",
                style(format!("commit dated {} after its push", render::duration(*by))).red()
            );
        }
    }

    if row.stale {
        let _ = write!(notes, "    {}", style("stale").yellow());
    }

    notes
}

/// `None` for an empty set -- there's no mean of nothing, and the count
/// line above has already said so.
fn distribution(scores: &[f64]) -> Option<String> {
    let (first, last) = (scores.first()?, scores.last()?);
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    // `rows` is sorted descending, so `scores` arrives that way too.
    let median = if scores.len().is_multiple_of(2) {
        (scores[scores.len() / 2 - 1] + scores[scores.len() / 2]) / 2.0
    } else {
        scores[scores.len() / 2]
    };
    Some(format!(
        "mean {mean:.1}   median {median:.1}   min {last:.1}   max {first:.1}"
    ))
}
