//! Instructor overrides: which commit to grade for one student, whatever
//! the deadline or a bless tag would otherwise select. Recorded by
//! `autograder override` and applied by every later `autograder fetch`, so
//! re-fetching a class doesn't quietly undo an exception that was already
//! granted.
//!
//! Stored under the submissions dir rather than in the assignment repo:
//! `publish` ships `autograder.toml` to students verbatim, and which
//! student got an exception is nobody else's business.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::id::GithubUser;

const OVERRIDES_FILE: &str = "overrides.toml";

/// One granted exception. `commit` is stored as the instructor typed it --
/// a sha, a tag, `HEAD~2` -- and resolved against the student's own clone
/// at fetch time, which is the first point a repository exists to resolve
/// it in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Override {
    pub github_user: GithubUser,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub recorded_at: Timestamp,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OverridesFile {
    #[serde(default, rename = "override")]
    overrides: Vec<Override>,
}

pub fn overrides_path(out_dir: &Path) -> PathBuf {
    out_dir.join(".fetch").join(OVERRIDES_FILE)
}

/// Every override recorded for this submissions dir, in the order they
/// were written. An absent file is an empty list, not an error.
pub fn load(out_dir: &Path) -> Result<Vec<Override>> {
    let path = overrides_path(out_dir);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(&path)?;
    let parsed: OverridesFile = toml::from_str(&contents)
        .map_err(|source| Error::Other(format!("failed to parse {}: {source}", path.display())))?;
    Ok(parsed.overrides)
}

pub fn find(overrides: &[Override], github_user: &GithubUser) -> Option<Override> {
    overrides
        .iter()
        .find(|o| o.github_user == *github_user)
        .cloned()
}

/// Records `entry`, replacing any existing override for the same student
/// -- one exception per student, and the latest decision is the one that
/// stands. Returns the override it replaced, if any, so the caller can say
/// what changed.
pub fn record(out_dir: &Path, entry: Override) -> Result<Option<Override>> {
    let mut overrides = load(out_dir)?;
    let replaced = overrides
        .iter()
        .position(|o| o.github_user == entry.github_user)
        .map(|i| overrides.remove(i));
    overrides.push(entry);

    let contents = toml::to_string_pretty(&OverridesFile { overrides })
        .map_err(|source| Error::Other(format!("failed to render overrides: {source}")))?;
    let path = overrides_path(out_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, contents)?;
    Ok(replaced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(github_user: &str, commit: &str) -> Override {
        Override {
            github_user: GithubUser::new(github_user),
            commit: commit.to_string(),
            reason: Some("tagged the wrong commit".to_string()),
            recorded_at: Timestamp::now(),
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn recorded_overrides_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), entry("alice", "9f3c1ab")).unwrap();
        record(dir.path(), entry("bob", "abc1234")).unwrap();

        let loaded = load(dir.path()).unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(
            find(&loaded, &GithubUser::new("alice")).unwrap().commit,
            "9f3c1ab"
        );
        assert_eq!(
            find(&loaded, &GithubUser::new("alice"))
                .unwrap()
                .reason
                .unwrap(),
            "tagged the wrong commit"
        );
        assert!(find(&loaded, &GithubUser::new("carol")).is_none());
    }

    #[test]
    fn recording_twice_for_one_student_replaces_rather_than_appends() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), entry("alice", "9f3c1ab")).unwrap();

        let replaced = record(dir.path(), entry("alice", "deadbee")).unwrap();

        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].commit, "deadbee");
        assert_eq!(replaced.unwrap().commit, "9f3c1ab");
    }
}
