//! The Enroll stage: puts every roster student in one GitHub team, so
//! the forks they'll later submit live somewhere the instructor
//! administers. Add-only -- it invites and it adds, and it never removes
//! anyone from anything. Team members with no roster row (staff, students
//! who dropped) are counted in the plan and otherwise left alone.
//!
//! One write per student, `PUT /orgs/{org}/teams/{team}/memberships/{user}`:
//! GitHub sends the org invitation itself for a non-member and queues the
//! team membership until it's accepted, so the two things this command
//! promises are one request. Nothing is persisted -- GitHub is the record,
//! and [`plan_enroll`] re-reads it from scratch every run.
//!
//! Like [`crate::submissions::plan_fetch`], planning is entirely read-only:
//! declining at the prompt costs nothing. `gh` is the only GitHub client
//! (see [`crate::submissions::forks`]), so no token reaches this crate.

use std::collections::BTreeSet;
use std::process::Command;

use jiff::Timestamp;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::id::GithubUser;
use crate::render::{bar, spinner};
use crate::submissions::source::{CsvRoster, RosterEntry};

const GH_BIN: &str = "gh";

/// A team as the API needs it. `slug` is what every path takes; `name` is
/// the display name, kept because that's what the instructor typed and
/// what they'll recognize in the plan.
#[derive(Debug, Clone, Deserialize)]
pub struct Team {
    pub name: String,
    pub slug: String,
}

/// Where one roster row stands before anything is written. Everything but
/// [`Status::OnTeam`] and [`Status::NoSuchUser`] gets the one PUT.
#[derive(Debug, Clone)]
pub enum Status {
    /// Already on the team. Nothing to do.
    OnTeam,
    /// In the org but not this team -- the PUT adds them outright, no
    /// invitation involved.
    InOrg,
    /// Invited to the org and hasn't accepted. Still re-sent: the pending
    /// invitation says nothing about *this* team, and only the PUT queues
    /// the team membership for when they accept.
    InvitePending { since: Timestamp },
    /// Not in the org at all. The PUT invites them.
    Invite,
    /// No such GitHub login. Skipped -- almost always a typo in the roster,
    /// and the PUT would only 404 halfway through the batch.
    NoSuchUser,
}

impl Status {
    /// Whether this row is one [`enroll_batch`] will write.
    pub fn is_actionable(&self) -> bool {
        !matches!(self, Status::OnTeam | Status::NoSuchUser)
    }
}

#[derive(Debug, Clone)]
pub struct PlanRow {
    pub entry: RosterEntry,
    pub status: Status,
}

/// Everything `enroll_batch` is about to do, in roster order.
#[derive(Debug)]
pub struct EnrollPlan {
    pub org: String,
    pub team: Team,
    /// How many members the team already has, roster or not -- context for
    /// "am I pointed at the right team".
    pub team_size: usize,
    pub rows: Vec<PlanRow>,
    /// Team members no roster row claims: staff, and students who dropped.
    /// Reported so a wrong `--team` is obvious, never touched.
    pub extra: Vec<String>,
}

impl EnrollPlan {
    pub fn to_enroll(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status.is_actionable())
            .count()
    }
}

/// Works out what enrolling would do without writing anything: resolves
/// the team, reads who's already in it and in the org, and checks any
/// remaining handle actually exists.
///
/// A missing team is a hard error rather than something to create -- a
/// typo'd `--team` would otherwise make a junk team and invite the whole
/// class into it.
pub fn plan_enroll(org: &str, team: &str, roster: &CsvRoster) -> Result<EnrollPlan> {
    let entries = roster.roster()?;

    let progress = spinner();
    progress.set_message(format!("looking up {team} in {org}..."));
    let team = find_team(org, team)?;

    progress.set_message(format!("reading members of {org}/{}...", team.slug));
    let members = logins(&format!("orgs/{org}/teams/{}/members", team.slug))?;
    let org_members = logins(&format!("orgs/{org}/members"))?;
    let pending = pending_invitations(org)?;

    // Logins are case-preserving but not case-sensitive, and a roster is
    // hand-typed -- same comparison `forks::match_forks` makes.
    let mut rows = Vec::with_capacity(entries.len());
    let mut claimed = BTreeSet::new();
    for entry in entries {
        let handle = entry.github_user.as_str().to_lowercase();
        let status = if members.contains(&handle) {
            claimed.insert(handle);
            Status::OnTeam
        } else if org_members.contains(&handle) {
            Status::InOrg
        } else if let Some(since) = pending.get(&handle) {
            Status::InvitePending { since: *since }
        } else {
            // Only the handles nothing else accounted for: a login already
            // in the org demonstrably exists, so this stays one call per
            // *unenrolled* student rather than one per roster row.
            progress.set_message(format!("checking {}...", entry.github_user));
            match user_exists(entry.github_user.as_str())? {
                true => Status::Invite,
                false => Status::NoSuchUser,
            }
        };
        rows.push(PlanRow { entry, status });
    }
    progress.finish_and_clear();

    let extra = members
        .iter()
        .filter(|login| !claimed.contains(*login))
        .cloned()
        .collect();

    Ok(EnrollPlan {
        org: org.to_string(),
        team_size: members.len(),
        team,
        rows,
        extra,
    })
}

