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

use crate::error::{Error, Result};

/// Recursively sums the byte size of every regular file under `dir`.
/// `dir` not existing is `0`, not an error -- a job that never got far
/// enough to write anything hasn't exceeded any quota.
pub fn directory_size_bytes(dir: &Path) -> Result<u64> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    let entries = std::fs::read_dir(dir).map_err(|source| Error::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        if file_type.is_dir() {
            total += directory_size_bytes(&entry.path())?;
        } else if file_type.is_file() {
            let metadata = entry.metadata().map_err(|source| Error::Io {
                path: entry.path(),
                source,
            })?;
            total += metadata.len();
        }
    }
    Ok(total)
}

/// Whether `dir`'s total on-disk size exceeds `quota_bytes`.
pub fn exceeds_quota(dir: &Path, quota_bytes: u64) -> Result<bool> {
    Ok(directory_size_bytes(dir)? > quota_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_directory_has_zero_size() {
        assert_eq!(
            directory_size_bytes(Path::new("/nonexistent/for/sure")).unwrap(),
            0
        );
    }

    #[test]
    fn sums_file_sizes_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.bin"), vec![0u8; 50]).unwrap();

        assert_eq!(directory_size_bytes(dir.path()).unwrap(), 150);
    }

    #[test]
    fn exceeds_quota_compares_against_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![0u8; 100]).unwrap();

        assert!(exceeds_quota(dir.path(), 50).unwrap());
        assert!(!exceeds_quota(dir.path(), 500).unwrap());
    }
}
