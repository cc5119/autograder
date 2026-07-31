use std::process::Command;

/// Embeds the commit `autograder --version` reports (see `cli::version_string`).
/// Falls back to `"unknown"` for each var independently if `git`/`.git`
/// isn't available (e.g. building from a source tarball) -- the build must
/// never fail because of this.
fn main() {
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
    println!("cargo:rerun-if-changed=.git/HEAD");

    set_env("AUTOGRADER_COMMIT_SHORT", &["rev-parse", "--short", "HEAD"]);
    set_env("AUTOGRADER_COMMIT_FULL", &["rev-parse", "HEAD"]);
    set_env(
        "AUTOGRADER_COMMIT_DATE",
        &["log", "-1", "--format=%cd", "--date=short"],
    );
}

fn set_env(var: &str, args: &[&str]) {
    let value = Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env={var}={value}");
}
