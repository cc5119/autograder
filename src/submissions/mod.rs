//! The Fetch stage: pulls each student's submission onto disk at
//! `work_dir/<student_id>/checkout/`, independently of grading --
//! `fetch_batch` is what `autograder fetch` runs directly, and what
//! `autograder grade --fetch` runs before Prepare/Evaluate/Grade. Grading
//! without `--fetch` never touches this module; it just reads the
//! [`FetchRecord`] a prior `fetch_batch` run left behind.
//!
//! `GitRepo::fetch` shells out to `git` on `PATH` -- a full, fresh clone
//! into `dest` on every call, no shared bare-clone cache (a cache keyed by
//! `repo_url` never helps the common one-fork-per-student case, and a full
//! clone is what the deadline-based ref search needs anyway).

pub mod source;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::id::StudentId;
use crate::model::StageStatus;
use crate::store::{read_json, write_json};
use crate::submissions::source::SubmissionsSource;

const GIT_BIN: &str = "git";

/// Generic over the fetchable type `F`, so it's a compile error to hand a
/// `CsvRoster`'s (`GitRepo`-fetching) submissions to code that only knows
/// how to fetch a `LocalPath`.
#[derive(Debug, Clone)]
pub struct Submission<F> {
    pub student_id: StudentId,
    pub fetchable: F,
    pub metadata: BTreeMap<String, String>,
}

/// A `Fetchable` for a path on disk to copy wholesale into the job
/// workspace. Produced by `DirectorySource`.
#[derive(Debug, Clone)]
pub struct LocalPath(pub PathBuf);

/// A `Fetchable` for a git remote: a clone URL plus an optional pinned
/// ref/branch override -- when unset, `Fetchable`'s impl for `GitRepo`
/// below resolves it via push-time deadline selection instead. Produced by
/// `CsvRoster`.
#[derive(Debug, Clone)]
pub struct GitRepo {
    pub url: String,
    pub r#ref: Option<String>,
}

/// Outcome of the Fetch stage for one submission.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub status: StageStatus,
    pub workspace: Option<PathBuf>,
    pub graded_commit: Option<String>,
    pub message: Option<String>,
}

impl FetchOutcome {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            status: StageStatus::FetchFailed,
            workspace: None,
            graded_commit: None,
            message: Some(message.into()),
        }
    }

    fn ok(workspace: PathBuf, graded_commit: String) -> Self {
        Self {
            status: StageStatus::Ok,
            workspace: Some(workspace),
            graded_commit: Some(graded_commit),
            message: None,
        }
    }
}

pub trait Fetchable {
    fn fetch(&self, dest: &Path, deadline: &Zoned) -> Result<FetchOutcome>;
}

impl Fetchable for LocalPath {
    fn fetch(&self, dest: &Path, _deadline: &Zoned) -> Result<FetchOutcome> {
        let src = &self.0;

        if !src.exists() {
            return Ok(FetchOutcome::failed(format!(
                "source directory {} does not exist",
                src.display()
            )));
        }
        if !src.is_dir() {
            return Ok(FetchOutcome::failed(format!(
                "source {} is not a directory",
                src.display()
            )));
        }
        if fs::is_empty_dir(src)? {
            return Ok(FetchOutcome::failed(format!(
                "source directory {} is empty",
                src.display()
            )));
        }

        fs::copy_dir_all(src, dest)?;
        let graded_commit = hash_tree(dest)?;
        Ok(FetchOutcome::ok(dest.to_path_buf(), graded_commit))
    }
}

impl Fetchable for GitRepo {
    /// **[deferred: needs network]** -- clones `self.url` into `dest`, then
    /// checks out `self.ref` if pinned, else the last commit at or before
    /// `deadline` on the default branch. Every failure degrades to
    /// `FetchOutcome::failed` rather than a hard `Err`, so one bad repo
    /// doesn't abort the batch.
    fn fetch(&self, dest: &Path, deadline: &Zoned) -> Result<FetchOutcome> {
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

        let sha = match &self.r#ref {
            Some(r) => match run_git(GIT_BIN, &rev_parse_argv(dest, r)) {
                Ok(sha) if !sha.is_empty() => sha,
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
                match run_git(GIT_BIN, &last_commit_before_argv(dest, &branch, deadline)) {
                    Ok(sha) if !sha.is_empty() => sha,
                    _ => {
                        return Ok(FetchOutcome::failed(format!(
                            "no commit on {branch} at or before the deadline ({deadline}) for {}",
                            self.url
                        )));
                    }
                }
            }
        };

