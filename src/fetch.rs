use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{GitRepo, LocalPath, StageStatus, Submission};
use crate::source::SubmissionsSource;

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

/// A type that knows how to fetch its own submission into a job workspace.
/// Each `Fetchable` type has exactly one way to do that, so there is no
/// separate swappable "fetcher" object — the pluggable axis is
/// `SubmissionsSource<F>` (where submissions come from), not how a given
/// `Fetchable` resolves. A real GitHub-backed clone with server-side
/// push-time deadline selection lands on `GitRepo` in M6 (design §7.1).
pub trait Fetchable {
    fn fetch(&self, dest: &Path) -> Result<FetchOutcome>;
}

impl Fetchable for LocalPath {
    /// Copies the local directory wholesale into `dest`. Does not enforce
    /// push-time deadlines — there is no server for a local path.
    fn fetch(&self, dest: &Path) -> Result<FetchOutcome> {
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
    fn fetch(&self, _dest: &Path) -> Result<FetchOutcome> {
        Err(Error::NotImplemented(
            "cloning a GitRepo (GitHubFetcher lands in M6)",
        ))
    }
}

impl<F: Fetchable> Submission<F> {
    /// Fetches this submission into `dest`. Shorthand for
    /// `self.fetchable.fetch(dest)` so call sites don't have to reach
    /// through the field.
    pub fn fetch(&self, dest: &Path) -> Result<FetchOutcome> {
        self.fetchable.fetch(dest)
    }
}

/// A `SubmissionsSource<LocalPath>` that treats each subdirectory of `root` as one
/// student's submission (`student_id` = directory name), so a full run
/// needs only a folder of sample submissions — no CSV, no network.
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
/// commit SHA until the real GitHub fetcher lands (M6).
fn hash_tree(dir: &Path) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn local_path(path: impl Into<PathBuf>) -> LocalPath {
        LocalPath(path.into())
    }

    #[test]
    fn fetches_a_populated_directory_into_a_workspace() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("main.rs"), b"fn main() {}").unwrap();
        let dest = tempfile::tempdir().unwrap();
        let workspace = dest.path().join("job");

        let outcome = local_path(src.path()).fetch(&workspace).unwrap();

        assert_eq!(outcome.status, StageStatus::Ok);
        assert!(workspace.join("main.rs").exists());
        assert!(outcome.graded_commit.is_some());
    }

    #[test]
    fn missing_directory_yields_fetch_failed_not_a_panic() {
        let dest = tempfile::tempdir().unwrap();
        let workspace = dest.path().join("job");

        let outcome = local_path("/nonexistent/path/for/sure")
            .fetch(&workspace)
            .unwrap();

        assert_eq!(outcome.status, StageStatus::FetchFailed);
        assert!(outcome.workspace.is_none());
    }

    #[test]
    fn empty_directory_yields_fetch_failed() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let workspace = dest.path().join("job");

        let outcome = local_path(src.path()).fetch(&workspace).unwrap();

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
}