/// What one PUT did. `Failed` is per-row, like [`crate::submissions::FetchOutcome`]:
/// one student GitHub refuses shouldn't cost the rest of the class their
/// invitation.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// On the team now.
    Active,
    /// Invited; the team membership lands when they accept.
    Pending,
    Failed {
        message: String,
    },
}

/// Enrolls every actionable row of `plan`, in roster order, one at a
/// time -- these are writes against a single org, and concurrent ones are
/// what GitHub's secondary rate limits exist to refuse. A class-sized
/// roster is a few seconds either way.
///
/// `on_result` is called once per row as it lands. Rows the plan already
/// settled ([`Status::OnTeam`], [`Status::NoSuchUser`]) are not visited.
pub fn enroll_batch(
    plan: &EnrollPlan,
    on_result: &dyn Fn(&GithubUser, &Outcome),
) -> Result<Vec<(GithubUser, Outcome)>> {
    let progress = bar(plan.to_enroll() as u64);

    let mut results = Vec::new();
    for row in plan.rows.iter().filter(|row| row.status.is_actionable()) {
        let user = row.entry.github_user;
        progress.set_message(format!("enrolling {user}..."));
        let outcome = match add_to_team(&plan.org, &plan.team.slug, user.as_str()) {
            Ok(outcome) => outcome,
            Err(e) => Outcome::Failed {
                message: e.to_string(),
            },
        };
        progress.inc(1);
        // Through `suspend`, so the line is printed above the bar rather
        // than scribbled over by the next redraw.
        progress.suspend(|| on_result(&user, &outcome));
        results.push((user, outcome));
    }
    progress.finish_and_clear();

    Ok(results)
}

/// `PUT /orgs/{org}/teams/{team_slug}/memberships/{username}`, the one
/// write this command makes. Always as `member`: a maintainer can add and
/// remove people, which is not something a roster row should confer.
fn add_to_team(org: &str, slug: &str, user: &str) -> Result<Outcome> {
    let output = Command::new(GH_BIN)
        .args([
            "api",
            "--method",
            "PUT",
            &format!("orgs/{org}/teams/{slug}/memberships/{user}"),
            "-f",
            "role=member",
        ])
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    parse_membership(&output.stdout)
}

#[derive(Debug, Deserialize)]
struct RawMembership {
    state: String,
}

fn parse_membership(stdout: &[u8]) -> Result<Outcome> {
    let membership: RawMembership = serde_json::from_slice(stdout)
        .map_err(|source| Error::Other(format!("failed to parse `gh api` output: {source}")))?;
    match membership.state.as_str() {
        "active" => Ok(Outcome::Active),
        "pending" => Ok(Outcome::Pending),
        other => Err(Error::Other(format!(
            "GitHub reported an unknown membership state {other:?}"
        ))),
    }
}

/// The team `wanted` names, matched on slug or display name. Listing every
/// team costs one call and is what makes the not-found error useful: the
/// slug an instructor needs (`cs101-2026`) rarely looks like the name they
/// typed (`CS101 2026`).
fn find_team(org: &str, wanted: &str) -> Result<Team> {
    let teams: Vec<Team> = paginated(&format!("orgs/{org}/teams"))?;
    let needle = wanted.to_lowercase();
    teams
        .iter()
        .find(|team| team.slug.to_lowercase() == needle || team.name.to_lowercase() == needle)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = teams.iter().map(|t| t.slug.as_str()).collect();
            Error::Other(format!(
                "no team {wanted:?} in {org} -- create it first. {}",
                if known.is_empty() {
                    format!("{org} has no teams at all")
                } else {
                    format!("{org} has: {}", known.join(", "))
                }
            ))
        })
}

