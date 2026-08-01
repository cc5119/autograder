use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::package::publish::PublishMode;

#[derive(Debug, Parser)]
#[command(name = "autograder", about = "Rust assignment autograder")]
pub struct Cli {
    /// Print the commit this binary was built from (and its date) and exit.
    #[arg(short = 'V', long)]
    pub version: bool,
    /// With `--version`, show the full commit hash instead of the short
    /// one; with `show`, print full diagnostics instead of a one-line
    /// status/label.
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
        /// Path to the (private) assignment repo.
        assignment: PathBuf,
    },
    /// Build the offline vendor dir + base image for an assignment.
    Vendor {
        /// Path to the assignment repo.
        assignment: PathBuf,
    },
    /// Fetch submissions: every fork of the assignment repo a roster
    /// student can push to.
    Fetch {
        /// Path to the (private) assignment repo.
        #[arg(long)]
        assignment: PathBuf,
        /// The public assignment repo students forked, as `owner/name`.
        #[arg(long)]
        repo: crate::submissions::forks::Upstream,
        /// Roster CSV: `student_id,university_id,...`, where `student_id`
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
        /// Skip the confirmation prompt. Required when stdout isn't a
        /// terminal, since fetching overwrites existing checkouts.
        #[arg(long)]
        yes: bool,
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
    /// Publish either the starter or solution repo from the private instructor workspace.
    Publish {
        /// Path to the private instructor workspace.
        assignment: PathBuf,
        /// Output directory for the published tree.
        #[arg(long)]
        out: PathBuf,
        /// Which view of the private workspace to publish.
        #[arg(long, value_enum, default_value = "starter")]
        mode: PublishModeArg,
    },
    /// Create a private GitHub repo for a published starter tree and push
    /// it there.
    Push {
        /// Path to an already-published starter tree (`autograder
        /// publish`'s `--out`).
        dir: PathBuf,
    },
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
