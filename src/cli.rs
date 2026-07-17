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
    /// Run the full Fetch -> Prepare -> Evaluate -> Grade -> Report pipeline.
    Grade {
        /// Path to the assignment repo.
        assignment: PathBuf,
        /// Where submissions come from: a roster CSV file, or a directory
        /// with one subdirectory per student. The kind is inferred from
        /// whether the path is a file or a directory.
        #[arg(long)]
        submissions: PathBuf,
        /// Number of concurrent grading jobs.
        #[arg(long)]
        jobs: Option<usize>,
        /// Override the deadline used for commit selection (RFC3339).
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Student-facing: run public tests only, advisory.
    Ci {
        /// Path to the vendored public harness dir.
        #[arg(long)]
        harness: PathBuf,
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
        /// Path to the assignment repo.
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
