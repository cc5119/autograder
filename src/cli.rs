use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::id::AssignmentId;
use crate::package::publish::PublishMode;
use crate::spec::AssignmentKind;

#[derive(Debug, Parser)]
#[command(name = "autograder", version, about = "Rust assignment autograder")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold an assignment.
    Init {
        /// Directory to create the new workspace.
        dir: PathBuf,
        /// The assignment kind.
        #[arg(long, value_enum)]
        kind: AssignmentKindArg,
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
    Prefetch {
        /// Path to the assignment repo.
        assignment: PathBuf,
    },
    /// Run the Fetch stage alone: lands each submission at
    /// `<out>/<student_id>/` and records the outcome at
    /// `<out>/.meta/<student_id>.json`, without running
    /// Prepare/Evaluate/Grade.
    Fetch {
        /// Path to the (private) assignment repo.
        assignment: PathBuf,
        /// Roster CSV: `student_id,repo_url,ref,...`.
        #[arg(long)]
        roster: PathBuf,
        /// Destination directory for fetched submissions (flat: one
        /// `<student_id>/` subdirectory per student, plus a `.meta/`
        /// directory of per-student fetch records).
        #[arg(long)]
        out: PathBuf,
        /// Override the deadline used for push-time commit selection
        /// ("<datetime>[<IANA zone>]", e.g. "2026-02-14T23:59:59[America/Santiago]")
        #[arg(long)]
        as_of: Option<jiff::Zoned>,
    },
    /// Run Prepare -> Evaluate and persist raw results, one directory
    /// regardless of how its submissions were fetched. Never fetches itself
    /// and never scores -- run `autograder grade` afterwards for that.
    Evaluate {
        /// Path to the (private) assignment repo.
        assignment: PathBuf,
        /// Directory shaped like `autograder fetch --out`'s own output:
        /// `<student_id>/<assignment.id>/...` per student, plus
        /// `.meta/<student_id>.json` fetch records.
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
    /// Compute scores from persisted `evaluate` results: applies the
    /// current scoring policy fresh every time.
    Grade {
        /// Assignment id to grade.
        assignment_id: AssignmentId,
        /// Path to the assignment repo (for the current scoring policy).
        #[arg(long)]
        assignment: PathBuf,
    },
    /// Emit a report from persisted grades.
    Report {
        /// Assignment id to report on.
        assignment_id: AssignmentId,
        /// Output format.
        #[arg(long, value_enum, default_value = "json")]
        format: ReportFormat,
        /// Output path (defaults to stdout).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Publish either the starter/template repo (for distribution to
    /// students) or the solution repo (real implementations, kept out of
    /// the harness) from the private instructor workspace.
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
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ReportFormat {
    Json,
    Csv,
}

/// Mirrors `spec::AssignmentKind`, kept separate so `spec` stays free of a
/// CLI-parsing dependency.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AssignmentKindArg {
    Library,
    Binary,
}

impl From<AssignmentKindArg> for AssignmentKind {
    fn from(kind: AssignmentKindArg) -> Self {
        match kind {
            AssignmentKindArg::Library => AssignmentKind::Library,
            AssignmentKindArg::Binary => AssignmentKind::Binary,
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
