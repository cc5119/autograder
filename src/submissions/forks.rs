//! Fork discovery: the roster names students by GitHub handle, and their
//! submissions are the forks of one upstream assignment repo. `gh` is the
//! only GitHub client here -- `list_forks` reads the fork list through it
//! and `fetch_fork` clones through it, so authentication (private forks
//! included) is entirely `gh`'s business and no token ever reaches this
//! crate.

use std::collections::BTreeMap;
use std::fmt;
use std::process::Command;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::id::StudentId;

const GH_BIN: &str = "gh";

/// The upstream repo whose forks are the submissions, as `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub owner: String,
    pub name: String,
}

impl fmt::Display for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// Parses `owner/name`, so a malformed `--repo` fails in clap rather than
/// halfway through a batch.
impl FromStr for Upstream {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.trim_end_matches('/');
        let (owner, name) = s
            .split_once('/')
            .ok_or_else(|| format!("expected a repo as `owner/name`, got {s:?}"))?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return Err(format!("expected a repo as `owner/name`, got {s:?}"));
        }
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

/// One entry from `GET /repos/{owner}/{repo}/forks`. `owner` is an org
/// when that's where the fork lives, and `name` is whatever the student
/// renamed it to -- neither says whose submission this is, which is what
/// [`match_forks`] needs a collaborator lookup for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fork {
    pub owner: String,
    pub name: String,
    pub clone_url: String,
}

impl Fork {
    /// `owner/name`, the form `gh repo clone` takes.
    pub fn nwo(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Deserialize)]
struct RawFork {
    name: String,
    clone_url: String,
    owner: RawOwner,
}

#[derive(Debug, Deserialize)]
struct RawOwner {
    login: String,
}

/// Direct forks only -- GitHub doesn't return forks-of-forks here, so a
/// student who forked a classmate's fork looks unmatched.
pub fn list_forks(upstream: &Upstream) -> Result<Vec<Fork>> {
    let output = Command::new(GH_BIN)
        .args([
            "api",
            &format!("repos/{}/{}/forks", upstream.owner, upstream.name),
            "--paginate",
        ])
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "failed to list forks of {upstream}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_forks(&output.stdout)
}

/// `--paginate` concatenates pages as `[...][...]`, not one array, hence
/// the streaming deserializer.
fn parse_forks(stdout: &[u8]) -> Result<Vec<Fork>> {
    let mut forks = Vec::new();
    for page in serde_json::Deserializer::from_slice(stdout).into_iter::<Vec<RawFork>>() {
        let page = page
            .map_err(|source| Error::Other(format!("failed to parse `gh api` output: {source}")))?;
        forks.extend(page.into_iter().map(|raw| Fork {
            owner: raw.owner.login,
            name: raw.name,
            clone_url: raw.clone_url,
        }));
    }
    Ok(forks)
}

