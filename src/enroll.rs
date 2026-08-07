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

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use jiff::Timestamp;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::github::{GH_BIN, check_users, logins, org_invitations, paginated};
use crate::id::GithubUser;
use crate::render::{bar, spinner};
use crate::submissions::source::{CsvRoster, RosterEntry};

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
    /// Invited to the org and hasn't accepted. Left alone: the invitation
    /// this command sent already queued the team membership, so a second
    /// PUT attaches nothing and may re-email them.
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
        !matches!(
            self,
            Status::OnTeam | Status::NoSuchUser | Status::InvitePending { .. }
        )
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
pub fn plan_enroll(org: &str, team: &str, roster: &CsvRoster, jobs: usize) -> Result<EnrollPlan> {
    let entries = roster.roster()?;

    let progress = spinner();
    progress.set_message(format!("looking up {team} in {org}..."));
    let team = find_team(org, team)?;

    // Three independent reads, so they wait on each other for nothing.
    progress.set_message(format!("reading members of {org}/{}...", team.slug));
    let (members, rest) = rayon::join(
        || logins(&format!("orgs/{org}/teams/{}/members", team.slug)),
        || {
            rayon::join(
                || logins(&format!("orgs/{org}/members")),
                || org_invitations(org),
            )
        },
    );
    let (members, org_members, pending) = (members?, rest.0?, rest.1?);

    progress.finish_and_clear();

    // The handles none of the three reads accounted for -- the only ones
    // worth a lookup. Keyed by the lowercased handle, which dedupes a
    // roster that lists someone twice; the value is the entry's own
    // spelling, which is what gets asked about.
    let unknown: BTreeMap<String, GithubUser> = entries
        .iter()
        .map(|entry| (entry.github_user.as_str().to_lowercase(), entry.github_user))
        .filter(|(handle, _)| {
            !members.contains(handle)
                && !org_members.contains(handle)
                && !pending.contains_key(handle)
        })
        .collect();
    let exists = check_users(&unknown, jobs)?;

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
        } else if exists[&handle] {
            // Indexing is sound: reaching here is exactly the condition
            // `unknown` filtered on, so `check_users` looked this one up.
            Status::Invite
        } else {
            Status::NoSuchUser
        };
        rows.push(PlanRow { entry, status });
    }

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

/// The `gh`-shaped functions here need the network; what's tested is the
/// parsing and the classification that decides who gets written to.
#[cfg(test)]
mod tests {
    use super::*;

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
    fn already_enrolled_pending_and_unknown_rows_are_the_ones_not_written_to() {
        assert!(!Status::OnTeam.is_actionable());
        assert!(!Status::NoSuchUser.is_actionable());
        assert!(
            !Status::InvitePending {
                since: Timestamp::now()
            }
            .is_actionable()
        );
        assert!(Status::Invite.is_actionable());
        assert!(Status::InOrg.is_actionable());
    }
}
