//! The Fetch stage: pulls each student's submission onto disk at
//! `out_dir/<student_id>/` (flat) and a `FetchRecord` at
//! `out_dir/.fetch/<student_id>.json`, independently of grading --
//! `fetch_batch` is what `autograder fetch` runs. Neither `autograder
//! evaluate` nor `autograder grade` touch this module or read a
//! `FetchRecord` back -- a submission directory dropped in by hand works
//! exactly like one `fetch_batch` produced. [`read_fetch_record`] exists
//! for whatever external tooling wants to know what a prior fetch did.
//!
//! `GitRepo::fetch` shells out to `git` on `PATH` -- a full, fresh clone
//! into `dest` on every call, no shared bare-clone cache (a cache keyed by
//! `repo_url` never helps the common one-fork-per-student case, and a full
//! clone is what the deadline-based ref search needs anyway).

pub mod github_events;
pub mod source;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::json::{read_json, write_json};
use crate::id::StudentId;
use crate::submissions::source::CsvRoster;

const GIT_BIN: &str = "git";

/// One roster row: a student and the git remote to fetch their submission
/// from.
#[derive(Debug, Clone)]
pub struct Submission {
    pub student_id: StudentId,
    pub git: GitRepo,
    pub metadata: BTreeMap<String, String>,
}

/// A git remote to fetch a submission from: a clone URL plus an optional
/// pinned ref/branch override -- when unset, `GitRepo::fetch` resolves it
/// via push-time deadline selection instead. Produced by `CsvRoster`.
#[derive(Debug, Clone)]
pub struct GitRepo {
    pub url: String,
    pub r#ref: Option<String>,
}

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

/// The latest on-time commit, captured alongside a `Blessed` submission so
/// grading can still fall back to it later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackCommit {
    pub sha: String,
    pub timestamp: CommitTimestamp,
}

/// How a submission's date was determined, and how much to trust it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubmissionDate {
    /// Never deadline-gated.
    Blessed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag_push_event: Option<Timestamp>,
        commit: CommitTimestamp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback: Option<FallbackCommit>,
    },
    Unblessed(CommitTimestamp),
}

impl SubmissionDate {
    /// `None` unless there's a GitHub-verified timestamp -- except for
    /// `Blessed`, where `commit_date` alone is fine since blessing already
    /// bypasses the deadline gate.
    pub fn trusted_submitted_at(&self) -> Option<Timestamp> {
        match self {
            SubmissionDate::Blessed {
                tag_push_event: Some(t),
                ..
            } => Some(*t),
            SubmissionDate::Blessed { commit, .. } => Some(commit.commit_date),
            SubmissionDate::Unblessed(commit) => commit.push_event,
        }
    }
}

/// Terminal status of the Fetch stage alone -- distinct from
/// `model::BuildStatus`/`model::RunStatus`, which are Evaluate's own
/// build/run vocabulary and have no notion of fetching at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchStatus {
    Ok,
    Failed,
}

/// Outcome of the Fetch stage for one submission.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub status: FetchStatus,
    pub workspace: Option<PathBuf>,
    pub graded_commit: Option<String>,
    pub message: Option<String>,
    pub submission_date: Option<SubmissionDate>,
}

impl FetchOutcome {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            status: FetchStatus::Failed,
            workspace: None,
            graded_commit: None,
            message: Some(message.into()),
            submission_date: None,
        }
    }

    fn ok(workspace: PathBuf, graded_commit: String) -> Self {
        Self {
            status: FetchStatus::Ok,
            workspace: Some(workspace),
            graded_commit: Some(graded_commit),
            message: None,
            submission_date: None,
        }
    }
}

impl GitRepo {
    /// Every failure degrades to `FetchOutcome::failed` rather than a hard `Err`,
    /// so one bad repo doesn't abort the batch.
    pub fn fetch(&self, dest: &Path, deadline: &Zoned) -> Result<FetchOutcome> {
        if dest.exists() {
            fs::remove_dir_all(dest)?;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        if let Err(e) = run_git(GIT_BIN, &clone_argv(&self.url, dest)) {
            return Ok(FetchOutcome::failed(format!(
                "failed to clone {}: {e}",
                self.url
            )));
        }

        let (sha, submission_date) = match &self.r#ref {
            Some(r) => match run_git(GIT_BIN, &rev_parse_argv(dest, r)) {
                Ok(sha) if !sha.is_empty() => (sha, None),
                _ => {
                    return Ok(FetchOutcome::failed(format!(
                        "ref {r:?} not found in {}",
                        self.url
                    )));
                }
            },
            None => {
                let branch = match run_git(GIT_BIN, &default_branch_argv(dest)) {
                    Ok(b) if !b.is_empty() => b,
                    _ => {
                        return Ok(FetchOutcome::failed(format!(
                            "could not determine the default branch for {}",
                            self.url
                        )));
                    }
                };
                match resolve_unpinned(dest, &self.url, &branch, deadline) {
                    Ok((sha, submission_date)) => (sha, Some(submission_date)),
                    Err(e) => return Ok(FetchOutcome::failed(e)),
                }
            }
        };

        if let Err(e) = run_git(GIT_BIN, &checkout_argv(dest, &sha)) {
            return Ok(FetchOutcome::failed(format!(
                "failed to check out {sha} for {}: {e}",
                self.url
            )));
        }

        Ok(FetchOutcome {
            submission_date,
            ..FetchOutcome::ok(dest.to_path_buf(), sha)
        })
    }
}

