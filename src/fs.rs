//! Thin wrappers around `std::fs` that attach the path to the resulting
//! error automatically. `std::io::Error` never carries the path itself --
//! it's just an OS error code / `ErrorKind` -- so a bare `std::fs` call
//! reports "No such file or directory (os error 2)" with no indication of
//! *which* file. Call these instead of `std::fs::*` directly anywhere a
//! `crate::error::Result` is expected; each one is the same signature as
//! its `std::fs` counterpart, just returning `Error::Io { path, source }`
//! on failure instead of a bare `io::Error`.

use std::fs::{DirEntry, FileType, Metadata, Permissions, ReadDir};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub fn read_to_string(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| io_err(path, source))
}

pub fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| io_err(path, source))
}

pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    std::fs::write(path, contents).map_err(|source| io_err(path, source))
}

/// `std::env::current_dir()`, with the error reported against `.` -- the
/// same shape of problem as every other bare `io::Error` in this module,
/// just from `std::env` rather than `std::fs`.
pub fn current_dir() -> Result<std::path::PathBuf> {
    std::env::current_dir().map_err(|source| io_err(Path::new("."), source))
}

pub fn create_dir(path: &Path) -> Result<()> {
    std::fs::create_dir(path).map_err(|source| io_err(path, source))
}

pub fn create_dir_all(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| io_err(path, source))
}

pub fn remove_dir_all(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).map_err(|source| io_err(path, source))
}

pub fn remove_file(path: &Path) -> Result<()> {
    std::fs::remove_file(path).map_err(|source| io_err(path, source))
}

/// Reports the error against `dst` if the copy fails -- the more
/// actionable path in practice (permission denied, missing parent
/// directory, disk full all manifest on the write side; a missing `src`
/// would already have failed the directory walk that found it).
pub fn copy(src: &Path, dst: &Path) -> Result<u64> {
    std::fs::copy(src, dst).map_err(|source| io_err(dst, source))
}

pub fn set_permissions(path: &Path, perms: Permissions) -> Result<()> {
    std::fs::set_permissions(path, perms).map_err(|source| io_err(path, source))
}

pub fn read_dir(path: &Path) -> Result<ReadDir> {
    std::fs::read_dir(path).map_err(|source| io_err(path, source))
}

/// Reads every entry of `dir` eagerly, reporting any midstream failure
/// against `dir` itself -- the OS gives no more specific path than that
/// for a failed readdir() step, since the failure isn't tied to one
/// particular entry. Replaces the common
/// `read_dir(dir)?.map(|e| e.map_err(...))` loop at each call site with a
/// single call.
pub fn read_dir_entries(dir: &Path) -> Result<Vec<DirEntry>> {
    read_dir(dir)?
        .map(|entry| entry.map_err(|source| io_err(dir, source)))
        .collect()
}

/// `entry.file_type()`, with the error reported against `entry.path()`.
pub fn file_type(entry: &DirEntry) -> Result<FileType> {
    entry
        .file_type()
        .map_err(|source| io_err(&entry.path(), source))
}

/// `entry.metadata()`, with the error reported against `entry.path()`.
pub fn entry_metadata(entry: &DirEntry) -> Result<Metadata> {
    entry
        .metadata()
        .map_err(|source| io_err(&entry.path(), source))
}

/// Whether `dir` exists and has no entries. Absent entirely -- not just
/// empty -- is *not* the same thing: callers that need to distinguish
/// "doesn't exist" from "exists but empty" should check `dir.is_dir()`
/// separately first, since a missing `dir` fails here with `Error::Io`
/// rather than reading as `true`.
pub fn is_empty_dir(dir: &Path) -> Result<bool> {
    Ok(read_dir(dir)?.next().is_none())
}

