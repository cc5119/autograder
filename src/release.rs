//! The Release stage: gives every roster student read access to one repo
//! as a *direct collaborator*, which is what lets them fork it without
//! letting them read each other's forks.
//!
//! The distinction is the whole point. A private fork inherits the
//! upstream's team permissions but not its individual ones, so a class
//! reading the starter repo through a team can read every submission
//! forked from it. Granting the same read student by student leaves the
//! forks private to their owners.
//!
//! Always `pull`. Forking needs read and nothing more, and a command that
//! hands a whole roster write access to the assignment repo is not one
//! worth being able to typo.
//!
//! One write per student, `PUT /repos/{owner}/{repo}/collaborators/{user}`.
//! Org members are added outright (204); anyone else gets an invitation
//! they have to accept (201). Nothing is persisted -- GitHub is the record,
//! and [`plan_release`] re-reads it from scratch every run.
//!
//! Add-only, like [`crate::enroll`]: it grants and it never revokes.
//! Direct collaborators with no roster row (staff, students who dropped)
//! are reported in the plan and otherwise left alone.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use jiff::Timestamp;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::github::{GH_BIN, check_users, paginated};
use crate::id::GithubUser;
use crate::render::{bar, spinner};
use crate::submissions::forks::Upstream;
use crate::submissions::source::{CsvRoster, RosterEntry};

/// The access level granted. Read is all a fork needs.
const PERMISSION: &str = "pull";

/// Where one roster row stands before anything is written. Only
/// [`Status::Grant`] gets the PUT.
#[derive(Debug, Clone)]
pub enum Status {
    /// Already a direct collaborator. Nothing to do -- at whatever level
    /// they hold, so a TA with `push` is never quietly downgraded to
    /// `pull`.
    HasAccess,
    /// Invited to the repo and hasn't accepted. Not re-sent: unlike the
    /// org invitation [`crate::enroll`] re-issues, this one *is* the grant,
    /// so there is nothing a second PUT would add.
    InvitePending { since: Timestamp },
    /// Not a collaborator. The PUT grants read, or invites if they're
    /// outside the org.
    Grant,
    /// No such GitHub login. Skipped -- almost always a typo in the roster,
    /// and the PUT would only 404 halfway through the batch.
    NoSuchUser,
}

impl Status {
    /// Whether this row is one [`release_batch`] will write.
    pub fn is_actionable(&self) -> bool {
        matches!(self, Status::Grant)
    }
}

#[derive(Debug, Clone)]
pub struct PlanRow {
    pub entry: RosterEntry,
    pub status: Status,
}

/// Everything `release_batch` is about to do, in roster order.
#[derive(Debug)]
pub struct ReleasePlan {
    pub repo: Upstream,
    pub rows: Vec<PlanRow>,
    /// Direct collaborators no roster row claims: staff, and students who
    /// dropped. Reported so a wrong `--repo` is obvious, never touched.
    pub extra: Vec<String>,
}

impl ReleasePlan {
    pub fn to_grant(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status.is_actionable())
            .count()
    }
}

/// Works out what releasing would do without writing anything: reads who
/// already has a direct grant, who has an invitation outstanding, and
/// whether any remaining handle exists at all.
pub fn plan_release(repo: &Upstream, roster: &CsvRoster, jobs: usize) -> Result<ReleasePlan> {
    let entries = roster.roster()?;

    let progress = spinner();
    progress.set_message(format!("reading who can already see {repo}..."));

    // Two independent reads, so they wait on each other for nothing.
    let (collaborators, invitations) = rayon::join(
        // `affiliation=direct` and not the default `all`: team members and
        // org owners see this repo too, and counting them as "already has
        // access" would skip the very grant this command exists to make.
        || direct_collaborators(repo),
        || pending_invitations(repo),
    );
    let (collaborators, pending) = (collaborators?, invitations?);

    progress.finish_and_clear();

    // The handles neither read accounted for -- the only ones worth a
    // lookup. Keyed by the lowercased handle, which dedupes a roster that
    // lists someone twice; the value is the entry's own spelling, which is
    // what gets asked about.
    let unknown: BTreeMap<String, GithubUser> = entries
        .iter()
        .map(|entry| (entry.github_user.as_str().to_lowercase(), entry.github_user))
        .filter(|(handle, _)| !collaborators.contains(handle) && !pending.contains_key(handle))
        .collect();
    let exists = check_users(&unknown, jobs)?;

    // Logins are case-preserving but not case-sensitive, and a roster is
    // hand-typed -- same comparison `forks::match_forks` makes.
    let mut rows = Vec::with_capacity(entries.len());
    let mut claimed = BTreeSet::new();
    for entry in entries {
        let handle = entry.github_user.as_str().to_lowercase();
        let status = if collaborators.contains(&handle) {
            claimed.insert(handle);
            Status::HasAccess
        } else if let Some(since) = pending.get(&handle) {
            Status::InvitePending { since: *since }
        } else if exists[&handle] {
            // Indexing is sound: reaching here is exactly the condition
            // `unknown` filtered on, so `check_users` looked this one up.
            Status::Grant
        } else {
            Status::NoSuchUser
        };
        rows.push(PlanRow { entry, status });
    }

    let extra = collaborators
        .iter()
        .filter(|login| !claimed.contains(*login))
        .cloned()
        .collect();

    Ok(ReleasePlan {
        repo: repo.clone(),
        rows,
        extra,
    })
}

