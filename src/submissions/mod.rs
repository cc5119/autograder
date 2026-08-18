//! The Fetch stage: pulls each student's submission onto disk at
//! `out_dir/<github_user>/` (flat) and a `FetchRecord` at
//! `out_dir/.fetch/<github_user>.json`, independently of grading --
//! `fetch_batch` is what `autograder fetch` runs. Neither `autograder
//! evaluate` nor `autograder grade` touch this module or read a
//! `FetchRecord` back -- a submission directory dropped in by hand works
//! exactly like one `fetch_batch` produced. [`read_fetch_record`] exists
//! for whatever external tooling wants to know what a prior fetch did.
//!
//! Submissions aren't listed anywhere: the roster names students by GitHub
//! handle and [`forks`] finds each one's fork of the upstream assignment
//! repo. Cloning goes through `gh` (so private forks work off the
//! instructor's existing login) and everything after it through `git` on
//! `PATH` -- a full, fresh clone into `dest` on every call, no shared
//! bare-clone cache (a cache never helps the one-fork-per-student case,
//! and a full clone is what the deadline-based commit search needs
//! anyway).

pub mod forks;
pub mod github_events;
pub mod overrides;
pub mod source;

use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use jiff::{Timestamp, Zoned};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::json::{read_json, write_json};
use crate::id::{CommitSha, GithubUser};
use crate::render::{bar, spinner};
use crate::submissions::forks::{Fork, Upstream};
use crate::submissions::github_events::PushEvent;
use crate::submissions::overrides::Override;
use crate::submissions::source::{CsvRoster, RosterEntry};

const GIT_BIN: &str = "git";
const GH_BIN: &str = "gh";

/// Tag a student pushes to bless a commit.
const BLESS_TAG: &str = "listoco";

/// `push_event` is server-verified and unforgeable; `commit_date` is the
/// commit's own, backdatable author/committer date.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitTimestamp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_event: Option<Timestamp>,
    pub commit_date: Timestamp,
}

/// A commit and how its date was established.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub sha: CommitSha,
    pub timestamp: CommitTimestamp,
}

/// How a submission's date was determined, and how much to trust it. Also
/// what names the graded commit -- there's no separate `graded_commit`
/// field anywhere, since the commit that was checked out is exactly the
/// one whose date this describes ([`SubmissionDate::graded`]).
///
/// Lateness isn't a variant's business to carry: the deadline this was
/// resolved against is on the [`FetchRecord`], so how late a `Late`
/// submission is stays derivable rather than baked in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubmissionDate {
    /// A bless tag. Never deadline-gated.
    Blessed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_push_event: Option<Timestamp>,
        commit: Commit,
        /// The latest on-time commit, kept so grading can still fall back
        /// to it later. The one field here naming a commit *other* than
        /// the graded one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback: Option<Commit>,
    },
    /// The latest commit at or before the deadline.
    OnTime(Commit),
    /// Nothing at or before the deadline -- this is the latest commit
    /// there is. Fetched and checked out anyway: what to do about lateness
    /// is grading's call, not fetch's.
    Late(Commit),
    /// An instructor named this commit (see [`overrides`]). Never
    /// deadline-gated, but `late_by` still records how late it was, so
    /// accepting a submission and pretending it was on time stay separate
    /// decisions.
    Override {
        commit: Commit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        overridden_at: Timestamp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        late_by: Option<jiff::SignedDuration>,
    },
    /// The fork has no commits at all.
    Empty,
}

impl SubmissionDate {
    /// The commit that was checked out, or `None` for a fork with no
    /// commits to check out.
    pub fn graded(&self) -> Option<&Commit> {
        match self {
            SubmissionDate::Blessed { commit, .. } => Some(commit),
            SubmissionDate::Override { commit, .. } => Some(commit),
            SubmissionDate::OnTime(commit) | SubmissionDate::Late(commit) => Some(commit),
            SubmissionDate::Empty => None,
        }
    }

    /// `None` unless there's a GitHub-verified timestamp -- except for
    /// `Blessed`, where `commit_date` alone is fine since blessing already
    /// bypasses the deadline gate.
    pub fn trusted_submitted_at(&self) -> Option<Timestamp> {
        match self {
            SubmissionDate::Blessed {
                tag_push_event: Some(t),
                ..
            } => Some(*t),
            SubmissionDate::Blessed { commit, .. } => Some(commit.timestamp.commit_date),
            // An instructor's decision carries no student-verified time:
            // whatever the commit's own date says, nobody pushed anything
            // to make this the graded submission.
            SubmissionDate::Override { .. } => None,
            SubmissionDate::OnTime(commit) | SubmissionDate::Late(commit) => {
                commit.timestamp.push_event
            }
            SubmissionDate::Empty => None,
        }
    }
}

