//! The Fetch stage (design §7.1): pulls each student's submission onto
//! disk at `work_dir/<student_id>/checkout/`, independently of grading --
//! `fetch_batch` is what `autograder fetch` runs directly, and what
//! `autograder grade --fetch` runs before Prepare/Evaluate/Grade. Grading
//! without `--fetch` never touches this module at all; it just reads the
//! [`FetchRecord`] a prior `fetch_batch` run left behind (see
//! `crate::pipeline::grade_batch`), so re-running just Prepare/Evaluate/
//! Grade (e.g. after fixing a harness bug) needs no network access.
//!
//! Each `Fetchable` type has exactly one way to fetch itself, so there is
//! no separate swappable "fetcher" object -- the pluggable axis is
//! `SubmissionsSource<F>` (where submissions come from), not how a given
//! `Fetchable` resolves. Every `Fetchable` takes the grading deadline,
//! even `LocalPath` (which ignores it -- there's no server-side push time
//! for a local directory to compare against): a single, uniform `fetch`
//! signature for every fetchable type is simpler than an associated
//! per-type context that only `GitRepo` actually needs.
//!
//! `GitRepo::fetch` shells out to `git` (assumed to be on `PATH`, same as
//! every other tool boundary in this codebase) -- a full, fresh `git
//! clone` into `dest` on every call, no shared bare-clone cache. That
//! *would* help students whose repos share ancestry (e.g. everyone forked
//! the same template), but only if the roster's `repo_url`s actually
//! coincide (an unusual roster shape) -- for the common one-fork-per-
//! student case it buys nothing, since a cache keyed by `repo_url` never
//! sees the same URL twice. A full clone is also what the deadline-based
//! ref search below needs anyway (it walks commit history), so there's no
//! shallow-clone shortcut being left on the table by skipping the cache.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{GitRepo, LocalPath, StageStatus, Submission};
use crate::source::SubmissionsSource;
use crate::store::{read_json, write_json};

const GIT_BIN: &str = "git";

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

/// A type that knows how to fetch its own submission into a job workspace,
/// given the grading deadline (see this module's doc comment for why every
/// impl takes it, even ones that ignore it).
pub trait Fetchable {
    fn fetch(&self, dest: &Path, deadline: DateTime<FixedOffset>) -> Result<FetchOutcome>;
}

impl Fetchable for LocalPath {
    /// Copies the local directory wholesale into `dest`. Does not enforce
    /// push-time deadlines -- there is no server for a local path.
    fn fetch(&self, dest: &Path, _deadline: DateTime<FixedOffset>) -> Result<FetchOutcome> {
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
        if is_empty_dir(src)? {
            return Ok(FetchOutcome::failed(format!(
                "source directory {} is empty",
                src.display()
            )));
        }

        copy_dir_all(src, dest)?;
        let graded_commit = hash_tree(dest)?;
        Ok(FetchOutcome::ok(dest.to_path_buf(), graded_commit))
    }
}