/// A `gh` failure fails the fetch outright, rather than silently falling
/// back to the forgeable `git log` date for every submission.
fn resolve_unpinned(
    dest: &Path,
    url: &str,
    branch: &str,
    deadline: &Zoned,
) -> std::result::Result<(String, SubmissionDate), String> {
    let events = match github_events::parse_github_url(url) {
        Some((owner, repo)) => github_events::list_push_events(&owner, &repo)
            .map_err(|e| format!("failed to read GitHub push history for {url}: {e}"))?,
        None => Vec::new(),
    };

    let fallback = resolve_fallback_commit(dest, branch, deadline, &events)?;

    let tag_ref = format!("refs/tags/{BLESS_TAG}");
    let tag_sha = run_git(
        GIT_BIN,
        &rev_parse_argv(dest, &format!("{tag_ref}^{{commit}}")),
    )
    .ok();

    if let Some(sha) = tag_sha.filter(|sha| !sha.is_empty()) {
        let tag_push_event = github_events::latest(&events, &tag_ref, None).map(|e| e.created_at);
        let commit_date = commit_date(dest, &sha)?;
        let submission_date = SubmissionDate::Blessed {
            tag_push_event,
            commit: CommitTimestamp {
                push_event: tag_push_event,
                commit_date,
            },
            fallback,
        };
        return Ok((sha, submission_date));
    }

    match fallback {
        Some(FallbackCommit { sha, timestamp }) => Ok((sha, SubmissionDate::Unblessed(timestamp))),
        None => Err(format!(
            "no commit on {branch} at or before the deadline ({deadline}) for {url}"
        )),
    }
}

/// The latest commit on `branch` at or before `deadline`
fn resolve_fallback_commit(
    dest: &Path,
    branch: &str,
    deadline: &Zoned,
    events: &[github_events::PushEvent],
) -> std::result::Result<Option<FallbackCommit>, String> {
    let branch_ref = format!("refs/heads/{branch}");
    if let Some(event) = github_events::latest(events, &branch_ref, Some(deadline.timestamp())) {
        let commit_date = commit_date(dest, &event.head)?;
        return Ok(Some(FallbackCommit {
            sha: event.head.clone(),
            timestamp: CommitTimestamp {
                push_event: Some(event.created_at),
                commit_date,
            },
        }));
    }

    let sha =
        run_git(GIT_BIN, &last_commit_before_argv(dest, branch, deadline)).unwrap_or_default();
    if sha.is_empty() {
        return Ok(None);
    }
    let commit_date = commit_date(dest, &sha)?;
    Ok(Some(FallbackCommit {
        sha,
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

impl Submission {
    pub fn fetch(&self, dest: &Path, deadline: &Zoned) -> Result<FetchOutcome> {
        self.git.fetch(dest, deadline)
    }
}

/// Durable record of the last fetch attempt for one student, written by
/// `fetch_batch` to `<out>/.fetch/<student_id>.json` (kept out of
/// `<out>/<student_id>/`, the submission's own flat checkout dir, so nothing
/// downstream ever needs to filter it out). Nothing in this codebase reads
/// it back except [`read_fetch_record`] itself -- see this module's doc
/// comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRecord {
    pub status: FetchStatus,
    pub graded_commit: Option<String>,
    pub message: Option<String>,
    pub fetched_at: Timestamp,
    pub submission_date: Option<SubmissionDate>,
}

pub(crate) fn fetch_record_path(out_dir: &Path, student_id: &StudentId) -> PathBuf {
    out_dir.join(".fetch").join(format!("{student_id}.json"))
}

fn write_fetch_record(out_dir: &Path, student_id: &StudentId, record: &FetchRecord) -> Result<()> {
    write_json(&fetch_record_path(out_dir, student_id), record)
}

/// `None` if `fetch_batch` has never run for this student.
pub fn read_fetch_record(out_dir: &Path, student_id: &StudentId) -> Result<Option<FetchRecord>> {
    let path = fetch_record_path(out_dir, student_id);
    if !path.is_file() {
        return Ok(None);
    }
    read_json(&path)
}

/// Runs the Fetch stage alone: lands each submission at `out_dir/<student_id>/`
/// (flat -- no `checkout/` nesting) and records the outcome at
/// `out_dir/.fetch/<student_id>.json`. Safe to run again -- always overwrites
/// both.
pub fn fetch_batch(
    source: &CsvRoster,
    out_dir: &Path,
    deadline: &Zoned,
) -> Result<Vec<(StudentId, FetchRecord)>> {
    let submissions = source.submissions()?;
    let mut records = Vec::new();
    for submission in submissions {
        let checkout_dir = out_dir.join(submission.student_id.as_str());
        let outcome = submission.fetch(&checkout_dir, deadline)?;
        let record = FetchRecord {
            status: outcome.status,
            graded_commit: outcome.graded_commit,
            message: outcome.message,
            fetched_at: Timestamp::now(),
            submission_date: outcome.submission_date,
        };
        write_fetch_record(out_dir, &submission.student_id, &record)?;
        records.push((submission.student_id, record));
    }
    Ok(records)
}

/// A full clone, not shallow: [`last_commit_before_argv`] needs the real
/// commit history to search when a roster row doesn't pin a `ref`.
fn clone_argv(repo_url: &str, dest: &Path) -> Vec<String> {
    vec![
        "clone".to_string(),
        repo_url.to_string(),
        dest.display().to_string(),
    ]
}

/// Used when a roster row leaves `ref` unset.
fn default_branch_argv(dest: &Path) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "symbolic-ref".to_string(),
        "--short".to_string(),
        "HEAD".to_string(),
    ]
}

/// Push-time deadline selection: the last commit at or before `deadline` on
/// `branch`. Empty stdout means nothing was pushed before the deadline.
fn last_commit_before_argv(dest: &Path, branch: &str, deadline: &Zoned) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "log".to_string(),
        format!("--before={}", deadline.timestamp()),
        "-1".to_string(),
        "--format=%H".to_string(),
        branch.to_string(),
    ]
}

