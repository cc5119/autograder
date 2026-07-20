//! Per-job disk quota enforcement. A real kernel-enforced quota needs host
//! setup this binary can't provision itself, so this module provides a
//! portable fallback instead: measure a job directory's actual disk usage
//! and flag it if it has grown past a cap, so a runaway build/test can be
//! failed the same way an OOM is, rather than silently exhausting host
//! disk across a whole batch.

use std::path::Path;

use crate::error::Result;
use crate::exec::fs::directory_size_bytes;

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
