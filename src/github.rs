//! The `gh`-shaped reads both roster-facing stages need. `gh` is the only
//! GitHub client in this crate (see [`crate::submissions::forks`]), so
//! authentication is entirely its business and no token reaches this code.
//!
//! Nothing here writes: [`crate::enroll`] and [`crate::release`] own their
//! own single write, which is what makes planning free to decline.

use std::collections::BTreeMap;
use std::process::Command;

use rayon::prelude::*;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::id::GithubUser;
use crate::render::bar;

pub(crate) const GH_BIN: &str = "gh";

#[derive(Debug, Deserialize)]
struct RawLogin {
    login: String,
}

/// Lowercased, since every comparison against a hand-typed roster is
/// case-insensitive.
pub(crate) fn logins(path: &str) -> Result<std::collections::BTreeSet<String>> {
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
    created_at: jiff::Timestamp,
}

/// Who has an unaccepted invitation to `org`, and since when. Shared by
/// [`crate::enroll`], which sends them, and [`crate::release`], which skips
/// the people waiting on one.
pub(crate) fn org_invitations(org: &str) -> Result<BTreeMap<String, jiff::Timestamp>> {
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
pub(crate) fn user_exists(user: &str) -> Result<bool> {
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

/// A pool of `jobs` threads, and not rayon's global one: that sizes itself
/// to the CPU count, the wrong number entirely for work that spends its
/// life blocked on `gh`. `what` names the work in the failure message.
pub(crate) fn pool(jobs: usize, what: &str) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .map_err(|source| Error::Other(format!("failed to start {jobs} {what} threads: {source}")))
}

/// Which of `unknown` are real GitHub logins, `jobs` lookups at a time,
/// keyed the way it was handed in. A login a cheaper read already
/// accounted for demonstrably exists, so callers only ask about the rest.
pub(crate) fn check_users(
    unknown: &BTreeMap<String, GithubUser>,
    jobs: usize,
) -> Result<BTreeMap<String, bool>> {
    if unknown.is_empty() {
        return Ok(BTreeMap::new());
    }

    let progress = bar(unknown.len() as u64);

    let pool = pool(jobs, "lookup")?;

    let checked = pool.install(|| {
        unknown
            .par_iter()
            .map(|(handle, user)| {
                let exists = user_exists(user.as_str())?;
                progress.inc(1);
                Ok((handle.clone(), exists))
            })
            .collect()
    });
    progress.finish_and_clear();
    checked
}

/// One `gh api --paginate` read. `--paginate` concatenates pages as
/// `[...][...]` rather than one array, hence the streaming deserializer
/// (same as [`crate::submissions::forks`]).
pub(crate) fn paginated<T: serde::de::DeserializeOwned>(path: &str) -> Result<Vec<T>> {
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

pub(crate) fn parse_pages<T: serde::de::DeserializeOwned>(stdout: &[u8]) -> Result<Vec<T>> {
    let mut items = Vec::new();
    for page in serde_json::Deserializer::from_slice(stdout).into_iter::<Vec<T>>() {
        let page = page
            .map_err(|source| Error::Other(format!("failed to parse `gh api` output: {source}")))?;
        items.extend(page);
    }
    Ok(items)
}

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
}
