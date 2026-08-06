//! `autograder enroll`: shows what enrolling every roster student
//! would do, and only then writes anything. Discovery is read-only, so
//! declining at the prompt costs nothing.

use std::fmt::Write as _;
use std::path::Path;

use console::{Term, style};
use dialoguer::Confirm;

use crate::enroll::{EnrollPlan, Outcome, Status, enroll_batch, plan_enroll};
use crate::error::{Error, Result};
use crate::id::GithubUser;
use crate::render;
use crate::submissions::source::CsvRoster;

pub fn run(
    roster: &Path,
    org: &str,
    team: &str,
    yes: bool,
    jobs: std::num::NonZeroUsize,
) -> Result<()> {
    let roster = CsvRoster::new(roster);
    let plan = plan_enroll(org, team, &roster, jobs.get())?;

    print!("{}", render_plan(&plan));
    if !confirm(plan.to_enroll(), yes)? {
        tracing::info!("nobody enrolled");
        return Ok(());
    }

    enroll_batch(&plan, &report)?;
    Ok(())
}

/// One line per student the moment their invitation lands, for the same
/// reason `fetch` reports as it goes: a class-sized run is otherwise
/// minutes of a progress bar and nothing else.
fn report(github_user: &GithubUser, outcome: &Outcome) {
    match outcome {
        Outcome::Active => tracing::info!(%github_user, "added to the team"),
        Outcome::Pending => tracing::info!(%github_user, "invited"),
        Outcome::Failed { message } => tracing::warn!(%github_user, message, "enroll failed"),
    }
}

/// The full manifest, in roster order: every student and what they'd get.
/// No collapsing -- who is already enrolled and who is a typo is exactly
/// what the instructor is being asked to confirm.
fn render_plan(plan: &EnrollPlan) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "\nEnrolling {} students in {}",
        plan.rows.len(),
        style(&plan.org).bold()
    );
    let _ = writeln!(
        s,
        "  team          {} ({}, {} members)",
        style(&plan.team.name).bold(),
        plan.team.slug,
        plan.team_size
    );
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
            Status::Invite => "invite".to_string(),
            Status::InOrg => "org member, add to team".to_string(),
            Status::InvitePending { since } => style(format!(
                "invite pending since {}",
                render::relative(&since.to_zoned(jiff::tz::TimeZone::system()), &now)
            ))
            .yellow()
            .to_string(),
            Status::OnTeam => style("already on the team").dim().to_string(),
            Status::NoSuchUser => style("no such GitHub user").red().to_string(),
        };
        let _ = writeln!(s, "  {id:<width$}  {note}");
    }

    let enrolling = plan.to_enroll();
    let on_team = count(plan, |st| matches!(st, Status::OnTeam));
    let unknown = count(plan, |st| matches!(st, Status::NoSuchUser));

    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "  {} on the roster: {enrolling} to enroll, {} already on the team",
        plan.rows.len(),
        on_team
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
            "  {} on the team but not on the roster: {}",
            plan.extra.len(),
            plan.extra.join(", ")
        );
    }
    let _ = writeln!(s);
    s
}

fn count(plan: &EnrollPlan, f: impl Fn(&Status) -> bool) -> usize {
    plan.rows.iter().filter(|row| f(&row.status)).count()
}

/// Refuses rather than assumes when there's no one to ask: enrolling
/// emails real people, so an unattended run has to say `--yes` out loud.
fn confirm(to_enroll: usize, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if to_enroll == 0 {
        println!("Nobody to enroll.");
        return Ok(false);
    }
    if !Term::stdout().is_term() {
        return Err(Error::Other(
            "refusing to enroll without confirmation: not running on a terminal, pass --yes"
                .to_string(),
        ));
    }

    Confirm::new()
        .with_prompt(format!("Enroll {to_enroll} students?"))
        .default(false)
        .interact()
        .map_err(|source| Error::Other(format!("failed to read confirmation: {source}")))
}
