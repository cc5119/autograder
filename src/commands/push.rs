use std::path::Path;
use std::process::Command;

use dialoguer::{Confirm, Input};
use jiff::Zoned;

use crate::error::{Error, Result};
use crate::spec::Spec;

const OWNER: &str = "cc5119";
const BOOKMARK: &str = "main";

/// Creates a private GitHub repo named `<year>-<semester>-<id>-starter` and pushes `dir` there.
/// Initializes `dir` as a repo (preferring `jj` over `git` if it's on `PATH`) only if it isn't
/// one already.
pub fn run(dir: &Path) -> Result<()> {
    let vcs = Vcs::detect();
    if vcs.already_has_a_local_bookmark(dir) {
        return Err(Error::Other(format!(
            "{} already has a {BOOKMARK:?} bookmark/branch -- `push` only initializes a fresh \
             starter repo, it never updates an existing one",
            dir.display()
        )));
    }

    let spec = Spec::load(dir)?;
    let id = spec.assignment.id;

    let semester = prompt_semester()?;
    let year = Zoned::now().year();
    let repo_name = format!("{year}-{semester:02}-{id}-starter");
    let full_name = format!("{OWNER}/{repo_name}");

    if remote_repo_exists(&full_name)? {
        return Err(Error::Other(format!(
            "{full_name} already exists on GitHub -- `push` only initializes a fresh starter \
             repo, it never updates an existing one"
        )));
    }

    let confirmed = Confirm::new()
        .with_prompt(format!(
            "Create private repo {full_name} and push {}?",
            dir.display()
        ))
        .default(false)
        .interact()
        .map_err(|source| Error::Other(format!("failed to read confirmation: {source}")))?;
    if !confirmed {
        println!("aborted");
        return Ok(());
    }

    vcs.init_and_commit(dir)?;

    run_cmd(dir, "gh", &["repo", "create", &full_name, "--private"])?;
    let remote_url = format!("git@github.com:{full_name}.git");
    vcs.add_remote_and_push(dir, &remote_url)?;

    println!("pushed to https://github.com/{full_name}");
    Ok(())
}

fn prompt_semester() -> Result<u8> {
    Input::<u8>::new()
        .with_prompt("Semester (1/2)")
        .validate_with(|input: &u8| -> std::result::Result<(), &str> {
            match input {
                1 | 2 => Ok(()),
                _ => Err("must be 1 or 2"),
            }
        })
        .interact_text()
        .map_err(|source| Error::Other(format!("failed to read semester: {source}")))
}

fn remote_repo_exists(full_name: &str) -> Result<bool> {
    Command::new("gh")
        .args(["repo", "view", full_name])
        .output()
        .map(|output| output.status.success())
        .map_err(|source| Error::Io {
            path: full_name.into(),
            source,
        })
}

enum Vcs {
    Jj,
    Git,
}

impl Vcs {
    fn detect() -> Self {
        let available = Command::new("jj")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if available { Vcs::Jj } else { Vcs::Git }
    }

    fn already_has_a_local_bookmark(&self, dir: &Path) -> bool {
        match self {
            Vcs::Git => Command::new("git")
                .args([
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("refs/heads/{BOOKMARK}"),
                ])
                .current_dir(dir)
                .output()
                .is_ok_and(|output| output.status.success()),
            Vcs::Jj => Command::new("jj")
                .args(["bookmark", "list", BOOKMARK])
                .current_dir(dir)
                .output()
                .is_ok_and(|output| !output.stdout.is_empty()),
        }
    }

    fn init_and_commit(&self, dir: &Path) -> Result<()> {
        match self {
            Vcs::Git => {
                run_cmd(dir, "git", &["init", "-b", BOOKMARK])?;
                run_cmd(dir, "git", &["add", "-A"])?;
                run_cmd(dir, "git", &["commit", "-m", "Initial commit"])
            }
            Vcs::Jj => {
                run_cmd(dir, "jj", &["git", "init", "--colocate"])?;
                run_cmd(dir, "jj", &["describe", "-m", "Initial commit"])?;
                run_cmd(dir, "jj", &["bookmark", "create", BOOKMARK, "-r", "@"])
            }
        }
    }

    fn add_remote_and_push(&self, dir: &Path, remote_url: &str) -> Result<()> {
        match self {
            Vcs::Git => {
                run_cmd(dir, "git", &["remote", "add", "origin", remote_url])?;
                run_cmd(dir, "git", &["push", "-u", "origin", BOOKMARK])
            }
            Vcs::Jj => {
                run_cmd(dir, "jj", &["git", "remote", "add", "origin", remote_url])?;
                run_cmd(
                    dir,
                    "jj",
                    &["git", "push", "--remote", "origin", "--bookmark", BOOKMARK],
                )
            }
        }
    }
}

fn run_cmd(dir: &Path, program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    if !status.success() {
        return Err(Error::Other(format!(
            "{program} {} failed in {}",
            args.join(" "),
            dir.display()
        )));
    }
    Ok(())
}