/// Outcome of fetching one fork. `Failed` is reserved for the machinery
/// breaking -- a clone, a branch lookup or a push-history read. Anything
/// about the *submission itself* (late, empty) is a successful fetch of a
/// poor submission: it's recorded and left for evaluate/grade to judge,
/// so problems get fixed downstream rather than hidden here.
#[derive(Debug, Clone)]
pub enum FetchOutcome {
    Ok {
        workspace: PathBuf,
        submission_date: SubmissionDate,
    },
    Failed {
        message: String,
    },
}

impl FetchOutcome {
    fn failed(message: impl Into<String>) -> Self {
        FetchOutcome::Failed {
            message: message.into(),
        }
    }
}

/// Clones `fork` into `dest` and checks out the commit the deadline (or a
/// bless tag) selects. Every failure degrades to `FetchOutcome::Failed`
/// rather than a hard `Err`, so one bad repo doesn't abort the batch.
///
/// `detach` removes the checkout's `.git` once it's on the right commit,
/// leaving a plain directory -- for landing submissions inside another git
/// repo, where they'd otherwise be nested repos it can't track.
pub fn fetch_fork(
    fork: &Fork,
    dest: &Path,
    deadline: &Zoned,
    detach: bool,
    r#override: Option<&Override>,
) -> Result<FetchOutcome> {
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let nwo = fork.nwo();
    if let Err(e) = clone(&nwo, dest) {
        return Ok(FetchOutcome::failed(format!("failed to clone {nwo}: {e}")));
    }

    let branch = match run_git(GIT_BIN, &default_branch_argv(dest)) {
        Ok(b) if !b.is_empty() => b,
        _ => {
            return Ok(FetchOutcome::failed(format!(
                "could not determine the default branch for {nwo}"
            )));
        }
    };

    // A `gh` failure fails the fetch outright, rather than silently
    // falling back to the forgeable `git log` date for every submission.
    let events = match github_events::list_push_events(&fork.owner, &fork.name) {
        Ok(events) => events,
        Err(e) => {
            return Ok(FetchOutcome::failed(format!(
                "failed to read GitHub push history for {nwo}: {e}"
            )));
        }
    };

    let submission_date = match resolve_commit(dest, &branch, deadline, &events, r#override) {
        Ok(date) => date,
        Err(e) => return Ok(FetchOutcome::failed(e)),
    };

    if let Some(commit) = submission_date.graded() {
        let sha = commit.sha;
        if let Err(e) = run_git(GIT_BIN, &checkout_argv(dest, sha.as_str())) {
            return Ok(FetchOutcome::failed(format!(
                "failed to check out {sha} for {nwo}: {e}"
            )));
        }
    }

    // Last, and only here: the branch lookup, the deadline commit search
    // and the bless tag above all need the history this throws away.
    if detach {
        let git_dir = dest.join(".git");
        if let Err(e) = fs::remove_dir_all(&git_dir) {
            return Ok(FetchOutcome::failed(format!(
                "failed to detach {}: {e}",
                git_dir.display()
            )));
        }
    }

    Ok(FetchOutcome::Ok {
        workspace: dest.to_path_buf(),
        submission_date,
    })
}

