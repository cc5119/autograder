//! `autograder fetch`: works out which fork belongs to which student,
//! shows the plan, and only then clones anything. Discovery is read-only,
//! so declining at the prompt costs nothing.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use console::{Term, style};
use dialoguer::Confirm;

use crate::error::{Error, Result};
use crate::id::GithubUser;
use crate::render;
use crate::spec::Spec;
use crate::submissions::forks::Upstream;
use crate::submissions::source::CsvRoster;
use crate::submissions::{
    FetchPlan, FetchRecord, FetchResult, Plan, SubmissionDate, fetch_batch, plan_fetch,
};

pub fn run(
    assignment: &Path,
    repo: &Upstream,
    roster: &Path,
    out: &Path,
    as_of: Option<jiff::Zoned>,
    detach: Option<bool>,
    jobs: std::num::NonZeroUsize,
) -> Result<()> {
    let spec = Spec::load(assignment)?;
    let deadline = as_of.unwrap_or_else(|| spec.assignment.deadline.clone());

    let roster = CsvRoster::new(roster);
    let plan = plan_fetch(repo, &roster)?;

    print!("{}", render_plan(&plan, repo, &deadline, out));
    if plan.to_fetch() == 0 {
        println!("Nothing to fetch.");
        return Ok(());
    }

    // Before the fetch prompt: it changes what lands on disk, so it's part
    // of what the user is about to say yes to.
    let detach = resolve_detach(detach, out, plan.to_fetch())?;
    if !confirm(plan.to_fetch())? {
        tracing::info!("nothing fetched");
        return Ok(());
    }

    fetch_batch(&plan, out, &deadline, detach, jobs.get(), &report)?;
    Ok(())
}

/// Settles `--detach`/`--no-detach` when neither was passed. Only the
/// nested-checkout case is worth raising: outside a work tree there's no
/// reason to throw away history nobody asked us to throw away.
///
/// With no terminal to ask on we detach rather than refuse -- the flags
/// exist for anyone who wants to be explicit, and a scripted run inside a
/// repo is precisely where a pile of nested `.git` dirs does its damage.
fn resolve_detach(detach: Option<bool>, out: &Path, to_fetch: usize) -> Result<bool> {
    if let Some(detach) = detach {
        return Ok(detach);
    }
    let Some(worktree) = enclosing_worktree(out) else {
        return Ok(false);
    };

    println!(
        "  {} {} is inside the git repo at\n    {} -- {to_fetch} checkouts\n    would be nested repos git can't track.\n",
        style("⚠").yellow(),
        out.display(),
        worktree.display(),
    );

    if !Term::stdout().is_term() {
        tracing::info!(
            "detaching checkouts (no terminal to ask on); pass --no-detach to keep .git"
        );
        return Ok(true);
    }
    Confirm::new()
        .with_prompt("Detach checkouts (remove .git)?")
        .default(true)
        .interact()
        .map_err(|source| Error::Other(format!("failed to read confirmation: {source}")))
}

