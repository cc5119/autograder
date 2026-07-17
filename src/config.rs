use std::path::PathBuf;

use serde::Deserialize;

/// Host-wide configuration: credentials location, storage dir, default
/// limits, container-runtime choice. Grows as later milestones need it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Directory where persisted `EvaluationResult`s and other run state live.
    pub storage_dir: PathBuf,
    /// Number of concurrent grading jobs.
    pub jobs: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storage_dir: PathBuf::from(".autograder-store"),
            jobs: 1,
        }
    }
}