/// Picks the commit to grade: an instructor's override if there is one,
/// then the bless tag (deadline exempt), otherwise the latest commit at or
/// before `deadline`, otherwise the latest commit there is, marked late.
fn resolve_commit(
    dest: &Path,
    branch: &str,
    deadline: &Zoned,
    events: &[PushEvent],
    r#override: Option<&Override>,
) -> std::result::Result<SubmissionDate, String> {
    // Ahead of the bless tag: an override exists precisely because the tag
    // (or the deadline) picked the wrong commit.
    if let Some(r#override) = r#override {
        return resolve_override(dest, deadline, events, r#override);
    }

    let fallback = resolve_fallback_commit(dest, branch, deadline, events)?;

    let tag_ref = format!("refs/tags/{BLESS_TAG}");
    let tag_sha = run_git(
        GIT_BIN,
        &rev_parse_argv(dest, &format!("{tag_ref}^{{commit}}")),
    )
    .ok();

    if let Some(sha) = tag_sha.filter(|sha| !sha.is_empty()) {
        let tag_push_event = github_events::latest(events, &tag_ref, None).map(|e| e.created_at);
        let commit_date = commit_date(dest, &sha)?;
        return Ok(SubmissionDate::Blessed {
            tag_push_event,
            commit: Commit {
                sha: CommitSha::new(sha),
                timestamp: CommitTimestamp {
                    push_event: tag_push_event,
                    commit_date,
                },
            },
            fallback,
        });
    }

    // Nothing on time isn't a failed fetch: take the latest commit there
    // is and say it's late, so the work is on disk to look at and grading
    // gets to decide what that's worth.
    match fallback {
        Some(commit) => Ok(SubmissionDate::OnTime(commit)),
        None => match latest_commit(dest, branch, None)? {
            Some(commit) => Ok(SubmissionDate::Late(commit)),
            None => Ok(SubmissionDate::Empty),
        },
    }
}

/// Resolves an instructor's override against the student's own clone.
/// Anything `git rev-parse` can't resolve is the instructor's typo (or a
/// commit that isn't in this fork), and fails the fetch for this student
/// rather than silently falling back to the deadline's pick -- an
/// exception that quietly didn't apply is worse than one that didn't run.
fn resolve_override(
    dest: &Path,
    deadline: &Zoned,
    events: &[PushEvent],
    r#override: &Override,
) -> std::result::Result<SubmissionDate, String> {
    let sha = run_git(
        GIT_BIN,
        &rev_parse_argv(dest, &format!("{}^{{commit}}", r#override.commit)),
    )
    .map_err(|e| {
        format!(
            "override names {:?}, which is not a commit in this fork: {e}",
            r#override.commit
        )
    })?;
    if sha.is_empty() {
        return Err(format!(
            "override names {:?}, which is not a commit in this fork",
            r#override.commit
        ));
    }

    let commit_date = commit_date(dest, &sha)?;
    // Prefer a verified push of this exact commit where there is one --
    // lateness is worth measuring from a time the student couldn't set.
    let push_event = events
        .iter()
        .filter(|e| e.head == sha)
        .map(|e| e.created_at)
        .min();
    let submitted = push_event.unwrap_or(commit_date);
    let late_by =
        (submitted > deadline.timestamp()).then(|| submitted.duration_since(deadline.timestamp()));

    Ok(SubmissionDate::Override {
        commit: Commit {
            sha: CommitSha::new(sha),
            timestamp: CommitTimestamp {
                push_event,
                commit_date,
            },
        },
        reason: r#override.reason.clone(),
        overridden_at: r#override.recorded_at,
        late_by,
    })
}

/// The latest commit on `branch` at or before `deadline`
fn resolve_fallback_commit(
    dest: &Path,
    branch: &str,
    deadline: &Zoned,
    events: &[PushEvent],
) -> std::result::Result<Option<Commit>, String> {
    let branch_ref = format!("refs/heads/{branch}");
    if let Some(event) = github_events::latest(events, &branch_ref, Some(deadline.timestamp())) {
        let commit_date = commit_date(dest, &event.head)?;
        return Ok(Some(Commit {
            sha: CommitSha::new(event.head.clone()),
            timestamp: CommitTimestamp {
                push_event: Some(event.created_at),
                commit_date,
            },
        }));
    }

    latest_commit(dest, branch, Some(deadline))
}

/// The latest commit on `branch`, optionally capped at `before`. `None`
/// means there isn't one: either the cap excluded everything, or the fork
/// has no commits at all.
fn latest_commit(
    dest: &Path,
    branch: &str,
    before: Option<&Zoned>,
) -> std::result::Result<Option<Commit>, String> {
    let sha = run_git(GIT_BIN, &last_commit_argv(dest, branch, before)).unwrap_or_default();
    if sha.is_empty() {
        return Ok(None);
    }
    let commit_date = commit_date(dest, &sha)?;
    Ok(Some(Commit {
        sha: CommitSha::new(sha),
        timestamp: CommitTimestamp {
            push_event: None,
            commit_date,
        },
    }))
}

fn commit_date(dest: &Path, sha: &str) -> std::result::Result<Timestamp, String> {
    let raw = run_git(GIT_BIN, &commit_date_argv(dest, sha)).map_err(|e| e.to_string())?;
    raw.parse()
        .map_err(|e| format!("failed to parse commit date {raw:?} for {sha}: {e}"))
}

/// Durable record of the last fetch attempt for one student, written by
/// `fetch_batch` to `<out>/.fetch/<github_user>.json` (kept out of
/// `<out>/<github_user>/`, the submission's own flat checkout dir, so nothing
/// downstream ever needs to filter it out). Nothing in this codebase reads
/// it back except [`read_fetch_record`] itself -- see this module's doc
/// comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRecord {
    pub fetched_at: Timestamp,
    /// The deadline this fetch resolved against -- the spec's, or whatever
    /// `--as-of` overrode it with. Recorded so everything about timing
    /// stays derivable from the record alone: how late a `Late` submission
    /// is, and which deadline a re-fetch applied.
    pub deadline: Zoned,
    /// Every fork matching this student, in GitHub's order. `[0]` is the
    /// one that was fetched; a longer list is an ambiguity that was
    /// resolved arbitrarily and is recorded here to be audited. Empty
    /// means no fork was found.
    #[serde(default)]
    pub forks: Vec<Fork>,
    /// The roster row's remaining columns, verbatim, in header order.
    #[serde(default)]
    pub metadata: IndexMap<String, String>,
    pub result: FetchResult,
}

/// The fetch's terminal state, with the payload attached to the variant
/// that has one -- see [`FetchOutcome`] for what does and doesn't count as
/// a failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FetchResult {
    Ok { submission_date: SubmissionDate },
    Failed { message: String },
}

impl FetchRecord {
    /// The commit that was checked out: `None` if the fetch failed or the
    /// fork had no commits.
    pub fn graded_commit(&self) -> Option<&Commit> {
        self.submission_date()?.graded()
    }

    pub fn submission_date(&self) -> Option<&SubmissionDate> {
        match &self.result {
            FetchResult::Ok { submission_date } => Some(submission_date),
            FetchResult::Failed { .. } => None,
        }
    }

    /// How late the graded commit is, or `None` if it isn't. Measured
    /// against the verified push time where there is one, since the
    /// commit's own date is the student's to set.
    pub fn late_by(&self) -> Option<jiff::SignedDuration> {
        let commit = match self.submission_date()? {
            SubmissionDate::Late(commit) => commit,
            // Measured when the override was resolved, against the
            // deadline in force then -- not re-derived here.
            SubmissionDate::Override { late_by, .. } => return *late_by,
            _ => return None,
        };
        let submitted = commit
            .timestamp
            .push_event
            .unwrap_or(commit.timestamp.commit_date);
        Some(submitted.duration_since(self.deadline.timestamp()))
    }
}

pub(crate) fn fetch_record_path(out_dir: &Path, github_user: &GithubUser) -> PathBuf {
    out_dir.join(".fetch").join(format!("{github_user}.json"))
}

fn write_fetch_record(
    out_dir: &Path,
    github_user: &GithubUser,
    record: &FetchRecord,
) -> Result<()> {
    write_json(&fetch_record_path(out_dir, github_user), record)
}

/// `None` if `fetch_batch` has never run for this student.
pub fn read_fetch_record(out_dir: &Path, github_user: &GithubUser) -> Result<Option<FetchRecord>> {
    let path = fetch_record_path(out_dir, github_user);
    if !path.is_file() {
        return Ok(None);
    }
    read_json(&path)
}

/// What one roster row's fetch will do. Decided before anything is
/// cloned, so `autograder fetch` can show it and ask.
#[derive(Debug, Clone)]
pub enum Plan {
    /// `fork` is the one to clone; `also` is the rest of the candidates,
    /// kept for the record so an arbitrary pick stays auditable.
    Fetch { fork: Fork, also: Vec<Fork> },
    /// Every fork this student can push to is shared with another student,
    /// so none of them can be attributed -- see [`forks::SharedFork`].
    Shared { forks: Vec<Fork>, message: String },
    /// No fork at all.
    Missing { message: String },
}

#[derive(Debug, Clone)]
pub struct PlanRow {
    pub entry: RosterEntry,
    pub plan: Plan,
}

/// Everything `fetch_batch` is about to do, in roster order.
#[derive(Debug)]
pub struct FetchPlan {
    pub rows: Vec<PlanRow>,
    /// Forks no roster student can push to -- outsiders forking a public
    /// assignment repo is normal and not the instructor's problem.
    pub unmatched: Vec<Fork>,
}

impl FetchPlan {
    pub fn to_fetch(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row.plan, Plan::Fetch { .. }))
            .count()
    }
}

