use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "autograder", version, about = "Rust assignment autograder")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build the offline vendor dir + base image for an assignment.
    Prefetch {
        /// Path to the assignment repo.
        assignment: PathBuf,
    },
    /// Run the Fetch stage alone: lands each submission at
    /// `<student_id>/checkout/` under the storage dir and records the
    /// outcome, without running Prepare/Evaluate/Grade. `grade --fetch`
    /// runs this same stage first; running it separately is what lets a
    /// later plain `grade` (no `--fetch`) redo just Prepare/Evaluate/Grade
    /// against what's already on disk, with no network access needed.
    Fetch {
        /// Path to the assignment repo.
        assignment: PathBuf,
        /// Where submissions come from: a roster CSV file, or a directory
        /// with one subdirectory per student. The kind is inferred from
        /// whether the path is a file or a directory.
        #[arg(long)]
        submissions: PathBuf,
        /// Override the deadline used for push-time commit selection
        /// (RFC3339) -- see `[assignment].deadline`.
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Run Prepare -> Evaluate -> Grade -> Report. By default reuses
    /// whatever a prior `autograder fetch` (or an earlier `grade --fetch`)
    /// already landed on disk -- pass `--fetch` to run the Fetch stage
    /// first, in the same command.
    Grade {
        /// Path to the assignment repo.
        assignment: PathBuf,
        /// Where submissions come from: a roster CSV file, or a directory
        /// with one subdirectory per student. The kind is inferred from
        /// whether the path is a file or a directory.
        #[arg(long)]
        submissions: PathBuf,
        /// Run the Fetch stage first, before grading (equivalent to
        /// `autograder fetch` followed by `autograder grade`). Without
        /// this flag, grading reuses whatever the most recent fetch left
        /// on disk and never touches the network.
        #[arg(long)]
        fetch: bool,
        /// Override the deadline used for push-time commit selection
        /// (RFC3339) -- only meaningful together with `--fetch`.
        #[arg(long)]
        as_of: Option<String>,
        /// Grade using the host-process `LocalSandbox` instead of the
        /// `ContainerSandbox` (skips Podman entirely). This drops the
        /// container isolation the authoritative tier relies on for
        /// untrusted student code (design §10) -- only for local
        /// development/testing on a host without a working Podman, never
        /// for grading real submissions.
        #[arg(long)]
        local_sandbox: bool,
    },
    /// Student-facing: run public tests only, advisory. Run from the repo
    /// root produced by `scaffold` (where `autograder.public.toml` and
    /// `harness/` live) -- the student's own crate is found at
    /// `<[assignment].id>/`, a sibling directory named after the spec's id.
    Ci {
        /// Run using the host-process `LocalSandbox` instead of the
        /// `ContainerSandbox` (skips Podman entirely). Same tradeoff as
        /// `grade --local-sandbox` -- only for local development/testing on
        /// a host without a working Podman, never for real CI runs.
        #[arg(long)]
        local_sandbox: bool,
    },
    /// Re-run only the Grade stage from persisted results.
    Regrade {
        /// Assignment id to regrade.
        assignment_id: String,
        /// Path to the assignment repo (for the current scoring policy).
        #[arg(long)]
        assignment: PathBuf,
    },
    /// Emit a report from persisted grades.
    Report {
        /// Assignment id to report on.
        assignment_id: String,
        /// Output format.
        #[arg(long, value_enum, default_value = "json")]
        format: ReportFormat,
        /// Output path (defaults to stdout).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Emit the starter/template repo for distribution to students.
    Scaffold {
        /// Path to the private instructor package (autograder.toml,
        /// harness/, and a reference solution directory named after
        /// [assignment].id).
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