/// Recursively copies `src`'s tree onto `dst`, path-for-path, overwriting
/// any file already at a given path but never removing one that isn't
/// present in `src`. `dst` is assumed to contain nothing at all for most
/// callers -- a freshly-created scratch directory that's never reused
/// across jobs -- so the overwrite behavior rarely matters in practice.
/// One caller does rely on it (see `pipeline::grade_batch`'s `binary`
/// judge overlay onto `workspace/tests/`, which wipes that directory with
/// `remove_dir_all` immediately beforehand precisely so this function's
/// "leaves unrelated files alone" behavior can't leave a stray
/// student-supplied file sitting there uncleared -- see that call site's
/// comment for why that matters for grading integrity, not just
/// tidiness).
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    create_dir_all(dst)?;
    for entry in read_dir_entries(src)? {
        let file_type = file_type(&entry)?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            copy(&entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Recursively collects every regular file under `root`, as paths relative
/// to `root` itself. A missing `root` yields an empty list rather than an
/// error -- the same "hasn't gotten far enough to have anything yet" logic
/// as [`directory_size_bytes`]'s zero-for-missing case.
pub fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_files_into(root, root, &mut out)?;
    Ok(out)
}

fn walk_files_into(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !current.is_dir() {
        return Ok(());
    }
    for entry in read_dir_entries(current)? {
        let path = entry.path();
        if file_type(&entry)?.is_dir() {
            walk_files_into(root, &path, out)?;
        } else {
            out.push(
                path.strip_prefix(root)
                    .expect("path is under root")
                    .to_path_buf(),
            );
        }
    }
    Ok(())
}

/// Recursively sums the byte size of every regular file under `dir`.
/// `dir` not existing is `0`, not an error -- a job that never got far
/// enough to write anything hasn't exceeded any quota.
pub fn directory_size_bytes(dir: &Path) -> Result<u64> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in read_dir_entries(dir)? {
        let file_type = file_type(&entry)?;
        if file_type.is_dir() {
            total += directory_size_bytes(&entry.path())?;
        } else if file_type.is_file() {
            total += entry_metadata(&entry)?.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_empty_dir_is_true_for_a_dir_with_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_empty_dir(dir.path()).unwrap());
    }

    #[test]
    fn is_empty_dir_is_false_once_something_is_in_it() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), "x");
        assert!(!is_empty_dir(dir.path()).unwrap());
    }

    #[test]
    fn copy_dir_all_recursively_copies_the_whole_tree() {
        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("a.txt"), "top-level");
        write(&src.path().join("sub/b.txt"), "nested");

        let dst = tempfile::tempdir().unwrap();
        copy_dir_all(src.path(), dst.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.path().join("a.txt")).unwrap(),
            "top-level"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("sub/b.txt")).unwrap(),
            "nested"
        );
    }

    #[test]
    fn copy_dir_all_overwrites_existing_files_but_leaves_others_alone() {
        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("a.txt"), "new");

        let dst = tempfile::tempdir().unwrap();
        write(&dst.path().join("a.txt"), "old");
        write(&dst.path().join("untouched.txt"), "still here");

        copy_dir_all(src.path(), dst.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.path().join("a.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("untouched.txt")).unwrap(),
            "still here"
        );
    }

    #[test]
    fn walk_files_returns_every_file_as_a_root_relative_path() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("a.txt"), "x");
        write(&root.path().join("sub/b.txt"), "y");

        let mut files = walk_files(root.path()).unwrap();
        files.sort();

        assert_eq!(
            files,
            vec![PathBuf::from("a.txt"), PathBuf::from("sub/b.txt")]
        );
    }

    #[test]
    fn walk_files_on_a_missing_root_is_empty_not_an_error() {
        assert_eq!(
            walk_files(Path::new("/nonexistent/for/sure")).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn directory_size_bytes_missing_directory_has_zero_size() {
        assert_eq!(
            directory_size_bytes(Path::new("/nonexistent/for/sure")).unwrap(),
            0
        );
    }

    #[test]
    fn directory_size_bytes_sums_file_sizes_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.bin"), vec![0u8; 50]).unwrap();

        assert_eq!(directory_size_bytes(dir.path()).unwrap(), 150);
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}