/// Works out what to fetch without fetching it: lists `upstream`'s forks
/// and attributes each to a roster student by its collaborators. Read-only
/// -- nothing on disk is touched, so a plan can be shown and declined for
/// free.
pub fn plan_fetch(upstream: &Upstream, roster: &CsvRoster, jobs: usize) -> Result<FetchPlan> {
    let entries = roster.roster()?;

    // Rejected here rather than tolerated: two rows sharing a github_user
    // write the same fetch record, and which one wins is decided by
    // whichever thread finishes last.
    if let Some(duplicate) = first_duplicate(&entries) {
        return Err(Error::InvalidSpec(format!(
            "roster lists {duplicate} more than once"
        )));
    }

    let progress = spinner();
    progress.set_message(format!("listing forks of {upstream}..."));
    let forks = forks::list_forks(upstream)?;
    progress.finish_and_clear();

    // Discovery is one API call for the fork list plus one per fork, and
    // the per-fork half is the slow part -- `jobs` at a time, or a
    // class-sized roster spends minutes here before the plan is even shown.
    let progress = bar(forks.len() as u64);
    let pool = crate::github::pool(jobs, "lookup")?;
    let mut matches = pool.install(|| {
        forks::match_forks(entries.iter().map(|e| e.github_user), forks, |fork| {
            let logins = forks::list_collaborators(fork);
            progress.inc(1);
            logins
        })
    })?;
    progress.finish_and_clear();

    let rows = entries
        .into_iter()
        .map(|entry| {
            let mut candidates = matches
                .by_student
                .remove(&entry.github_user)
                .unwrap_or_default();
            let shared = matches.shared_for(&entry.github_user);

            let plan = if candidates.is_empty() {
                if shared.is_empty() {
                    Plan::Missing {
                        message: format!(
                            "no fork of {upstream} that {} can push to",
                            entry.github_user
                        ),
                    }
                } else {
                    Plan::Shared {
                        message: shared_message(&shared, &entry.github_user),
                        forks: shared.iter().map(|s| s.fork.clone()).collect(),
                    }
                }
            } else {
                Plan::Fetch {
                    fork: candidates.remove(0),
                    also: candidates,
                }
            };
            PlanRow { entry, plan }
        })
        .collect();

    Ok(FetchPlan {
        rows,
        unmatched: matches.unmatched,
    })
}