/// The logins with direct access to `fork`, from
/// `GET /repos/{owner}/{repo}/collaborators`. Needs push access on the
/// repo, which the instructor has by administering the org the forks live
/// in -- a failure here is a real error, not a case to shrug off, since it
/// would otherwise read as "no student owns this fork".
pub fn list_collaborators(fork: &Fork) -> Result<Vec<String>> {
    let output = Command::new(GH_BIN)
        .args([
            "api",
            &format!("repos/{}/collaborators", fork.nwo()),
            "--paginate",
        ])
        .output()
        .map_err(|source| Error::Other(format!("failed to run `{GH_BIN}`: {source}")))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "failed to list collaborators of {}: {}",
            fork.nwo(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_collaborators(&output.stdout)
}

fn parse_collaborators(stdout: &[u8]) -> Result<Vec<String>> {
    let mut logins = Vec::new();
    for page in serde_json::Deserializer::from_slice(stdout).into_iter::<Vec<RawOwner>>() {
        let page = page
            .map_err(|source| Error::Other(format!("failed to parse `gh api` output: {source}")))?;
        logins.extend(page.into_iter().map(|raw| raw.login));
    }
    Ok(logins)
}

/// A fork two or more roster students can push to. Fetched for nobody:
/// the submission it holds can't be attributed, and guessing an owner is
/// worse than saying so. Both students get a `Failed` record naming this.
#[derive(Debug, Clone)]
pub struct SharedFork {
    pub fork: Fork,
    pub students: Vec<StudentId>,
}

/// Every fork sorted into one of three buckets. See [`match_forks`].
#[derive(Debug, Default)]
pub struct Matches {
    /// Forks a single roster student can push to, in GitHub's order, so
    /// `[0]` is the one to fetch and any remainder is the ambiguity to
    /// report.
    pub by_student: BTreeMap<StudentId, Vec<Fork>>,
    pub shared: Vec<SharedFork>,
    /// No roster student can push to these -- outsiders forking a public
    /// assignment is normal.
    pub unmatched: Vec<Fork>,
}

impl Matches {
    /// The shared forks blocking `student`, for the message their `Failed`
    /// record carries.
    pub fn shared_for(&self, student: &StudentId) -> Vec<&SharedFork> {
        self.shared
            .iter()
            .filter(|s| s.students.contains(student))
            .collect()
    }
}

/// Sorts `forks` by which roster students can push to them, asking
/// `collaborators` who those are (one API call per fork -- the fork
/// listing itself says nothing about authorship). Logins are compared
/// case-insensitively: GitHub logins are case-preserving but not
/// case-sensitive, and a roster is hand-typed. Collaborators who aren't on
/// the roster -- instructors, TAs, org admins -- are ignored.
pub fn match_forks(
    students: impl IntoIterator<Item = StudentId>,
    forks: Vec<Fork>,
    mut collaborators: impl FnMut(&Fork) -> Result<Vec<String>>,
) -> Result<Matches> {
    let by_handle: BTreeMap<String, StudentId> = students
        .into_iter()
        .map(|id| (id.as_str().to_lowercase(), id))
        .collect();

    let mut matches = Matches::default();
    for fork in forks {
        let mut owners: Vec<StudentId> = collaborators(&fork)?
            .iter()
            .filter_map(|login| by_handle.get(&login.to_lowercase()).copied())
            .collect();
        owners.dedup();

        match owners.len() {
            0 => matches.unmatched.push(fork),
            1 => matches.by_student.entry(owners[0]).or_default().push(fork),
            _ => matches.shared.push(SharedFork {
                fork,
                students: owners,
            }),
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_parses_owner_slash_name() {
        let upstream: Upstream = "ucsc-cse130/hw3".parse().unwrap();
        assert_eq!(upstream.owner, "ucsc-cse130");
        assert_eq!(upstream.name, "hw3");
        assert_eq!(upstream.to_string(), "ucsc-cse130/hw3");
    }

    #[test]
    fn upstream_tolerates_a_dot_git_suffix_and_trailing_slash() {
        assert_eq!(
            "org/repo.git".parse::<Upstream>().unwrap(),
            "org/repo/".parse::<Upstream>().unwrap()
        );
    }

    #[test]
    fn upstream_rejects_anything_that_is_not_owner_slash_name() {
        for bad in ["repo", "https://github.com/org/repo", "/repo", "org/"] {
            assert!(bad.parse::<Upstream>().is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn parse_forks_reads_across_pages() {
        let stdout = br#"[{"name":"hw3","clone_url":"https://github.com/alice/hw3.git","owner":{"login":"alice"}}][{"name":"hw3-mine","clone_url":"https://github.com/bob/hw3-mine.git","owner":{"login":"bob"}}]"#;

        let forks = parse_forks(stdout).unwrap();

        assert_eq!(forks.len(), 2);
        assert_eq!(forks[0].owner, "alice");
        assert_eq!(forks[0].nwo(), "alice/hw3");
        assert_eq!(forks[1].nwo(), "bob/hw3-mine");
    }

    #[test]
    fn parse_collaborators_reads_logins_across_pages() {
        let stdout = br#"[{"login":"alice"},{"login":"ta-1"}][{"login":"bob"}]"#;
        assert_eq!(
            parse_collaborators(stdout).unwrap(),
            ["alice", "ta-1", "bob"]
        );
    }

    /// Named after the org it lives in, like the real thing -- the owner
    /// and name deliberately say nothing about who submitted it.
    fn fork(name: &str) -> Fork {
        Fork {
            owner: "cse130".to_string(),
            name: name.to_string(),
            clone_url: format!("https://github.com/cse130/{name}.git"),
        }
    }

    /// A collaborator lookup driven by the fork's name, so tests never
    /// touch the network.
    fn access(
        pairs: &[(&'static str, &'static [&'static str])],
    ) -> impl Fn(&Fork) -> Result<Vec<String>> {
        let map: BTreeMap<String, Vec<String>> = pairs
            .iter()
            .map(|(name, logins)| {
                (
                    name.to_string(),
                    logins.iter().map(|l| l.to_string()).collect(),
                )
            })
            .collect();
        move |fork: &Fork| Ok(map.get(&fork.name).cloned().unwrap_or_default())
    }

    #[test]
    fn match_forks_attributes_by_collaborator_not_by_owner_or_name() {
        let students = [StudentId::new("alice"), StudentId::new("bob")];
        let forks = vec![fork("hw3-a"), fork("whatever")];
        let matches = match_forks(
            students,
            forks,
            access(&[
                ("hw3-a", &["instructor", "alice"]),
                ("whatever", &["bob", "ta-1"]),
            ]),
        )
        .unwrap();

        assert_eq!(
            matches.by_student[&StudentId::new("alice")][0].name,
            "hw3-a"
        );
        assert_eq!(
            matches.by_student[&StudentId::new("bob")][0].name,
            "whatever"
        );
        assert!(matches.unmatched.is_empty());
    }

    #[test]
    fn match_forks_compares_logins_case_insensitively() {
        let matches = match_forks(
            [StudentId::new("Alice")],
            vec![fork("hw3-a")],
            access(&[("hw3-a", &["ALICE"])]),
        )
        .unwrap();

        assert_eq!(matches.by_student[&StudentId::new("Alice")].len(), 1);
    }

    #[test]
    fn match_forks_sets_aside_a_fork_no_roster_student_can_push_to() {
        let matches = match_forks(
            [StudentId::new("alice")],
            vec![fork("hw3-x")],
            access(&[("hw3-x", &["carol", "instructor"])]),
        )
        .unwrap();

        assert!(matches.by_student.is_empty());
        assert_eq!(matches.unmatched[0].name, "hw3-x");
    }

    #[test]
    fn match_forks_keeps_every_candidate_in_github_order() {
        let matches = match_forks(
            [StudentId::new("alice")],
            vec![fork("first"), fork("second")],
            access(&[("first", &["alice"]), ("second", &["alice"])]),
        )
        .unwrap();

        let candidates = &matches.by_student[&StudentId::new("alice")];
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].name, "first");
        assert_eq!(candidates[1].name, "second");
    }

    #[test]
    fn match_forks_sets_aside_a_fork_two_students_share_rather_than_guessing() {
        let students = [StudentId::new("alice"), StudentId::new("bob")];
        let matches = match_forks(
            students,
            vec![fork("shared")],
            access(&[("shared", &["alice", "bob", "instructor"])]),
        )
        .unwrap();

        assert!(matches.by_student.is_empty());
        assert_eq!(matches.shared.len(), 1);
        assert_eq!(
            matches.shared[0].students,
            [StudentId::new("alice"), StudentId::new("bob")]
        );
        assert_eq!(
            matches.shared_for(&StudentId::new("bob"))[0].fork.name,
            "shared"
        );
        assert!(matches.shared_for(&StudentId::new("carol")).is_empty());
    }

    #[test]
    fn a_shared_fork_does_not_cost_a_student_their_own_unshared_one() {
        let students = [StudentId::new("alice"), StudentId::new("bob")];
        let matches = match_forks(
            students,
            vec![fork("shared"), fork("alices-own")],
            access(&[("shared", &["alice", "bob"]), ("alices-own", &["alice"])]),
        )
        .unwrap();

        assert_eq!(
            matches.by_student[&StudentId::new("alice")][0].name,
            "alices-own"
        );
        assert_eq!(matches.shared.len(), 1);
    }

    #[test]
    fn match_forks_leaves_out_a_student_with_no_fork() {
        let matches = match_forks([StudentId::new("alice")], vec![], access(&[])).unwrap();
        assert!(matches.by_student.is_empty());
    }

    #[test]
    fn match_forks_propagates_a_collaborator_lookup_failure() {
        let err = match_forks([StudentId::new("alice")], vec![fork("hw3-a")], |_| {
            Err(Error::Other("403".to_string()))
        })
        .unwrap_err();

        assert!(err.to_string().contains("403"));
    }
}
