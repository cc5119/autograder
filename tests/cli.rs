//! Subprocess-level smoke tests: runs the real compiled `autograder`
//! binary, not `autograder::package::init`/`autograder::package::publish` in-process. Exists
//! specifically to guard against regressions that only manifest with
//! **relative** paths as typed on a real command line -- `library_package`
//! (via `common`) and every other in-process test here use
//! `tempfile::tempdir()`, which is always absolute, so they structurally
//! can't catch that class of bug (see the `overlay::apply` relative-root
//! fix this test was written to pin down).

use crate::common::{autograder_bin, write};

#[test]
fn init_then_publish_with_relative_paths_and_matching_dir_and_id_names() {
    let cwd = tempfile::tempdir().unwrap();

    // Mirrors the exact reported regression: `autograder init --id hw0
    // --kind library hw0` followed by `autograder publish --out
    // hw0-starter hw0`, both with relative arguments from the shell's cwd,
    // and the package dir's own name coinciding with `--id`.
    let init_status = autograder_bin()
        .args(["init", "--id", "hw0", "--kind", "library", "hw0"])
        .current_dir(cwd.path())
        .status()
        .unwrap();
    assert!(init_status.success(), "autograder init failed");

    let publish_output = autograder_bin()
        .args(["publish", "--out", "hw0-starter", "hw0"])
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(
        publish_output.status.success(),
        "autograder publish failed: {}",
        String::from_utf8_lossy(&publish_output.stderr)
    );

    assert!(
        cwd.path().join("hw0-starter/hw0/src/lib.rs").is_file(),
        "hw0-starter/hw0/src/lib.rs should have been copied by publish"
    );
}

#[test]
fn init_rejects_writing_into_a_nonempty_directory_via_the_real_binary() {
    let cwd = tempfile::tempdir().unwrap();
    write(&cwd.path().join("hw1/existing.txt"), "content");

    let output = autograder_bin()
        .args(["init", "--id", "hw1", "--kind", "library", "hw1"])
        .current_dir(cwd.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
}