fn first_duplicate(entries: &[RosterEntry]) -> Option<GithubUser> {
    let mut seen = std::collections::BTreeSet::new();
    entries
        .iter()
        .find(|entry| !seen.insert(entry.github_user))
        .map(|entry| entry.github_user)
}

/// Runs the Fetch stage alone: lands each submission `plan` resolved at
/// `out_dir/<github_user>/` (flat -- no `checkout/` nesting) and records the
/// outcome at `out_dir/.fetch/<github_user>.json`. Safe to run again --
/// always overwrites both.
///
/// A roster row with no usable fork still gets a `Failed` record rather
/// than being skipped, so it shows up downstream instead of vanishing.
///
/// Rows are fetched `jobs` at a time. Every row is attempted even if some
/// fail, and the returned records stay in roster order no matter what
/// order they finished in; `on_result` is called once per row as it lands,
/// in completion order.
pub fn fetch_batch(
    plan: &FetchPlan,
    out_dir: &Path,
    deadline: &Zoned,
    detach: bool,
    jobs: usize,
    on_result: &(dyn Fn(&GithubUser, &FetchRecord) + Sync),
) -> Result<Vec<(GithubUser, FetchRecord)>> {
    // Only the clones get a progress unit. The rows with no fork to clone
    // finish the instant they're claimed, and counting those would make
    // the early ETA nonsense.
    let progress = bar(plan.to_fetch() as u64);

    // Read once for the batch: a re-fetch has to reapply every exception
    // already granted, or it silently reverts them.
    let overrides = overrides::load(out_dir)?;

    let pool = crate::github::pool(jobs, "fetch")?;

    // `Vec<Result<_>>` and not `Result<Vec<_>>`: collecting into the
    // latter short-circuits, and one repo breaking shouldn't cost every
    // student behind it in the queue their fetch. `collect` restores
    // roster order regardless of what finished when.
    let results: Vec<Result<(GithubUser, FetchRecord)>> = pool.install(|| {
        plan.rows
            .par_iter()
            .map(|row| {
                let fetched = fetch_row(row, out_dir, deadline, detach, &overrides);
                if matches!(row.plan, Plan::Fetch { .. }) {
                    progress.inc(1);
                }
                let (github_user, record) = fetched?;
                // Through `suspend`, so the line is printed above the bar
                // rather than scribbled over by the next redraw.
                progress.suspend(|| on_result(&github_user, &record));
                Ok((github_user, record))
            })
            .collect()
    });
    progress.finish_and_clear();

    let mut records = Vec::with_capacity(results.len());
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(record) => records.push(record),
            Err(e) => errors.push(e),
        }
    }
    // Everything fetchable is already on disk by now, so the only thing
    // left is to say what broke. The first failure is the return value
    // and the rest would otherwise vanish with it.
    if !errors.is_empty() {
        for e in &errors[1..] {
            tracing::error!(error = %e, "fetch failed");
        }
        return Err(errors.remove(0));
    }
    Ok(records)
}

/// One roster row: its fetch (or the recorded reason there wasn't one)
/// plus its record on disk. An `Err` is the machinery breaking -- a bad
/// submission comes back `Ok` with a `Failed` record, see [`FetchOutcome`].
fn fetch_row(
    row: &PlanRow,
    out_dir: &Path,
    deadline: &Zoned,
    detach: bool,
    overrides: &[Override],
) -> Result<(GithubUser, FetchRecord)> {
    let github_user = row.entry.github_user;
    let r#override = overrides::find(overrides, &github_user);
    let (outcome, forks) = match &row.plan {
        Plan::Fetch { fork, also } => {
            let outcome = fetch_fork(
                fork,
                &out_dir.join(github_user.as_str()),
                deadline,
                detach,
                r#override.as_ref(),
            )?;
            let mut forks = vec![fork.clone()];
            forks.extend(also.iter().cloned());
            (outcome, forks)
        }
        // A shared fork is fetched for nobody, but still goes in the
        // record, so the failure names what was found rather than
        // reading like a missing submission.
        Plan::Shared { forks, message } => (FetchOutcome::failed(message), forks.clone()),
        Plan::Missing { message } => (FetchOutcome::failed(message), Vec::new()),
    };

    let result = match outcome {
        FetchOutcome::Ok {
            submission_date, ..
        } => FetchResult::Ok { submission_date },
        FetchOutcome::Failed { message } => FetchResult::Failed { message },
    };

    let record = FetchRecord {
        result,
        fetched_at: Timestamp::now(),
        deadline: deadline.clone(),
        forks,
        metadata: row.entry.metadata.clone(),
    };
    write_fetch_record(out_dir, &github_user, &record)?;
    Ok((github_user, record))
}