/// The work tree `out` would land in, if any. Resolved from `out`'s
/// nearest existing ancestor, since `out` itself usually doesn't exist
/// until the fetch creates it. Any `git` failure means "not in a work
/// tree" -- this only drives a warning, so it's not worth failing over.
fn enclosing_worktree(out: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(out).ok()?;
    let start = absolute.ancestors().find(|dir| dir.is_dir())?;
    let output = Command::new("git")
        .args([
            "-C",
            &start.display().to_string(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// One line per submission, printed the moment that submission lands
/// rather than after the whole batch -- with several fetches in flight
/// and nothing else on screen but a progress bar, waiting until the end
/// to say anything would leave a class-sized run silent for minutes.
fn report(github_user: &GithubUser, record: &FetchRecord) {
    match &record.result {
        // Late and empty submissions are fetched, not failed -- they're
        // logged loudly but grading, not fetching, decides what they're
        // worth.
        FetchResult::Ok { submission_date } => {
            let note = match submission_date {
                SubmissionDate::Late(_) => "late",
                SubmissionDate::Empty => "no commits",
                _ => "",
            };
            tracing::info!(
                %github_user,
                repo = record.forks.first().map(|f| f.nwo()).unwrap_or_default(),
                commit = record.graded_commit().map(|c| c.sha.as_str()).unwrap_or(""),
                note,
                "fetched"
            );
        }
        FetchResult::Failed { message } => {
            tracing::warn!(%github_user, message, "fetch failed");
        }
    }
}

/// The full manifest, in roster order: every student, the repo they'll be
/// fetched from, and why the exceptions are exceptions. No collapsing --
/// an org fork's owner and name say nothing about who it belongs to, so
/// the listing is the only place the attribution is visible.
fn render_plan(
    plan: &FetchPlan,
    upstream: &Upstream,
    deadline: &jiff::Zoned,
    out: &Path,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\nFetching forks of {}", style(upstream).bold());
    let _ = writeln!(s, "  deadline      {}", render::datetime(deadline));
    let _ = writeln!(
        s,
        "                {}",
        relative_deadline(deadline, &jiff::Zoned::now())
    );
    let _ = writeln!(s, "  destination   {}", out.display());
    let _ = writeln!(s);

    let width = plan
        .rows
        .iter()
        .map(|row| row.entry.github_user.as_str().len())
        .max()
        .unwrap_or(0);

    for row in &plan.rows {
        let id = row.entry.github_user.as_str();
        let student_id = row.entry.student_id.as_str();
        let (repo, note) = match &row.plan {
            Plan::Fetch { fork, also } if also.is_empty() => (fork.nwo(), String::new()),
            Plan::Fetch { fork, also } => (
                fork.nwo(),
                style(format!(
                    "ambiguous, also {}",
                    also.iter().map(|f| f.nwo()).collect::<Vec<_>>().join(", ")
                ))
                .yellow()
                .to_string(),
            ),
            Plan::Shared { forks, .. } => (
                "--".to_string(),
                style(format!(
                    "not fetched: {} is shared",
                    forks.iter().map(|f| f.nwo()).collect::<Vec<_>>().join(", ")
                ))
                .red()
                .to_string(),
            ),
            Plan::Missing { .. } => ("--".to_string(), style("no fork").red().to_string()),
        };
        let _ = writeln!(s, "  {id:<width$}  {student_id:<12}  {repo:<40}  {note}");
    }

    let fetching = plan.to_fetch();
    let shared = count(plan, |p| matches!(p, Plan::Shared { .. }));
    let missing = count(plan, |p| matches!(p, Plan::Missing { .. }));
    let ambiguous = count(
        plan,
        |p| matches!(p, Plan::Fetch { also, .. } if !also.is_empty()),
    );

    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  {} on the roster: {fetching} will be fetched, {} recorded as failed",
        plan.rows.len(),
        shared + missing
    );
    if missing > 0 {
        let _ = writeln!(s, "  {missing} with no fork");
    }
    if shared > 0 {
        let _ = writeln!(
            s,
            "  {shared} whose only fork is shared with another student -- fetched for neither"
        );
    }
    if ambiguous > 0 {
        let _ = writeln!(
            s,
            "  {ambiguous} ambiguous -- the repo listed first is the one fetched"
        );
    }
    if !plan.unmatched.is_empty() {
        let _ = writeln!(
            s,
            "  {} forks skipped, no roster student can push to them",
            plan.unmatched.len()
        );
    }
    let overwritten = plan
        .rows
        .iter()
        .filter(|row| matches!(row.plan, Plan::Fetch { .. }))
        .filter(|row| out.join(row.entry.github_user.as_str()).exists())
        .count();
    if overwritten > 0 {
        let _ = writeln!(
            s,
            "  {} existing checkouts under {} will be {}",
            overwritten,
            out.display(),
            style("overwritten").yellow()
        );
    }
    let _ = writeln!(s);
    s
}

fn count(plan: &FetchPlan, f: impl Fn(&Plan) -> bool) -> usize {
    plan.rows.iter().filter(|row| f(&row.plan)).count()
}

/// How long ago the deadline was -- fetching *before* it means grabbing
/// work students are still writing, which the timestamp alone doesn't say.
fn relative_deadline(deadline: &jiff::Zoned, now: &jiff::Zoned) -> String {
    let relative = render::relative(deadline, now);
    if deadline.timestamp() > now.timestamp() {
        format!(
            "({})",
            style(format!("{relative} -- submissions may be incomplete")).yellow()
        )
    } else {
        format!("({relative})")
    }
}

/// Nobody to ask means nobody to stop us: an unattended run fetches, and
/// overwrites whatever checkouts were there before.
fn confirm(to_fetch: usize) -> Result<bool> {
    if !Term::stdout().is_term() {
        return Ok(true);
    }

    Confirm::new()
        .with_prompt(format!("Fetch {to_fetch} repositories?"))
        .default(false)
        .interact()
        .map_err(|source| Error::Other(format!("failed to read confirmation: {source}")))
}