/// What one PUT did. `Failed` is per-row, like [`crate::enroll::Outcome`]:
/// one student GitHub refuses shouldn't cost the rest of the class their
/// access.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// An org member -- access is live now, nothing to accept.
    Granted,
    /// Outside the org, so GitHub sent an invitation instead.
    Invited,
    Failed {
        message: String,
    },
}

/// Grants every actionable row of `plan`, in roster order, one at a
/// time -- these are writes against a single repo, and concurrent ones are
/// what GitHub's secondary rate limits exist to refuse. A class-sized
/// roster is a few seconds either way.
///
/// `on_result` is called once per row as it lands. Rows the plan already
/// settled are not visited.
pub fn release_batch(
    plan: &ReleasePlan,
    on_result: &dyn Fn(&GithubUser, &Outcome),
) -> Result<Vec<(GithubUser, Outcome)>> {
    let progress = bar(plan.to_grant() as u64);

    let mut results = Vec::new();
    for row in plan.rows.iter().filter(|row| row.status.is_actionable()) {
        let user = row.entry.github_user;
        progress.set_message(format!("granting {user}..."));
        let outcome = match add_collaborator(&plan.repo, user.as_str()) {
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

/// `PUT /repos/{owner}/{repo}/collaborators/{username}`, the one write this
/// command makes.
fn add_collaborator(repo: &Upstream, user: &str) -> Result<Outcome> {
    let output = Command::new(GH_BIN)
        .args([
            "api",
            "--method",
            "PUT",
            &format!("repos/{repo}/collaborators/{user}"),
            "-f",
            &format!("permission={PERMISSION}"),
        ])
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(classify_grant(&output.stdout))
}

/// GitHub answers 204 with an empty body when the user is an org member
/// and the grant takes effect immediately, and 201 with the invitation
/// when it doesn't. `gh` prints the body and nothing else, so an empty
/// stdout is the 204.
fn classify_grant(stdout: &[u8]) -> Outcome {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        Outcome::Granted
    } else {
        Outcome::Invited
    }
}

#[derive(Debug, Deserialize)]
struct RawCollaborator {
    login: String,
}

/// Only the direct grants: `affiliation=all` would also return everyone
/// who reaches this repo through a team or through org ownership, which is
/// precisely the access [`plan_release`] is trying to replace.
fn direct_collaborators(repo: &Upstream) -> Result<BTreeSet<String>> {
    Ok(
        paginated::<RawCollaborator>(&format!("repos/{repo}/collaborators?affiliation=direct"))?
            .into_iter()
            .map(|raw| raw.login.to_lowercase())
            .collect(),
    )
}

/// `invitee` is null for an invitation sent to an email address, which no
/// roster row can match -- those are dropped rather than guessed at.
#[derive(Debug, Deserialize)]
struct RawInvitation {
    invitee: Option<RawCollaborator>,
    created_at: Timestamp,
}

fn pending_invitations(repo: &Upstream) -> Result<BTreeMap<String, Timestamp>> {
    Ok(
        paginated::<RawInvitation>(&format!("repos/{repo}/invitations"))?
            .into_iter()
            .filter_map(|raw| Some((raw.invitee?.login.to_lowercase(), raw.created_at)))
            .collect(),
    )
}

/// The `gh`-shaped functions here need the network; what's tested is the
/// parsing and the classification that decides who gets written to.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_body_is_the_204_that_means_access_is_live() {
        assert!(matches!(classify_grant(b""), Outcome::Granted));
        assert!(matches!(classify_grant(b"\n"), Outcome::Granted));
    }

    #[test]
    fn an_invitation_body_is_the_201_that_means_they_must_accept() {
        let body = br#"{"id":42,"invitee":{"login":"alice"},"permissions":"read"}"#;
        assert!(matches!(classify_grant(body), Outcome::Invited));
    }

    #[test]
    fn invitations_without_an_invitee_are_dropped_rather_than_guessed_at() {
        let stdout = br#"[{"invitee":{"login":"Alice"},"created_at":"2026-02-01T00:00:00Z"},
                          {"invitee":null,"created_at":"2026-02-02T00:00:00Z"}]"#;
        let raw: Vec<RawInvitation> = crate::github::parse_pages(stdout).unwrap();
        let pending: BTreeMap<String, Timestamp> = raw
            .into_iter()
            .filter_map(|raw| Some((raw.invitee?.login.to_lowercase(), raw.created_at)))
            .collect();
        assert_eq!(pending.len(), 1);
        assert!(pending.contains_key("alice"));
    }

    #[test]
    fn only_rows_with_no_access_at_all_are_written_to() {
        assert!(Status::Grant.is_actionable());
        assert!(!Status::HasAccess.is_actionable());
        assert!(!Status::NoSuchUser.is_actionable());
        assert!(
            !Status::InvitePending {
                since: Timestamp::now()
            }
            .is_actionable()
        );
    }
}
