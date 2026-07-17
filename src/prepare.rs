use std::path::Path;

use crate::error::{Error, Result};
use crate::spec::{AssignmentKind, Spec};

/// How the student's checkout is wired to the harness, decided structurally
/// from `[assignment].kind`. Offline cargo env + manifest allowlist diff are
/// added in M2 (steps 12-13); this only identifies the wiring target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wiring {
    /// `linked-library`: a driver crate scaffold path-depends on the
    /// student's package (by `[student].package-name`).
    LinkedLibrary { driver_dir: std::path::PathBuf },
    /// `binary-harness`: the built binary target the judge will spawn.
    BinaryHarness { bin_name: String },
}

#[derive(Debug, Clone)]
pub struct PrepareOutcome {
    pub wiring: Wiring,
}

/// Assembles the workspace: student checkout (already at `workspace`) with
/// the instructor `harness/` and `fixtures/` overlaid on top — instructor
/// files win, and any student file at a matching path is replaced (design
/// §7.2). `package_dir` is the instructor assignment package containing
/// `harness/` and `fixtures/`.
pub fn prepare(workspace: &Path, package_dir: &Path, spec: &Spec) -> Result<PrepareOutcome> {
    let harness_dir = package_dir.join("harness");
    if harness_dir.is_dir() {
        overlay_dir(workspace, &harness_dir)?;
    }
    let fixtures_dir = package_dir.join("fixtures");
    if fixtures_dir.is_dir() {
        overlay_dir(&workspace.join("fixtures"), &fixtures_dir)?;
    }

    let wiring = match spec.assignment.kind {
        AssignmentKind::LinkedLibrary => {
            let driver_dir = workspace.join("driver");
            std::fs::create_dir_all(&driver_dir).map_err(|source| Error::Io {
                path: driver_dir.clone(),
                source,
            })?;
            Wiring::LinkedLibrary { driver_dir }
        }
        AssignmentKind::BinaryHarness => {
            let bin_name = spec.student.bin_name.clone().ok_or_else(|| {
                Error::InvalidSpec("binary-harness assignment missing [student].bin-name".into())
            })?;
            Wiring::BinaryHarness { bin_name }
        }
    };

    Ok(PrepareOutcome { wiring })
}

/// Copies `overlay_root`'s tree onto `dest`, path-for-path. Any existing
/// file at a destination path is removed first, so instructor files always
/// win over student files at the same path.
fn overlay_dir(dest: &Path, overlay_root: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|source| Error::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    let entries = std::fs::read_dir(overlay_root).map_err(|source| Error::Io {
        path: overlay_root.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: overlay_root.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: entry.path(),
            source,
        })?;
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            overlay_dir(&dest_path, &entry.path())?;
        } else if file_type.is_file() {
            if dest_path.exists() {
                std::fs::remove_file(&dest_path).map_err(|source| Error::Io {
                    path: dest_path.clone(),
                    source,
                })?;
            }
            std::fs::copy(entry.path(), &dest_path).map_err(|source| Error::Io {
                path: dest_path.clone(),
                source,
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn instructor_harness_replaces_student_file_at_same_path() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();

        write(&workspace.path().join("tests/foo.rs"), "student version");
        write(&package.path().join("harness/tests/foo.rs"), "instructor version");
        write(&package.path().join("harness/tests/hidden.rs"), "hidden test");

        overlay_dir(workspace.path(), &package.path().join("harness")).unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.path().join("tests/foo.rs")).unwrap(),
            "instructor version"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("tests/hidden.rs")).unwrap(),
            "hidden test"
        );
    }

    #[test]
    fn unrelated_student_files_are_left_alone() {
        let workspace = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();

        write(&workspace.path().join("src/lib.rs"), "student lib");
        write(&package.path().join("harness/tests/foo.rs"), "instructor version");

        overlay_dir(workspace.path(), &package.path().join("harness")).unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.path().join("src/lib.rs")).unwrap(),
            "student lib"
        );
    }
}