impl Fetchable for GitRepo {
    /// **[deferred: needs network]** -- clones `self.url` fresh into
    /// `dest` (wiping any prior checkout there first), then resolves the
    /// commit to check out: `self.ref` if the roster pinned one
    /// (`rev-parse`d to a full SHA, so `FetchOutcome::graded_commit`
    /// always reports a real hash, never a symbolic name); otherwise the
    /// last commit at or before `deadline` on the repo's default branch
    /// (push-time deadline selection, design §7.1). Every failure mode
    /// here (clone/resolve/checkout) degrades to `FetchOutcome::failed`
    /// rather than a hard `Err`, matching `LocalPath`'s shape -- one
    /// unreachable/private/misconfigured repo shouldn't abort the whole
    /// batch.
    fn fetch(&self, dest: &Path, deadline: DateTime<FixedOffset>) -> Result<FetchOutcome> {
        if dest.exists() {
            std::fs::remove_dir_all(dest).map_err(|source| Error::Io {
                path: dest.to_path_buf(),
                source,
            })?;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
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
    /// Fetches this submission into `dest`. Shorthand for
    /// `self.fetchable.fetch(dest, deadline)` so call sites don't have to
    /// reach through the field.
    pub fn fetch(&self, dest: &Path, deadline: DateTime<FixedOffset>) -> Result<FetchOutcome> {
        self.fetchable.fetch(dest, deadline)
    }
}

/// A `SubmissionsSource<LocalPath>` that treats each subdirectory of `root` as one
/// student's submission (`student_id` = directory name), so a full run
/// needs only a folder of sample submissions -- no CSV, no network.
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
        let entries = std::fs::read_dir(&self.root).map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| Error::Io {
                path: self.root.clone(),
                source,
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let student_id = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
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
/// `crate::pipeline::grade_batch` -- so grading can reuse a checkout
/// without re-fetching, and still know what it's grading (`graded_commit`)
/// without re-deriving it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRecord {
    pub status: StageStatus,
    pub graded_commit: Option<String>,
    pub message: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

fn fetch_record_path(job_root: &Path) -> PathBuf {
    job_root.join("fetch.json")
}

fn write_fetch_record(job_root: &Path, record: &FetchRecord) -> Result<()> {
    write_json(&fetch_record_path(job_root), record)
}

/// The record of the most recent `fetch_batch` run for the student at
/// `job_root`, or `None` if `fetch_batch` has never run for them (a clean
/// `autograder grade` with no prior `fetch`, no `--fetch`).
pub fn read_fetch_record(job_root: &Path) -> Result<Option<FetchRecord>> {
    let path = fetch_record_path(job_root);
    if !path.is_file() {
        return Ok(None);
    }
    read_json(&path)
}

/// Runs the Fetch stage alone, for every submission `source` yields: lands
/// each one at `work_dir/<student_id>/checkout/` and durably records the
/// outcome (see [`FetchRecord`]) so a later `grade` (with or without
/// `--fetch`) knows what's there without re-fetching. Always overwrites
/// both -- safe, and expected, to run again (a later `--as-of` cutoff, a
/// resubmission window, just picking up new commits).
pub fn fetch_batch<F: Fetchable>(
    source: &dyn SubmissionsSource<F>,
    work_dir: &Path,
    deadline: DateTime<FixedOffset>,
) -> Result<Vec<(String, FetchRecord)>> {
    let submissions = source.submissions()?;
    let mut records = Vec::new();
    for submission in submissions {
        let job_root = work_dir.join(&submission.student_id);
        let checkout_dir = job_root.join("checkout");
        let outcome = submission.fetch(&checkout_dir, deadline)?;
        let record = FetchRecord {
            status: outcome.status,
            graded_commit: outcome.graded_commit,
            message: outcome.message,
            fetched_at: Utc::now(),
        };
        write_fetch_record(&job_root, &record)?;
        records.push((submission.student_id, record));
    }
    Ok(records)
}

fn is_empty_dir(dir: &Path) -> Result<bool> {
    let mut entries = std::fs::read_dir(dir).map_err(|source| Error::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(entries.next().is_none())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|source| Error::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    let entries = std::fs::read_dir(src).map_err(|source| Error::Io {
        path: src.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: src.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &dst_path).map_err(|source| Error::Io {
                path: entry.path(),
                source,
            })?;
        }
    }
    Ok(())
}

/// A synthetic content hash of a directory tree, standing in for a real
/// commit SHA for `LocalPath` submissions (which have no commit at all).
fn hash_tree(dir: &Path) -> Result<String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut paths = Vec::new();
    collect_files(dir, dir, &mut paths)?;
    paths.sort();

    let mut hasher = DefaultHasher::new();
    for rel_path in &paths {
        rel_path.hash(&mut hasher);
        let contents = std::fs::read(dir.join(rel_path)).map_err(|source| Error::Io {
            path: dir.join(rel_path),
            source,
        })?;
        contents.hash(&mut hasher);
    }
    Ok(format!("{:016x}", hasher.finish()))
}

fn collect_files(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(current).map_err(|source| Error::Io {
        path: current.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: current.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    Ok(())
}

/// `git clone <repo_url> <dest>` argv (excluding the `git` binary itself).
/// A full clone, not shallow: [`last_commit_before_argv`] needs the real
/// commit history to search when a roster row doesn't pin a `ref`.
fn clone_argv(repo_url: &str, dest: &Path) -> Vec<String> {
    vec![
        "clone".to_string(),
        repo_url.to_string(),
        dest.display().to_string(),
    ]
}

/// `git -C <dest> symbolic-ref --short HEAD` argv -- the clone's default
/// branch, used when a roster row leaves `ref` unset.
fn default_branch_argv(dest: &Path) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "symbolic-ref".to_string(),
        "--short".to_string(),
        "HEAD".to_string(),
    ]
}

/// `git -C <dest> log --before=<deadline> -1 --format=%H <branch>` argv --
/// the SHA of the last commit at or before `deadline` on `branch` (push-time
/// deadline selection). Empty stdout means nothing was pushed before the
/// deadline.
fn last_commit_before_argv(
    dest: &Path,
    branch: &str,
    deadline: DateTime<FixedOffset>,
) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "log".to_string(),
        format!("--before={}", deadline.to_rfc3339()),
        "-1".to_string(),
        "--format=%H".to_string(),
        branch.to_string(),
    ]
}

