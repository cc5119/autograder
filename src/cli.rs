use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{package::PublishMode, submissions};

#[derive(Debug, Parser)]
// `max_term_width`, not `term_width`: help follows a narrow terminal but
// stops widening past readability on a wide one. Needs clap's `wrap_help`
// feature -- without it both settings are silently ignored.
#[command(
    name = "autograder",
    about = "Rust assignment autograder",
    max_term_width = 100
)]
pub struct Cli {
    /// Print the commit this binary was built from (and its date) and exit.
    #[arg(short = 'V', long)]
    pub version: bool,
    /// Use verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// `env!()`, not `option_env!()` -- `build.rs` always sets these (falling
/// back to `"unknown"` itself when `git` isn't available), so a missing var
/// here would mean `build.rs` itself is broken, not a legitimate runtime
/// case to handle gracefully.
pub fn version_string(verbose: bool) -> String {
    let hash = if verbose {
        env!("AUTOGRADER_COMMIT_FULL")
    } else {
        env!("AUTOGRADER_COMMIT_SHORT")
    };
    format!("autograder {hash} ({})", env!("AUTOGRADER_COMMIT_DATE"))
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold an assignment.
    Init {
        /// Directory to create the new workspace.
        dir: PathBuf,
        /// `[assignment].id`: the crate name, and the directory name the
        /// reference solution lives in.
        #[arg(long)]
        id: String,
    },
    /// (Re)resolve the workspace-root `Cargo.lock` and record its hash in
    /// `autograder.toml` as the "blessed" lock.
    Lock {
        /// Path to the (private) assignment repo. Defaults to the current
        /// directory.
        #[arg(long, default_value = ".")]
        assignment: PathBuf,
    },
    /// Build the offline vendor dir + base image for an assignment.
    Vendor {
        /// Path to the assignment repo. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        assignment: PathBuf,
    },
    /// Invite every roster student to a GitHub org and add them to one of
    /// its teams. Add-only: nobody is ever removed.
    Register {
        /// Roster CSV: `github_user,student_id,...`, where `github_user`
        /// is the student's GitHub handle.
        #[arg(long)]
        roster: PathBuf,
        /// The GitHub org to invite students into.
        #[arg(long)]
        org: String,
        /// The team within `--org` to add them to, by slug or display
        /// name. Must already exist.
        #[arg(long)]
        team: String,
        /// Skip the confirmation prompt. Required when stdout isn't a
        /// terminal, since registering emails real people.
        #[arg(long)]
        yes: bool,
    },
    /// Fetch submissions: every fork of the assignment repo a roster
    /// student can push to.
    Fetch {
        /// Path to the (private) assignment repo.
        #[arg(long)]
        assignment: PathBuf,
        /// The public assignment repo students forked, as `owner/name`.
        #[arg(long)]
        repo: submissions::forks::Upstream,
        /// Roster CSV: `github_user,student_id,...`, where `github_user`
        /// is the student's GitHub handle and any further columns are
        /// carried into the fetch record.
        #[arg(long)]
        roster: PathBuf,
        /// Destination directory
        #[arg(long)]
        out: PathBuf,
        /// Override the deadline used for push-time commit selection
        /// ("<datetime>[<IANA zone>]", e.g. "2026-02-14T23:59:59[America/Santiago]")
        #[arg(long)]
        as_of: Option<jiff::Zoned>,
        #[command(flatten)]
        detach: DetachFlag,
        /// How many submissions to fetch at once. Lower it if GitHub
        /// starts refusing the concurrent requests.
        #[arg(short = 'j', long, default_value = "8")]
        jobs: std::num::NonZeroUsize,
    },
    /// Evaluate submissions
    Evaluate {
        /// Path to the (private) assignment repo.
        #[arg(long)]
        assignment: PathBuf,
        /// Directory containing submissions
        #[arg(long)]
        submissions: PathBuf,
        /// Grade using the host-process "local sandbox" instead of podman
        #[arg(long)]
        local_sandbox: bool,
    },
    /// Student-facing: run public tests only, advisory.
    Ci {
        /// Grade using the host-process "local sandbox" instead of podman.
        #[arg(long)]
        local_sandbox: bool,
    },
    /// Compute scores from persisted `evaluate` results and writes
    /// applies the current scoring policy fresh every time, and writes a
    /// gradebook CSV to `<submissions>/.grades/grades.csv`.
    Grade {
        /// Path to the assignment repo (for the current scoring policy).
        #[arg(long)]
        assignment: PathBuf,
        /// The same directory `autograder evaluate --submissions` was
        /// pointed at.
        #[arg(long)]
        submissions: PathBuf,
    },
    /// Print one submission's persisted fetch record (if any) and latest
    /// evaluate run (if any). Read-only -- never re-runs Fetch or
    /// Evaluate.
    Show {
        /// Path to a submission checkout dir, e.g. `<submissions>/alice`
        /// (the same layout `autograder fetch --out`/`evaluate
        /// --submissions` use) -- `.fetch/`/`.eval/` records are looked up
        /// as its siblings.
        submission: PathBuf,
    },
    /// Publish either the starter or solution repo from the private
    /// instructor workspace. Asks which of the two unless `--mode` says.
    Publish {
        /// Path to the private instructor workspace. Defaults to the
        /// current directory.
        #[arg(long, default_value = ".")]
        assignment: PathBuf,
        /// Output directory for the published tree.
        #[arg(long)]
        out: PathBuf,
        /// Which view of the private workspace to publish. Asked at a
        /// prompt when omitted.
        #[arg(long, value_enum)]
        mode: Option<PublishModeArg>,
    },
    /// Create a private GitHub repo for a published tree and push it
    /// there. Prompts for the semester and whether the tree is the starter
    /// or the solution.
    Push {
        #[arg(default_value = ".")]
        assignment: PathBuf,
    },
}

/// `--detach` / `--no-detach`, as one tri-state answer.
#[derive(Debug, clap::Args)]
pub struct DetachFlag {
    /// Remove each submission's `.git` after checkout, leaving plain
    /// directories.
    #[arg(long, overrides_with = "no_detach")]
    detach: bool,
    /// Keep each submission's `.git`. Default when not fetching inside
    /// a git repo.
    #[arg(long, overrides_with = "detach")]
    no_detach: bool,
}

impl DetachFlag {
    /// `None` when neither flag was passed -- the state `fetch` resolves
    /// by asking, or by deciding for itself when there's no terminal.
    pub fn choice(&self) -> Option<bool> {
        match (self.detach, self.no_detach) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    }
}

/// Mirrors `package::publish::PublishMode`, kept separate so `package`
/// stays free of a CLI-parsing dependency.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PublishModeArg {
    Starter,
    Solution,
}

impl From<PublishModeArg> for PublishMode {
    fn from(mode: PublishModeArg) -> Self {
        match mode {
            PublishModeArg::Starter => PublishMode::Starter,
            PublishModeArg::Solution => PublishMode::Solution,
        }
    }
}
