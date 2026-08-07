//! `autograder release`: shows who would gain read access to a repo, and
//! only then writes anything. Discovery is read-only, so declining at the
//! prompt costs nothing.

use std::fmt::Write as _;
use std::path::Path;

use console::{Term, style};
use dialoguer::Confirm;

use crate::error::{Error, Result};
use crate::id::GithubUser;
use crate::release::{Outcome, ReleasePlan, Status, plan_release, release_batch};
use crate::render;
use crate::submissions::forks::Upstream;
use crate::submissions::source::CsvRoster;

pub fn run(repo: &Upstream, roster: &Path, yes: bool, jobs: std::num::NonZeroUsize) -> Result<()> {
    let roster = CsvRoster::new(roster);
    let plan = plan_release(repo, &roster, jobs.get())?;

    print!("{}", render_plan(&plan));
    if !confirm(plan.to_grant(), yes)? {
        tracing::info!("nobody granted access");
        return Ok(());
    }

    release_batch(&plan, &report)?;
    Ok(())
}

/// One line per student the moment their grant lands, for the same reason
/// `enroll` reports as it goes: a class-sized run is otherwise minutes of
/// a progress bar and nothing else.
fn report(github_user: &GithubUser, outcome: &Outcome) {
    match outcome {
        Outcome::Granted => tracing::info!(%github_user, "granted read access"),
        Outcome::Invited => tracing::info!(%github_user, "invited"),
        Outcome::Failed { message } => tracing::warn!(%github_user, message, "release failed"),
    }
}

/// The full manifest, in roster order: every student and what they'd get.
/// No collapsing -- who already has access and who is a typo is exactly
/// what the instructor is being asked to confirm.
fn render_plan(plan: &ReleasePlan) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "\nReleasing {} to {} students",
        style(&plan.repo).bold(),
        plan.rows.len()
    );
    let _ = writeln!(s, "  access        read, as a direct collaborator");
    let _ = writeln!(s);

    let width = plan
        .rows
        .iter()
        .map(|row| row.entry.github_user.as_str().len())
        .max()
        .unwrap_or(0);

    let now = jiff::Zoned::now();
    for row in &plan.rows {
        let id = row.entry.github_user.as_str();
        let note = match &row.status {
            Status::Grant => "grant read".to_string(),
            Status::InvitePending { since } => style(format!(
                "invite pending since {}",
                render::relative(&since.to_zoned(jiff::tz::TimeZone::system()), &now)
            ))
            .yellow()
            .to_string(),
            Status::HasAccess => style("already has access").dim().to_string(),
            Status::NoSuchUser => style("no such GitHub user").red().to_string(),
        };
        let _ = writeln!(s, "  {id:<width$}  {note}");
    }

    let granting = plan.to_grant();
    let has_access = count(plan, |st| matches!(st, Status::HasAccess));
    let unknown = count(plan, |st| matches!(st, Status::NoSuchUser));

    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  {} on the roster: {granting} to grant, {has_access} already have access",
        plan.rows.len()
    );
    if unknown > 0 {
        let _ = writeln!(
            s,
            "  {unknown} skipped: no such GitHub user -- check the roster's handles"
        );
    }
    if !plan.extra.is_empty() {
        let _ = writeln!(
            s,
            "  {} with access but not on the roster: {}",
            plan.extra.len(),
            plan.extra.join(", ")
        );
    }
    let _ = writeln!(s);
    s
}

fn count(plan: &ReleasePlan, f: impl Fn(&Status) -> bool) -> usize {
    plan.rows.iter().filter(|row| f(&row.status)).count()
}

/// Refuses rather than assumes when there's no one to ask: releasing hands
/// real people access to a repo, so an unattended run has to say `--yes`
/// out loud.
fn confirm(to_grant: usize, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if to_grant == 0 {
        println!("Nobody to grant access to.");
        return Ok(false);
    }
    if !Term::stdout().is_term() {
        return Err(Error::Other(
            "refusing to release without confirmation: not running on a terminal, pass --yes"
                .to_string(),
        ));
    }

    Confirm::new()
        .with_prompt(format!("Grant read access to {to_grant} students?"))
        .default(false)
        .interact()
        .map_err(|source| Error::Other(format!("failed to read confirmation: {source}")))
}