        if let Err(e) = run_git(GIT_BIN, &checkout_argv(dest, &sha)) {
            return Ok(FetchOutcome::failed(format!(
                "failed to check out {sha} for {}: {e}",
                self.url
            )));
        }

        Ok(FetchOutcome::ok(dest.to_path_buf(), sha))
    }
}

impl<F: Fetchable> Submission<F> {
    pub fn fetch(&self, dest: &Path, deadline: &Zoned) -> Result<FetchOutcome> {
        self.fetchable.fetch(dest, deadline)
    }
}

/// Treats each subdirectory of `root` as one student's submission
/// (`student_id` = directory name).
pub struct DirectorySource {
    root: PathBuf,
}

impl DirectorySource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SubmissionsSource<LocalPath> for DirectorySource {
    fn submissions(&self) -> Result<Vec<Submission<LocalPath>>> {
        let mut submissions = Vec::new();
        for entry in fs::read_dir_entries(&self.root)? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let student_id = StudentId::new(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default(),
            );
            submissions.push(Submission {
                student_id,
                fetchable: LocalPath(path),
                metadata: Default::default(),
            });
        }
        submissions.sort_by(|a, b| a.student_id.cmp(&b.student_id));
        Ok(submissions)
    }
}

/// Durable record of the last fetch attempt for one student, written by
/// `fetch_batch` alongside `job_root/checkout/` and read back by
/// `crate::pipeline::grade_batch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRecord {
    pub status: StageStatus,
    pub graded_commit: Option<String>,
    pub message: Option<String>,
    pub fetched_at: Timestamp,
}

fn fetch_record_path(job_root: &Path) -> PathBuf {
    job_root.join("fetch.json")
}

fn write_fetch_record(job_root: &Path, record: &FetchRecord) -> Result<()> {
    write_json(&fetch_record_path(job_root), record)
}

/// `None` if `fetch_batch` has never run for this student.
pub fn read_fetch_record(job_root: &Path) -> Result<Option<FetchRecord>> {
    let path = fetch_record_path(job_root);
    if !path.is_file() {
        return Ok(None);
    }
    read_json(&path)
}

/// Runs the Fetch stage alone: lands each submission at
/// `work_dir/<student_id>/checkout/` and records the outcome. Safe to run
/// again -- always overwrites both.
pub fn fetch_batch<F: Fetchable>(
    source: &dyn SubmissionsSource<F>,
    work_dir: &Path,
    deadline: &Zoned,
) -> Result<Vec<(StudentId, FetchRecord)>> {
    let submissions = source.submissions()?;
    let mut records = Vec::new();
    for submission in submissions {
        let job_root = work_dir.join(submission.student_id.as_str());
        let checkout_dir = job_root.join("checkout");
        let outcome = submission.fetch(&checkout_dir, deadline)?;
        let record = FetchRecord {
            status: outcome.status,
            graded_commit: outcome.graded_commit,
            message: outcome.message,
            fetched_at: Timestamp::now(),
        };
        write_fetch_record(&job_root, &record)?;
        records.push((submission.student_id, record));
    }
    Ok(records)
}

/// A synthetic content hash of a directory tree, standing in for a real
/// commit SHA for `LocalPath` submissions (which have no commit at all).
fn hash_tree(dir: &Path) -> Result<String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut paths = fs::walk_regular_files(dir)?;
    paths.sort();

    let mut hasher = DefaultHasher::new();
    for rel_path in &paths {
        rel_path.hash(&mut hasher);
        let contents = fs::read(&dir.join(rel_path))?;
        contents.hash(&mut hasher);
    }
    Ok(format!("{:016x}", hasher.finish()))
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
/// (`LocalPath::fetch`/`DirectorySource`/`fetch_batch`/`read_fetch_record`)
/// lives in `tests/fetch.rs` as an integration test instead (see that
/// file's doc comment). These stay here because the `git`-argv builders
/// and `run_git` are private -- there's no way to reach them from outside
/// the crate.
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

    // `GitRepo::fetch`'s live clone/resolve/checkout sequence needs real
    // git + network -- [deferred: needs network].
}
