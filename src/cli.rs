use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::id::AssignmentId;
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
    /// `<student_id>/checkout/` under the storage dir and records the
    /// outcome, without running Prepare/Evaluate/Grade.
    Fetch {
        /// Path to the (private) assignment repo.
        assignment: PathBuf,
        /// Where submissions come from: a roster CSV file, or a directory
        /// with one subdirectory per student. The kind is inferred from
        /// whether the path is a file or a directory.
        #[arg(long)]
        submissions: PathBuf,
        /// Override the deadline used for push-time commit selection
        /// ("<datetime>[<IANA zone>]", e.g. "2026-02-14T23:59:59[America/Santiago]")
        #[arg(long)]
        as_of: Option<jiff::Zoned>,
    },
    /// Run Prepare -> Evaluate -> Grade -> Report. By default reuses
    /// submissions pre-fetched with  `autograder fetch`.
    Grade {
        /// Path to the (private) assignment repo.
        assignment: PathBuf,
        /// Where submissions come from: a roster CSV file, or a directory
        /// with one subdirectory per student. The kind is inferred from
        /// whether the path is a file or a directory.
        #[arg(long)]
        submissions: PathBuf,
        /// Run the Fetch stage first, before grading (equivalent to
        /// `autograder fetch` followed by `autograder grade`).
        #[arg(long)]
        fetch: bool,
        /// Override the deadline used for push-time commit selection
        /// ("<datetime>[<IANA zone>]", e.g. "2026-02-14T23:59:59[America/Santiago]")
        #[arg(long, requires = "fetch")]
        as_of: Option<jiff::Zoned>,
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
    /// Re-run only the Grade stage from persisted results.
    Regrade {
        /// Assignment id to regrade.
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
    /// Publish the starter/template repo for distribution to students.
    Publish {
        /// Path to the private instructor workspace.
        assignment: PathBuf,
        /// Output directory for the starter template.
        #[arg(long)]
        out: PathBuf,
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