#[derive(Debug, Deserialize)]
struct RawLogin {
    login: String,
}

/// Lowercased, since every comparison against a hand-typed roster is
/// case-insensitive.
fn logins(path: &str) -> Result<BTreeSet<String>> {
    Ok(paginated::<RawLogin>(path)?
        .into_iter()
        .map(|raw| raw.login.to_lowercase())
        .collect())
}

/// `login` is null for an invitation sent to an email address, which no
/// roster row can match -- those are dropped rather than guessed at.
#[derive(Debug, Deserialize)]
struct RawInvitation {
    login: Option<String>,
    created_at: Timestamp,
}

fn pending_invitations(org: &str) -> Result<std::collections::BTreeMap<String, Timestamp>> {
    Ok(
        paginated::<RawInvitation>(&format!("orgs/{org}/invitations"))?
            .into_iter()
            .filter_map(|raw| Some((raw.login?.to_lowercase(), raw.created_at)))
            .collect(),
    )
}

/// `false` on a 404 and only a 404 -- anything else (no network, a revoked
/// token) is a real error, since reporting it as "no such user" would
/// quietly drop a student who does exist.
fn user_exists(user: &str) -> Result<bool> {
    let output = Command::new(GH_BIN)
        .args(["api", &format!("users/{user}")])
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("HTTP 404") || stderr.contains("Not Found") {
        return Ok(false);
    }
    Err(Error::Other(format!(
        "failed to look up GitHub user {user}: {}",
        stderr.trim()
    )))
}

/// One `gh api --paginate` read. `--paginate` concatenates pages as
/// `[...][...]` rather than one array, hence the streaming deserializer
/// (same as [`crate::submissions::forks`]).
fn paginated<T: serde::de::DeserializeOwned>(path: &str) -> Result<Vec<T>> {
    let output = Command::new(GH_BIN)
        .args(["api", path, "--paginate"])
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "failed to read {path}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_pages(&output.stdout)
}

fn parse_pages<T: serde::de::DeserializeOwned>(stdout: &[u8]) -> Result<Vec<T>> {
    let mut items = Vec::new();
    for page in serde_json::Deserializer::from_slice(stdout).into_iter::<Vec<T>>() {
        let page = page
            .map_err(|source| Error::Other(format!("failed to parse `gh api` output: {source}")))?;
        items.extend(page);
    }
    Ok(items)
}

/// The `gh`-shaped functions here need the network; what's tested is the
/// parsing and the classification that decides who gets written to.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pages_concatenates_gh_paginate_output() {
        let stdout = br#"[{"login":"alice"},{"login":"bob"}][{"login":"carol"}]"#;
        let logins: Vec<RawLogin> = parse_pages(stdout).unwrap();
        assert_eq!(logins.len(), 3);
        assert_eq!(logins[2].login, "carol");
    }

    #[test]
    fn parse_membership_maps_state_to_an_outcome() {
        let active = parse_membership(br#"{"role":"member","state":"active"}"#).unwrap();
        assert!(matches!(active, Outcome::Active));
        let pending = parse_membership(br#"{"role":"member","state":"pending"}"#).unwrap();
        assert!(matches!(pending, Outcome::Pending));
    }

    #[test]
    fn parse_membership_rejects_a_state_it_doesnt_understand() {
        let err = parse_membership(br#"{"role":"member","state":"banished"}"#).unwrap_err();
        assert!(err.to_string().contains("banished"), "{err}");
    }

    #[test]
    fn already_enrolled_and_unknown_rows_are_the_ones_not_written_to() {
        assert!(!Status::OnTeam.is_actionable());
        assert!(!Status::NoSuchUser.is_actionable());
        assert!(Status::Invite.is_actionable());
        assert!(Status::InOrg.is_actionable());
        assert!(
            Status::InvitePending {
                since: Timestamp::now()
            }
            .is_actionable()
        );
    }
}