fn shared_message(shared: &[&forks::SharedFork], student: &GithubUser) -> String {
    let described: Vec<String> = shared
        .iter()
        .map(|s| {
            let others: Vec<&str> = s
                .students
                .iter()
                .filter(|id| *id != student)
                .map(|id| id.as_str())
                .collect();
            format!("{} (shared with {})", s.fork.nwo(), others.join(", "))
        })
        .collect();
    format!(
        "fetched nothing: the only fork {student} can push to is shared -- {}",
        described.join("; ")
    )
}

/// `gh repo clone` rather than `git clone`, so a private fork works off
/// the instructor's existing `gh` login with no token in this process's
/// argv. A full clone, not shallow: [`last_commit_before_argv`] needs the
/// real commit history to search.
fn clone(nwo: &str, dest: &Path) -> Result<()> {
    let output = Command::new(GH_BIN)
        .args(["repo", "clone", nwo, &dest.display().to_string()])
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// The fork's own default branch, whatever the student set it to.
fn default_branch_argv(dest: &Path) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "symbolic-ref".to_string(),
        "--short".to_string(),
        "HEAD".to_string(),
    ]
}

/// The last commit on `branch`, optionally capped at `before` for
/// deadline selection. Empty stdout means there's no such commit.
fn last_commit_argv(dest: &Path, branch: &str, before: Option<&Zoned>) -> Vec<String> {
    let mut argv = vec![
        "-C".to_string(),
        dest.display().to_string(),
        "log".to_string(),
    ];
    if let Some(before) = before {
        argv.push(format!("--before={}", before.timestamp()));
    }
    argv.extend([
        "-1".to_string(),
        "--format=%H".to_string(),
        branch.to_string(),
    ]);
    argv
}

/// Resolves a branch/tag/sha to a full commit SHA.
fn rev_parse_argv(dest: &Path, r#ref: &str) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "rev-parse".to_string(),
        r#ref.to_string(),
    ]
}

fn checkout_argv(dest: &Path, sha: &str) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "checkout".to_string(),
        sha.to_string(),
    ]
}

/// `%cI` so it parses straight into a `Timestamp`.
fn commit_date_argv(dest: &Path, sha: &str) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "log".to_string(),
        "-1".to_string(),
        "--format=%cI".to_string(),
        sha.to_string(),
    ]
}

