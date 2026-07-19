//! Per-job target volume provisioning. `pipeline::grade_batch` already
//! gives each job a **fresh-per-student, disk-backed, never-shared**
//! directory under `config.storage_dir` (`job_root`/`workspace`/
//! `driver_dir` -- plain host directories, never a container `tmpfs`,
//! never reused across different students): that satisfies everything
//! asked of the volume *except* the size quota.
//!
//! A real, kernel-enforced quota (an XFS project quota, a loopback-mounted
//! per-job filesystem) needs host setup this binary can't provision itself
//! -- the same category of prerequisite as the podman base image
//! (`spec::Sandbox::image`) or the seccomp profile
//! (`config::Config::seccomp_profile`): provisioned once by whoever deploys
//! the grading host, not per job. What this module provides instead is the
//! portable fallback available without any of that: measure a job
//! directory's actual disk usage and flag it if it has grown past a cap,
//! so a runaway build/test (e.g. a student writing gigabytes of files) is
//! caught and can be failed the same way an OOM is, rather than silently
//! exhausting host disk across a whole batch.

use std::path::Path;

use crate::error::Result;
use crate::fs::directory_size_bytes;

/// Whether `dir`'s total on-disk size exceeds `quota_bytes`.
pub fn exceeds_quota(dir: &Path, quota_bytes: u64) -> Result<bool> {
    Ok(directory_size_bytes(dir)? > quota_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exceeds_quota_compares_against_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![0u8; 100]).unwrap();

        assert!(exceeds_quota(dir.path(), 50).unwrap());
        assert!(!exceeds_quota(dir.path(), 500).unwrap());
    }
}
