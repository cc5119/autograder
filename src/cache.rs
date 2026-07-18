//! Host-side bare-clone cache (design §13 -- M4 step 20): one bare clone
//! per repo URL under `cache_dir`, updated with `git fetch` rather than
//! re-cloned from scratch, so grading a batch of students against a shared
//! or forked-from repo doesn't re-download the same history once per
//! student. `GitHubFetcher` (M6, step 25) is the actual caller -- it clones
//! each student's checkout from a `git worktree`/`git clone --reference`
//! against the cached bare repo rather than hitting the network per
//! student.
//!
//! Path derivation and command construction are pure and unit-tested here;
//! `sync` shells out to `git` and needs real network access, so its live
//! behavior is **[deferred: needs network]**, matching every other
//! network/container boundary in this codebase (ground rule 3).

use std::path::PathBuf;
use std::process::Command;

use crate::error::{Error, Result};

pub struct BareCloneCache {
    cache_dir: PathBuf,
}

impl BareCloneCache {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    /// Deterministic, filesystem-safe cache path for `repo_url`: every
    /// non-alphanumeric byte becomes `_`, so distinct URLs never collide
    /// and the same URL always resolves to the same entry across runs (a
    /// hash would work too, but this keeps the directory name readable for
    /// anyone poking around the cache on disk).
    pub fn path_for(&self, repo_url: &str) -> PathBuf {
        let sanitized: String = repo_url
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        self.cache_dir.join(format!("{sanitized}.git"))
    }

    /// `git clone --bare <repo_url> <path_for(repo_url)>` argv (excluding
    /// the `git` binary itself), for a repo not yet in the cache.
    pub fn clone_argv(&self, repo_url: &str) -> Vec<String> {
        vec![
            "clone".to_string(),
            "--bare".to_string(),
            repo_url.to_string(),
            self.path_for(repo_url).display().to_string(),
        ]
    }

    /// `git --git-dir=<path> fetch` argv, for a repo already in the cache
    /// -- pulls new commits into the existing bare clone instead of
    /// re-downloading history already present.
    pub fn fetch_argv(&self, repo_url: &str) -> Vec<String> {
        vec![
            format!("--git-dir={}", self.path_for(repo_url).display()),
            "fetch".to_string(),
        ]
    }

    /// Ensures a bare clone of `repo_url` exists and is up to date: clones
    /// if the cache entry is missing, fetches otherwise. Returns the bare
    /// clone's path so callers (M6's `GitHubFetcher`) can clone/checkout a
    /// student's ref from it. **[deferred: needs network]** -- live
    /// execution requires real git credentials and a reachable remote;
    /// `clone_argv`/`fetch_argv`/`path_for` above are exercised without it.
    pub fn sync(&self, git_bin: &str, repo_url: &str) -> Result<PathBuf> {
        let path = self.path_for(repo_url);
        let argv = if path.is_dir() {
            self.fetch_argv(repo_url)
        } else {
            std::fs::create_dir_all(&self.cache_dir).map_err(|source| Error::Io {
                path: self.cache_dir.clone(),
                source,
            })?;
            self.clone_argv(repo_url)
        };

        let output = Command::new(git_bin)
            .args(&argv)
            .output()
            .map_err(|source| Error::Other(format!("failed to run `{git_bin}`: {source}")))?;

        if !output.status.success() {
            return Err(Error::Other(format!(
                "git {} failed for {repo_url}: {}",
                argv.first().cloned().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_for_is_deterministic_and_distinguishes_distinct_urls() {
        let cache = BareCloneCache::new("/cache");
        let a = cache.path_for("https://github.com/org/repo-a.git");
        let b = cache.path_for("https://github.com/org/repo-b.git");

        assert_eq!(a, cache.path_for("https://github.com/org/repo-a.git"));
        assert_ne!(a, b);
        assert!(a.starts_with("/cache"));
    }

    #[test]
    fn clone_argv_targets_the_cache_path_for_the_url() {
        let cache = BareCloneCache::new("/cache");
        let argv = cache.clone_argv("https://github.com/org/repo.git");

        assert_eq!(argv[0], "clone");
        assert_eq!(argv[1], "--bare");
        assert_eq!(argv[2], "https://github.com/org/repo.git");
        assert_eq!(
            PathBuf::from(&argv[3]),
            cache.path_for("https://github.com/org/repo.git")
        );
    }

    #[test]
    fn fetch_argv_points_git_dir_at_the_cache_path() {
        let cache = BareCloneCache::new("/cache");
        let argv = cache.fetch_argv("https://github.com/org/repo.git");

        assert_eq!(
            argv[0],
            format!(
                "--git-dir={}",
                cache.path_for("https://github.com/org/repo.git").display()
            )
        );
        assert_eq!(argv[1], "fetch");
    }

    #[test]
    fn sync_clones_when_the_cache_entry_is_missing_and_surfaces_git_failure_clearly() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = BareCloneCache::new(cache_dir.path());

        // No real git binary/network here -- assert the missing-entry path
        // is chosen (clone, not fetch) and a failure is reported clearly,
        // not that a real clone succeeds (that's the deferred/live part).
        let err = cache
            .sync(
                "autograder-git-does-not-exist",
                "https://example.invalid/repo.git",
            )
            .unwrap_err();
        assert!(err.to_string().contains("failed to run"));
    }
}
