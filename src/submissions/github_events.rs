//! Reads a repo's push history via `gh api`. `PushEvent.created_at` is
//! server-stamped, unlike a commit's own author/committer date.

use std::process::Command;

use jiff::Timestamp;
use serde::Deserialize;

use crate::error::{Error, Result};

const GH_BIN: &str = "gh";

/// One `PushEvent` from `GET /repos/{owner}/{repo}/events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushEvent {
    pub created_at: Timestamp,
    pub r#ref: String,
    pub head: String,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    r#type: String,
    created_at: Timestamp,
    #[serde(default)]
    payload: Option<RawPayload>,
}

#[derive(Debug, Deserialize)]
struct RawPayload {
    r#ref: Option<String>,
    head: Option<String>,
}

/// `None` means "not a GitHub URL," not an error.
pub fn parse_github_url(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim_end_matches('/');
    let path = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("ssh://git@github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);

    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

pub fn list_push_events(owner: &str, repo: &str) -> Result<Vec<PushEvent>> {
    let output = Command::new(GH_BIN)
        .args(["api", &format!("repos/{owner}/{repo}/events"), "--paginate"])
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    parse_push_events(&output.stdout)
}

/// `--paginate` concatenates pages as `[...][...]`, not one array, hence
/// the streaming deserializer.
fn parse_push_events(stdout: &[u8]) -> Result<Vec<PushEvent>> {
    let mut events = Vec::new();
    for page in serde_json::Deserializer::from_slice(stdout).into_iter::<Vec<RawEvent>>() {
        let page = page.map_err(|source| {
            Error::Other(format!("failed to parse `gh api` output: {source}"))
        })?;
        for raw in page {
            if raw.r#type != "PushEvent" {
                continue;
            }
            let Some(payload) = raw.payload else {
                continue;
            };
            let (Some(r#ref), Some(head)) = (payload.r#ref, payload.head) else {
                continue;
            };
            events.push(PushEvent {
                created_at: raw.created_at,
                r#ref,
                head,
            });
        }
    }
    Ok(events)
}

pub fn latest<'a>(
    events: &'a [PushEvent],
    ref_name: &str,
    deadline: Option<Timestamp>,
) -> Option<&'a PushEvent> {
    events
        .iter()
        .filter(|e| e.r#ref == ref_name)
        .filter(|e| deadline.is_none_or(|d| e.created_at <= d))
        .max_by_key(|e| e.created_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_github_url_handles_https_and_ssh_forms() {
        assert_eq!(
            parse_github_url("https://github.com/org/repo.git"),
            Some(("org".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_github_url("https://github.com/org/repo"),
            Some(("org".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_github_url("git@github.com:org/repo.git"),
            Some(("org".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_github_url("ssh://git@github.com/org/repo.git"),
            Some(("org".to_string(), "repo".to_string()))
        );
    }

    #[test]
    fn parse_github_url_rejects_non_github_hosts() {
        assert_eq!(parse_github_url("https://gitlab.com/org/repo.git"), None);
    }

    #[test]
    fn parse_push_events_skips_non_push_events_and_reads_across_pages() {
        let stdout = br#"[{"type":"PushEvent","created_at":"2026-02-14T10:00:00Z","payload":{"ref":"refs/heads/main","head":"aaa"}},{"type":"WatchEvent","created_at":"2026-02-14T11:00:00Z"}][{"type":"PushEvent","created_at":"2026-02-13T10:00:00Z","payload":{"ref":"refs/tags/listoco","head":"bbb"}}]"#;

        let events = parse_push_events(stdout).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].r#ref, "refs/heads/main");
        assert_eq!(events[0].head, "aaa");
        assert_eq!(events[1].r#ref, "refs/tags/listoco");
        assert_eq!(events[1].head, "bbb");
    }

    fn event(ref_name: &str, created_at: &str, head: &str) -> PushEvent {
        PushEvent {
            created_at: created_at.parse().unwrap(),
            r#ref: ref_name.to_string(),
            head: head.to_string(),
        }
    }

    #[test]
    fn latest_picks_the_newest_matching_ref() {
        let events = vec![
            event("refs/heads/main", "2026-02-10T00:00:00Z", "old"),
            event("refs/heads/main", "2026-02-14T00:00:00Z", "new"),
            event("refs/tags/listoco", "2026-02-15T00:00:00Z", "tag"),
        ];

        let found = latest(&events, "refs/heads/main", None).unwrap();
        assert_eq!(found.head, "new");
    }

    #[test]
    fn latest_respects_the_deadline() {
        let events = vec![
            event("refs/heads/main", "2026-02-10T00:00:00Z", "early"),
            event("refs/heads/main", "2026-02-20T00:00:00Z", "late"),
        ];

        let deadline = "2026-02-14T00:00:00Z".parse().unwrap();
        let found = latest(&events, "refs/heads/main", Some(deadline)).unwrap();
        assert_eq!(found.head, "early");
    }

    #[test]
    fn latest_is_none_when_nothing_matches() {
        let events = vec![event("refs/heads/main", "2026-02-10T00:00:00Z", "a")];
        assert!(latest(&events, "refs/tags/listoco", None).is_none());
    }
}