/// `git -C <dest> rev-parse <ref>` argv -- resolves a pinned branch/tag/sha
/// to a full commit SHA.
fn rev_parse_argv(dest: &Path, r#ref: &str) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "rev-parse".to_string(),
        r#ref.to_string(),
    ]
}

/// `git -C <dest> checkout <sha>` argv.
fn checkout_argv(dest: &Path, sha: &str) -> Vec<String> {
    vec![
        "-C".to_string(),
        dest.display().to_string(),
        "checkout".to_string(),
        sha.to_string(),
    ]
}

/// Runs `<git_bin> <argv>`, returning trimmed stdout on success.
/// **[deferred: needs network]** for the clone/fetch-touching argv above --
/// `git_bin` stays a parameter (rather than always `GIT_BIN`) purely so
/// tests can exercise the failure path with a binary that doesn't exist,
/// without needing real git or network access, matching the convention
/// `cache.rs` established for this codebase's other network boundary.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn local_path(path: impl Into<PathBuf>) -> LocalPath {
        LocalPath(path.into())
    }

    /// `LocalPath::fetch` ignores the deadline entirely -- this stands in
    /// for it in tests that don't care what it is.
    fn test_deadline() -> DateTime<FixedOffset> {
        "2026-02-14T23:59:59-08:00".parse().unwrap()
    }

    #[test]
    fn fetches_a_populated_directory_into_a_workspace() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("main.rs"), b"fn main() {}").unwrap();
        let dest = tempfile::tempdir().unwrap();
        let workspace = dest.path().join("job");

        let outcome = local_path(src.path())
            .fetch(&workspace, test_deadline())
            .unwrap();

        assert_eq!(outcome.status, StageStatus::Ok);
        assert!(workspace.join("main.rs").exists());
        assert!(outcome.graded_commit.is_some());
    }

    #[test]
    fn missing_directory_yields_fetch_failed_not_a_panic() {
        let dest = tempfile::tempdir().unwrap();
        let workspace = dest.path().join("job");

        let outcome = local_path("/nonexistent/path/for/sure")
            .fetch(&workspace, test_deadline())
            .unwrap();

        assert_eq!(outcome.status, StageStatus::FetchFailed);
        assert!(outcome.workspace.is_none());
    }

    #[test]
    fn empty_directory_yields_fetch_failed() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let workspace = dest.path().join("job");

        let outcome = local_path(src.path())
            .fetch(&workspace, test_deadline())
            .unwrap();

        assert_eq!(outcome.status, StageStatus::FetchFailed);
    }

    #[test]
    fn directory_source_treats_each_subdir_as_a_student() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("alice")).unwrap();
        std::fs::create_dir(root.path().join("bob")).unwrap();

        let submissions = DirectorySource::new(root.path()).submissions().unwrap();

        assert_eq!(submissions.len(), 2);
        assert_eq!(submissions[0].student_id, "alice");
        assert_eq!(submissions[1].student_id, "bob");
    }

    #[test]
    fn fetch_batch_writes_a_fetch_record_per_student() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alice")).unwrap();
        std::fs::write(root.path().join("alice/main.rs"), b"fn main() {}").unwrap();
        let source = DirectorySource::new(root.path());
        let work_dir = tempfile::tempdir().unwrap();

        let records = fetch_batch(&source, work_dir.path(), test_deadline()).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, "alice");
        assert_eq!(records[0].1.status, StageStatus::Ok);
        assert!(work_dir.path().join("alice/checkout/main.rs").is_file());

        let record = read_fetch_record(&work_dir.path().join("alice"))
            .unwrap()
            .unwrap();
        assert_eq!(record.status, StageStatus::Ok);
        assert!(record.graded_commit.is_some());
    }

    #[test]
    fn read_fetch_record_is_none_when_nothing_has_fetched_yet() {
        let work_dir = tempfile::tempdir().unwrap();
        assert!(
            read_fetch_record(&work_dir.path().join("alice"))
                .unwrap()
                .is_none()
        );
    }

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
        let deadline: DateTime<FixedOffset> = "2026-02-14T23:59:59-08:00".parse().unwrap();
        let argv = last_commit_before_argv(Path::new("/tmp/dest"), "main", deadline);
        assert_eq!(
            argv,
            vec![
                "-C",
                "/tmp/dest",
                "log",
                "--before=2026-02-14T23:59:59-08:00",
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

    // `GitRepo::fetch`'s live clone/resolve/checkout sequence needs a real
    // `git` binary and network access -- **[deferred: needs network]**,
    // matching every other network boundary in this codebase (see
    // `run_git`, and `cache.rs`'s equivalent convention before it was
    // removed in favor of this module). The argv-construction functions
    // above and `run_git`'s own failure path are what's exercised without
    // it.
}
