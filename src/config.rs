use std::path::PathBuf;

use serde::Deserialize;

/// Host-wide configuration: credentials location, storage dir, default
/// limits, container-runtime choice. Grows as later milestones need it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Directory where persisted `EvaluationResult`s and other run state live.
    pub storage_dir: PathBuf,
    /// Seccomp profile passed to `podman run --security-opt seccomp=...`
    /// (design §10). Must exist on the podman host; base-image build-out
    /// and profile provisioning land in M4 (step 20).
    pub seccomp_profile: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storage_dir: PathBuf::from(".autograder-store"),
            seccomp_profile: PathBuf::from("/etc/autograder/seccomp.json"),
        }
    }
}
