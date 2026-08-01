//! The report `autograder grade` prints to the terminal. The gradebook
//! CSV it writes alongside is [`super::csv`] -- this is the same data,
//! ordered and aggregated for reading rather than for import.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::path::Path;

use console::style;

use crate::model::{Grade, GradeOutcome};

/// Highest score first, so the scores worth a second look -- and the
/// unscored rows after them -- land next to the summary at the bottom,
/// which is what stays on screen after a class-sized listing scrolls.
pub fn render(grades: &[Grade], gradebook: &Path) -> String {
    let mut s = String::new();

    if grades.is_empty() {
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
        grades.len(),
        style("->").dim(),
        gradebook.display()
    );

    let mut rows: Vec<&Grade> = grades.iter().collect();
    rows.sort_by(|a, b| {
        match (a.score(), b.score()) {
            (Some(x), Some(y)) => y.total_cmp(&x),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
        // Ties broken by name, so re-running doesn't reshuffle the list.
        .then_with(|| a.github_user.cmp(&b.github_user))
    });

    let width = rows
        .iter()
        .map(|g| g.github_user.as_str().len())
        .max()
        .unwrap_or(0);

    for grade in &rows {
        let id = grade.github_user.as_str();
        match &grade.outcome {
            GradeOutcome::Scored {
                score,
                passed,
                total,
            } => {
                let tests = format!("{passed}/{total} tests");
                let tests = if passed == total {
                    tests
                } else {
                    style(tests).yellow().to_string()
                };
                let _ = writeln!(s, "  {id:<width$}  {score:>6.1}    {tests}");
            }
            GradeOutcome::Unscored { reason } => {
                // Padded before it's styled: the escape codes count toward
                // a format width and would eat the alignment.
                let _ = writeln!(
                    s,
                    "  {id:<width$}  {}    {}",
                    style(format!("{:>6}", "--")).red(),
                    style(reason).red()
                );
            }
        }
    }

    let scores: Vec<f64> = rows.iter().filter_map(|g| g.score()).collect();
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
    let _ = writeln!(s);
    s
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