/// Resolves a pinned branch/tag/sha to a full commit SHA.
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
/// (`GitRepo::fetch`/`fetch_batch`/`read_fetch_record`) lives in
/// `tests/fetch.rs` as an integration test instead (see that file's doc
/// comment). These stay here because the `git`-argv builders and `run_git`
/// are private -- there's no way to reach them from outside the crate.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_argv_is_a_full_clone_to_dest() {
        let argv = clone_argv("https://github.com/org/repo.git", Path::new("/tmp/dest"));
        assert_eq!(
            argv,
            vec!["clone", "https://github.com/org/repo.git", "/tmp/dest"]
        );
    }

    #[test]
    fn default_branch_argv_reads_symbolic_head() {
        let argv = default_branch_argv(Path::new("/tmp/dest"));
        assert_eq!(
            argv,
            vec!["-C", "/tmp/dest", "symbolic-ref", "--short", "HEAD"]
        );
    }

    #[test]
    fn last_commit_before_argv_searches_the_given_branch_up_to_the_deadline() {
        let deadline: Zoned = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
            .parse()
            .unwrap();
        let argv = last_commit_before_argv(Path::new("/tmp/dest"), "main", &deadline);
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

    // `GitRepo::fetch` itself needs real network -- [deferred]. The
    // functions below don't, so they're tested directly.

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

        assert_eq!(fallback.sha, sha);
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
        let events = vec![github_events::PushEvent {
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

        assert_eq!(fallback.sha, sha);
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
    fn resolve_unpinned_blessed_tag_bypasses_the_deadline_entirely() {
        let repo = init_repo();
        let sha = commit(repo.path(), "a.txt", "2026-03-01T00:00:00Z");
        git(repo.path(), &["tag", BLESS_TAG]);

        // Deadline is well before the (blessed) commit -- an `Unblessed`
        // resolution would reject this outright.
        let (resolved_sha, submission_date) = resolve_unpinned(
            repo.path(),
            "local-repo-not-a-github-url",
            "main",
            &deadline("2026-01-01T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(resolved_sha, sha);
        assert!(matches!(submission_date, SubmissionDate::Blessed { .. }));
    }

    #[test]
    fn resolve_unpinned_unblessed_respects_the_deadline() {
        let repo = init_repo();
        let on_time = commit(repo.path(), "a.txt", "2026-02-10T00:00:00Z");
        commit(repo.path(), "b.txt", "2026-02-20T00:00:00Z");

        let (sha, submission_date) = resolve_unpinned(
            repo.path(),
            "local-repo-not-a-github-url",
            "main",
            &deadline("2026-02-14T00:00:00Z"),
        )
        .unwrap();

        assert_eq!(sha, on_time);
        assert!(matches!(submission_date, SubmissionDate::Unblessed(_)));
    }

    #[test]
    fn resolve_unpinned_unblessed_errs_when_nothing_predates_the_deadline() {
        let repo = init_repo();
        commit(repo.path(), "a.txt", "2026-02-20T00:00:00Z");

        let err = resolve_unpinned(
            repo.path(),
            "local-repo-not-a-github-url",
            "main",
            &deadline("2026-02-14T00:00:00Z"),
        )
        .unwrap_err();

        assert!(err.contains("no commit on main"));
    }
}