/// Runs `<git_bin> <argv>`, returning trimmed stdout on success. `git_bin`
/// stays a parameter (not always `GIT_BIN`) so tests can exercise the
/// failure path with a nonexistent binary, no network needed.
fn run_git(git_bin: &str, argv: &[String]) -> Result<String> {
    let output = Command::new(git_bin)
        .args(argv)
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{git_bin}`: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Everything about the Fetch stage's public workflow
/// (`fetch_fork`/`fetch_batch`/`read_fetch_record`) lives in
/// `tests/fetch.rs` as an integration test instead (see that file's doc
/// comment). These stay here because the `git`-argv builders, `run_git`
/// and the commit-selection functions are private -- there's no way to
/// reach them from outside the crate.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_branch_argv_reads_symbolic_head() {
        let argv = default_branch_argv(Path::new("/tmp/dest"));
        assert_eq!(
            argv,
            vec!["-C", "/tmp/dest", "symbolic-ref", "--short", "HEAD"]
        );
    }

    #[test]
    fn last_commit_argv_caps_at_the_deadline_when_one_is_given() {
        let deadline: Zoned = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
            .parse()
            .unwrap();
        let argv = last_commit_argv(Path::new("/tmp/dest"), "main", Some(&deadline));
        assert_eq!(
            argv,
            vec![
                "-C",
                "/tmp/dest",
                "log",
                "--before=2026-02-15T07:59:59Z",
                "-1",
                "--format=%H",
                "main",
            ]
        );
    }

    #[test]
    fn last_commit_argv_without_a_deadline_takes_the_branch_tip() {
        let argv = last_commit_argv(Path::new("/tmp/dest"), "main", None);
        assert_eq!(
            argv,
            vec!["-C", "/tmp/dest", "log", "-1", "--format=%H", "main"]
        );
    }

    #[test]
    fn rev_parse_argv_resolves_the_given_ref() {
        let argv = rev_parse_argv(Path::new("/tmp/dest"), "v1.0");
        assert_eq!(argv, vec!["-C", "/tmp/dest", "rev-parse", "v1.0"]);
    }

    #[test]
    fn checkout_argv_targets_the_given_sha() {
        let argv = checkout_argv(Path::new("/tmp/dest"), "abc123");
        assert_eq!(argv, vec!["-C", "/tmp/dest", "checkout", "abc123"]);
    }

    #[test]
    fn run_git_surfaces_a_missing_binary_clearly_instead_of_panicking() {
        let err = run_git("autograder-git-does-not-exist", &["--version".to_string()]).unwrap_err();
        assert!(err.to_string().contains("failed to run"));
    }

    // `fetch_fork` itself needs real network -- [deferred]. The functions
    // below don't (push events are passed in, not fetched), so they're
    // tested directly.

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        dir
    }

    /// Commits one file, backdated via `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE`.
    fn commit(dir: &Path, filename: &str, date: &str) -> String {
        std::fs::write(dir.join(filename), "x").unwrap();
        git(dir, &["add", filename]);
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-q", "-m", "msg"])
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .unwrap();
        assert!(output.status.success());
        git(dir, &["rev-parse", "HEAD"])
    }

    fn deadline(s: &str) -> Zoned {
        format!("{s}[UTC]").parse().unwrap()
    }

    #[test]
    fn resolve_fallback_commit_falls_back_to_git_log_when_no_push_events() {
        let repo = init_repo();
        let sha = commit(repo.path(), "a.txt", "2026-02-10T00:00:00Z");

        let fallback =
            resolve_fallback_commit(repo.path(), "main", &deadline("2026-02-14T00:00:00Z"), &[])
                .unwrap()
                .unwrap();

        assert_eq!(fallback.sha, sha.as_str());
        assert!(fallback.timestamp.push_event.is_none());
        assert_eq!(
            fallback.timestamp.commit_date,
            "2026-02-10T00:00:00Z".parse().unwrap()
        );
    }

    #[test]
    fn resolve_fallback_commit_prefers_a_verified_push_event_over_commit_date() {
        let repo = init_repo();
        // Backdated to look on-time -- the verified event should still win.
        let sha = commit(repo.path(), "a.txt", "2026-01-01T00:00:00Z");
        let events = vec![PushEvent {
            created_at: "2026-02-12T00:00:00Z".parse().unwrap(),
            r#ref: "refs/heads/main".to_string(),
            head: sha.clone(),
        }];

        let fallback = resolve_fallback_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &events,
        )
        .unwrap()
        .unwrap();

        assert_eq!(fallback.sha, sha.as_str());
        assert_eq!(
            fallback.timestamp.push_event,
            Some("2026-02-12T00:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn resolve_fallback_commit_is_none_when_nothing_predates_the_deadline() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "2026-02-20T00:00:00Z");

        let fallback =
            resolve_fallback_commit(repo.path(), "main", &deadline("2026-02-14T00:00:00Z"), &[])
                .unwrap();

        assert!(fallback.is_none());
    }

    #[test]
    fn resolve_commit_blessed_tag_bypasses_the_deadline_entirely() {
        let repo = init_repo();
        let sha = commit(repo.path(), "a.txt", "2026-03-01T00:00:00Z");
        git(repo.path(), &["tag", BLESS_TAG]);

        // Deadline is well before the (blessed) commit -- unblessed, this
        // would resolve as `Late`.
        let date = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-01-01T00:00:00Z"),
            &[],
            None,
        )
        .unwrap();

        assert!(matches!(date, SubmissionDate::Blessed { .. }));
        assert_eq!(date.graded().unwrap().sha, sha.as_str());
    }

    #[test]
    fn resolve_commit_on_time_respects_the_deadline() {
        let repo = init_repo();
        let on_time = commit(repo.path(), "a.txt", "2026-02-10T00:00:00Z");
        commit(repo.path(), "b.txt", "2026-02-20T00:00:00Z");

        let date = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &[],
            None,
        )
        .unwrap();

        assert!(matches!(date, SubmissionDate::OnTime(_)));
        assert_eq!(date.graded().unwrap().sha, on_time.as_str());
    }

    /// Fetch records lateness rather than refusing: the work lands on disk
    /// and grading decides what it's worth.
    #[test]
    fn resolve_commit_takes_the_latest_commit_when_nothing_is_on_time() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "2026-02-20T00:00:00Z");
        let latest = commit(repo.path(), "b.txt", "2026-02-21T00:00:00Z");

        let date = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &[],
            None,
        )
        .unwrap();

        assert!(matches!(date, SubmissionDate::Late(_)));
        assert_eq!(date.graded().unwrap().sha, latest.as_str());
    }

    #[test]
    fn resolve_commit_reports_a_fork_with_no_commits_as_empty() {
        let repo = init_repo();

        let date = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &[],
            None,
        )
        .unwrap();

        assert!(matches!(date, SubmissionDate::Empty));
        assert!(date.graded().is_none());
    }

    fn override_for(commit: &str) -> Override {
        Override {
            github_user: GithubUser::new("alice"),
            commit: commit.to_string(),
            reason: Some("tagged the wrong commit".to_string()),
            recorded_at: "2026-02-16T00:00:00Z".parse().unwrap(),
        }
    }

    /// The case the whole feature exists for: the student blessed one
    /// commit and meant another.
    #[test]
    fn an_override_wins_over_a_bless_tag() {
        let repo = init_repo();
        let intended = commit(repo.path(), "a.txt", "2026-02-10T00:00:00Z");
        commit(repo.path(), "b.txt", "2026-02-11T00:00:00Z");
        git(repo.path(), &["tag", BLESS_TAG]);

        let date = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &[],
            Some(&override_for(&intended)),
        )
        .unwrap();

        assert_eq!(date.graded().unwrap().sha, intended.as_str());
        let SubmissionDate::Override {
            reason, late_by, ..
        } = &date
        else {
            panic!("expected an override, got {date:?}");
        };
        assert_eq!(reason.as_deref(), Some("tagged the wrong commit"));
        assert!(late_by.is_none(), "the named commit predates the deadline");
    }

    /// Accepting a late commit and calling it on time are separate
    /// decisions: the override grades it, and still records how late it is.
    #[test]
    fn an_override_records_lateness_without_enforcing_it() {
        let repo = init_repo();
        let late = commit(repo.path(), "a.txt", "2026-02-20T00:00:00Z");

        let date = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &[],
            Some(&override_for(&late)),
        )
        .unwrap();

        assert_eq!(date.graded().unwrap().sha, late.as_str());
        let SubmissionDate::Override { late_by, .. } = &date else {
            panic!("expected an override, got {date:?}");
        };
        assert_eq!(late_by.unwrap().as_hours(), 6 * 24);
    }

    /// A typo'd sha fails this student's fetch rather than quietly falling
    /// back to the deadline's pick -- an exception that silently didn't
    /// apply is worse than one that didn't run.
    #[test]
    fn an_override_naming_an_unknown_commit_fails_the_fetch() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "2026-02-10T00:00:00Z");

        let err = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &[],
            Some(&override_for("nosuchcommit")),
        )
        .unwrap_err();

        assert!(err.contains("nosuchcommit"), "got {err:?}");
    }

    /// An override accepts any ref `git rev-parse` resolves, not just a
    /// full sha -- what the instructor has to hand is usually a short sha.
    #[test]
    fn an_override_resolves_a_short_sha_to_the_full_commit() {
        let repo = init_repo();
        let full = commit(repo.path(), "a.txt", "2026-02-10T00:00:00Z");

        let date = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &[],
            Some(&override_for(&full[..8])),
        )
        .unwrap();

        assert_eq!(date.graded().unwrap().sha, full.as_str());
    }

    #[test]
    fn blessing_keeps_the_on_time_commit_as_the_fallback() {
        let repo = init_repo();
        let on_time = commit(repo.path(), "a.txt", "2026-02-10T00:00:00Z");
        let late = commit(repo.path(), "b.txt", "2026-02-20T00:00:00Z");
        git(repo.path(), &["tag", BLESS_TAG]);

        let date = resolve_commit(
            repo.path(),
            "main",
            &deadline("2026-02-14T00:00:00Z"),
            &[],
            None,
        )
        .unwrap();

        assert_eq!(date.graded().unwrap().sha, late.as_str());
        let SubmissionDate::Blessed { fallback, .. } = &date else {
            panic!("expected a blessed submission, got {date:?}");
        };
        assert_eq!(fallback.as_ref().unwrap().sha, on_time.as_str());
    }

    #[test]
    fn late_by_measures_against_the_deadline_on_the_record() {
        let record = FetchRecord {
            fetched_at: Timestamp::now(),
            deadline: deadline("2026-02-14T00:00:00Z"),
            forks: Vec::new(),
            metadata: IndexMap::new(),
            result: FetchResult::Ok {
                submission_date: SubmissionDate::Late(Commit {
                    sha: CommitSha::new("abc123"),
                    timestamp: CommitTimestamp {
                        push_event: Some("2026-02-14T03:00:00Z".parse().unwrap()),
                        // Backdated to look on time -- the verified push
                        // time is what lateness is measured from.
                        commit_date: "2026-02-10T00:00:00Z".parse().unwrap(),
                    },
                }),
            },
        };

        assert_eq!(record.late_by().unwrap().as_hours(), 3);
    }

    #[test]
    fn late_by_is_none_for_an_on_time_submission() {
        let record = FetchRecord {
            fetched_at: Timestamp::now(),
            deadline: deadline("2026-02-14T00:00:00Z"),
            forks: Vec::new(),
            metadata: IndexMap::new(),
            result: FetchResult::Ok {
                submission_date: SubmissionDate::OnTime(Commit {
                    sha: CommitSha::new("abc123"),
                    timestamp: CommitTimestamp {
                        push_event: None,
                        commit_date: "2026-02-10T00:00:00Z".parse().unwrap(),
                    },
                }),
            },
        };

        assert!(record.late_by().is_none());
    }
}
