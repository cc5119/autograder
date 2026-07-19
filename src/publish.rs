//! Publishes the starter/template repo for distribution to students from
//! the **private instructor package** in one pass: copy everything real,
//! then strip the sensitive parts in place. No hand-maintained `public/`
//! sibling repo.
//!
//! A test's presence in the published package *is* its visibility -- there
//! is no separate visibility flag or declared test list to consult.
//! [`strip_stub`] (already used for `src/**`) strips `harness/tests/**`
//! too, via the exact same `keep`/`stub`/`hide` doc-comment convention
//! `crate::stub` applies to ordinary items: an unmarked `#[test]` fn is
//! private and non-`main`, so it's dropped by default like any other
//! unmarked private item, and only ships when the instructor marks it
//! `keep` or `stub`. The judge always lives in `harness/`, a sibling
//! package of `{id}`, for both `library` and `binary` (see
//! `evaluator::library`'s and `evaluator::binary`'s module doc comments).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fs;
use crate::overlay::{self, Context, MatchedFile, Rule};
use crate::spec::{self, Spec};

#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub out_dir: PathBuf,
}

fn rules() -> Vec<Rule> {
    vec![
        Rule::File("Cargo.toml", None),
        Rule::File("Cargo.lock", None),
        Rule::File(spec::SPEC_FILE, None),
        Rule::File("{id}/Cargo.toml", Some(validate_manifest)),
        Rule::Glob("{id}/src/**", Some(strip_stub)),
        Rule::File("{harness}/Cargo.toml", None),
        Rule::Glob("{harness}/src/**", None),
        Rule::Glob("{harness}/tests/**", Some(strip_stub)),
    ]
}

pub fn publish(package_dir: &Path, out_dir: &Path) -> Result<PublishOutcome> {
    let private_spec_path = package_dir.join(spec::SPEC_FILE);
    if !private_spec_path.is_file() {
        return Err(Error::InvalidSpec(format!(
            "publish requires {} in {} (the private instructor package)",
            spec::SPEC_FILE,
            package_dir.display()
        )));
    }
    let spec = Spec::load_file(&private_spec_path)?;
    let id = spec.assignment.id;

    // Refuses to ship a `[allowed-crates]`-equivalent that's gone stale --
    // `Cargo.lock`'s resolved dependency graph *is* the allowlist (see
    // `manifest_check`'s module doc comment), so if it doesn't match the
    // hash `autograder lock` last recorded, publishing would ship students
    // an allowlist inconsistent with what grading actually checks against.
    if let Some(message) = crate::lock::verify(package_dir, &spec) {
        return Err(Error::InvalidSpec(format!(
            "refusing to publish: {message}"
        )));
    }

    let ctx = Context {
        source_root: package_dir.to_path_buf(),
        substitutions: HashMap::from([
            ("id", id.to_string()),
            ("harness", spec.assignment.harness.clone()),
        ]),
    };

    overlay::apply(&ctx, out_dir, &rules())?;

    let student_dir = out_dir.join(id.as_str());
    run_cargo_fix(&student_dir)?;

    let workflow_dir = out_dir.join(".github/workflows");
    fs::create_dir_all(&workflow_dir)?;
    let workflow_path = workflow_dir.join("autograde.yml");
    let workflow_yaml = autograde_workflow_yaml(&spec.sandbox.image)?;
    fs::write(&workflow_path, workflow_yaml)?;

    Ok(PublishOutcome {
        out_dir: out_dir.to_path_buf(),
    })
}

fn validate_manifest(path: &str, file: MatchedFile, ctx: &Context) -> Result<MatchedFile> {
    let manifest_path = ctx.source_root.join(path);
    let value: toml::Value = toml::from_str(&file.content).map_err(|source| Error::Toml {
        path: manifest_path.clone(),
        source: Box::new(source),
    })?;
    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    let id = ctx
        .substitutions
        .get("id")
        .map(String::as_str)
        .unwrap_or_default();
    if package_name != Some(id) {
        return Err(Error::InvalidSpec(format!(
            "{} has [package].name = {:?}, expected {:?} to match [assignment].id -- \
             rename the package, or the solution directory/id, so they agree",
            manifest_path.display(),
            package_name.unwrap_or("<missing>"),
            id
        )));
    }
    Ok(file)
}

fn strip_stub(
    _pattern: &str,
    matches: Vec<MatchedFile>,
    _ctx: &Context,
) -> Result<Vec<MatchedFile>> {
    matches
        .into_iter()
        .map(|file| {
            if file.rel_path.extension().is_some_and(|ext| ext == "rs") {
                let stripped = crate::stub::strip_to_stub(&file.content)?;
                Ok(MatchedFile {
                    content: stripped,
                    ..file
                })
            } else {
                Ok(file)
            }
        })
        .collect()
}

/// The template's release coordinates (repo, version, sha256) are
/// themselves placeholders an instructor edits after publishing, once they
/// stand up their own fork/release -- only `{base_image}` is filled in here.
fn autograde_workflow_yaml(base_image: &str) -> Result<String> {
    crate::template::render_file(
        "autograde.yml",
        &HashMap::from([("base_image", base_image)]),
    )
}

/// Runs `cargo fix` to prune `use` lines stub-stripping left unused, then
/// removes the `target/` dir it leaves behind. `--locked` keeps it from
/// silently rewriting the just-shipped, blessed `Cargo.lock` (student
/// crate's own manifest dependencies are unchanged by stubbing, so the
/// existing lock still satisfies it).
fn run_cargo_fix(student_dir: &Path) -> Result<()> {
    let output = std::process::Command::new("cargo")
        .args([
            "fix",
            "--allow-dirty",
            "--allow-staged",
            "--allow-no-vcs",
            "--locked",
        ])
        .current_dir(student_dir)
        .output()
        .map_err(|source| Error::Io {
            path: student_dir.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "cargo fix failed while stripping the starter at {}:\n{}",
            student_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let target_dir = student_dir.join("target");
    if target_dir.is_dir() {
        fs::remove_dir_all(&target_dir)?;
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

    const HW3_LOCK: &str = "version = 4\n\n[[package]]\nname = \"driver\"\nversion = \"0.0.0\"\ndependencies = [\n \"hw3\",\n]\n\n[[package]]\nname = \"hw3\"\nversion = \"0.0.0\"\n";

    /// The private spec, with `cargo-lock-sha256` computed from `HW3_LOCK`
    /// -- matching what `write_instructor_package` writes to disk, exactly
    /// as `autograder lock` would leave things (see `crate::lock`).
    fn private_spec() -> String {
        format!(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{}"


[sandbox]
image = "autograder-base:1.86.0"

[limits.build]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256

[limits.run]
cpu-time = "5s"
wall-clock = "10s"
cpus = 1
memory = "512MiB"
pids = 128
max-output-bytes = "1MiB"

[scoring]
formula = "sum"
base = 0.0
"#,
            crate::cargo_lock::sha256_hex(HW3_LOCK)
        )
    }

    const HARNESS_MANIFEST: &str = "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nhw3 = { path = \"../hw3\" }\n";

    const JUDGE_RS: &str = r#"
        /// autograder: keep
        #[test]
        fn insert_basic() {
            assert!(true);
        }

        #[test]
        fn balance_adversarial() {
            assert!(true);
        }
    "#;

    fn write_solution_crate(solution_dir: &Path, package_name: &str) {
        write(
            &solution_dir.join("Cargo.toml"),
            &format!(
                "[package]\nname = \"{package_name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n"
            ),
        );
        write(
            &solution_dir.join("src/lib.rs"),
            r#"
                use std::collections::HashSet;

                pub struct Stack<T> {
                    items: Vec<T>,
                }

                impl<T> Stack<T> {
                    pub fn new() -> Self {
                        Stack { items: Vec::new() }
                    }

                    pub fn push(&mut self, value: T) {
                        self.items.push(value);
                    }

                    fn dedup_hint(&self) -> HashSet<usize> {
                        HashSet::new()
                    }
                }
            "#,
        );
    }

    fn write_instructor_package(package_dir: &Path) {
        write(&package_dir.join(spec::SPEC_FILE), &private_spec());
        write(&package_dir.join("Cargo.lock"), HW3_LOCK);
        write(
            &package_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"harness\", \"hw3\"]\n",
        );
        write(&package_dir.join("harness/Cargo.toml"), HARNESS_MANIFEST);
        write(&package_dir.join("harness/src/main.rs"), "fn main() {}\n");
        write(&package_dir.join("harness/tests/judge.rs"), JUDGE_RS);
        write_solution_crate(&package_dir.join("hw3"), "hw3");
    }

    #[test]
    fn publish_produces_the_documented_starter_tree() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());

        let out_dir = tempfile::tempdir().unwrap();
        let outcome = publish(package_dir.path(), out_dir.path()).unwrap();

        assert!(outcome.out_dir.join(spec::SPEC_FILE).is_file());
        assert!(outcome.out_dir.join("harness/tests/judge.rs").is_file());
        assert!(
            outcome
                .out_dir
                .join(".github/workflows/autograde.yml")
                .is_file()
        );
        assert!(outcome.out_dir.join("Cargo.toml").is_file());
        assert!(outcome.out_dir.join("hw3/Cargo.toml").is_file());
        assert!(outcome.out_dir.join("hw3/src/lib.rs").is_file());
    }

    #[test]
    fn publish_ships_the_spec_file_verbatim_under_the_same_name() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let shipped_spec = std::fs::read_to_string(out_dir.path().join(spec::SPEC_FILE)).unwrap();
        assert_eq!(shipped_spec, private_spec());
    }

    #[test]
    fn publish_ships_the_workspace_root_cargo_lock() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(out_dir.path().join("Cargo.lock")).unwrap(),
            HW3_LOCK
        );
    }

    #[test]
    fn publish_refuses_to_ship_a_stale_cargo_lock() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        // The instructor changed a dependency but forgot to rerun `autograder
        // lock` -- the checked-in Cargo.lock no longer matches the hash
        // recorded in autograder.toml.
        write(
            &package_dir.path().join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"hw3\"\nversion = \"9.9.9\"\n",
        );
        let out_dir = tempfile::tempdir().unwrap();

        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
        assert!(!out_dir.path().join(spec::SPEC_FILE).exists());
    }

    #[test]
    fn publish_derives_a_public_harness_with_only_the_kept_test_and_a_path_dependency() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(out_dir.path().join("harness/tests/judge.rs")).unwrap();
        assert!(judge.contains("fn insert_basic"));
        assert!(!judge.contains("balance_adversarial"));

        let manifest = std::fs::read_to_string(out_dir.path().join("harness/Cargo.toml")).unwrap();
        assert!(!manifest.contains("patch"));
        assert!(manifest.contains("path = \"../hw3\""));
    }

    #[test]
    fn publish_errors_clearly_when_the_solution_directory_is_missing() {
        let package_dir = tempfile::tempdir().unwrap();
        write(&package_dir.path().join(spec::SPEC_FILE), &private_spec());
        write(&package_dir.path().join("Cargo.lock"), HW3_LOCK);
        write(
            &package_dir.path().join("harness/Cargo.toml"),
            HARNESS_MANIFEST,
        );
        write(&package_dir.path().join("harness/tests/judge.rs"), JUDGE_RS);
        let out_dir = tempfile::tempdir().unwrap();

        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn emitted_workflow_runs_ci_from_the_repo_root_inside_podman() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let workflow =
            std::fs::read_to_string(out_dir.path().join(".github/workflows/autograde.yml"))
                .unwrap();
        assert!(workflow.contains("on:\n  push:\n    branches: [main]"));
        assert!(workflow.contains("sha256sum -c -"));
        assert!(workflow.contains("command -v podman"));
        assert!(workflow.contains("./autograder prefetch ."));
        assert!(workflow.contains("podman pull autograder-base:1.86.0"));
        assert!(workflow.contains("./autograder ci"));
        assert!(!workflow.contains("--harness"));
    }

    #[test]
    fn emitted_workspace_manifest_lists_the_harness_and_student_crate() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let workspace_manifest =
            std::fs::read_to_string(out_dir.path().join("Cargo.toml")).unwrap();
        assert_eq!(
            workspace_manifest,
            "[workspace]\nmembers = [\"harness\", \"hw3\"]\n"
        );
    }

    #[test]
    fn emitted_student_manifest_matches_the_solutions_own_cargo_toml() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let solution_manifest =
            std::fs::read_to_string(package_dir.path().join("hw3/Cargo.toml")).unwrap();
        let starter_manifest =
            std::fs::read_to_string(out_dir.path().join("hw3/Cargo.toml")).unwrap();
        assert_eq!(starter_manifest, solution_manifest);
    }

    #[test]
    fn publish_rejects_a_package_dir_without_a_private_spec() {
        let package_dir = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();

        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn publish_rejects_a_package_dir_without_a_root_workspace_manifest() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        std::fs::remove_file(package_dir.path().join("Cargo.toml")).unwrap();
        let out_dir = tempfile::tempdir().unwrap();

        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn publish_derives_a_building_stub_from_the_id_named_solution_dir() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());

        let out_dir = tempfile::tempdir().unwrap();
        publish(package_dir.path(), out_dir.path()).unwrap();

        let src = std::fs::read_to_string(out_dir.path().join("hw3/src/lib.rs")).unwrap();
        assert!(src.contains("pub struct Stack"));
        assert!(src.contains("pub fn new"));
        assert!(src.contains("pub fn push"));
        assert!(src.contains("todo!"));
        assert!(!src.contains("dedup_hint"));
        assert!(!src.contains("HashSet"));

        let build = std::process::Command::new("cargo")
            .arg("build")
            .current_dir(out_dir.path().join("hw3"))
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "published starter failed to build: {}",
            String::from_utf8_lossy(&build.stderr)
        );
    }

    #[test]
    fn cargo_test_at_the_starter_root_runs_the_public_harness() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());

        let out_dir = tempfile::tempdir().unwrap();
        publish(package_dir.path(), out_dir.path()).unwrap();

        let test = std::process::Command::new("cargo")
            .arg("test")
            .current_dir(out_dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&test.stdout);
        assert!(
            test.status.success(),
            "cargo test at the starter root failed: {}{}",
            stdout,
            String::from_utf8_lossy(&test.stderr)
        );
        assert!(stdout.contains("insert_basic"));
    }

    #[test]
    fn publish_rejects_a_solution_dir_whose_package_name_does_not_match_the_id() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        write_solution_crate(&package_dir.path().join("hw3"), "wrong-name");

        let out_dir = tempfile::tempdir().unwrap();
        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn publish_never_copies_a_vendor_directory_dropped_in_the_solution_crate() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        write(
            &package_dir.path().join("hw3/vendor/some-crate/src/lib.rs"),
            "not checked into the starter\n",
        );
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        assert!(!out_dir.path().join("hw3/vendor").exists());
    }

    // Unlike `library`, `binary`'s harness has no Cargo dependency edge to
    // `{id}` (see `evaluator::binary`'s module doc comment) -- the lock
    // must match that, or `cargo fix --locked` (run by `publish`) would
    // need to rewrite it.
    const WC_LOCK: &str = "version = 4\n\n[[package]]\nname = \"driver\"\nversion = \"0.0.0\"\n\n[[package]]\nname = \"wc\"\nversion = \"0.0.0\"\n";

    fn binary_private_spec() -> String {
        format!(
            r#"
[assignment]
id = "wc"
name = "Word count"
kind = "binary"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{}"

[sandbox]
image = "autograder-base:1.86.0"

[limits.build]
wall-clock = "60s"
cpus = 2
memory = "1GiB"
pids = 128

[limits.run]
cpu-time = "5s"
wall-clock = "10s"
cpus = 1
memory = "256MiB"
pids = 64
max-output-bytes = "64KiB"

[scoring]
formula = "sum"
base = 0.0
"#,
            crate::cargo_lock::sha256_hex(WC_LOCK)
        )
    }

    const BINARY_HARNESS_MANIFEST: &str =
        "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";

    const BINARY_JUDGE_RS: &str = r#"
        /// autograder: keep
        #[test]
        fn counts_words() {
            assert!(true);
        }

        #[test]
        fn counts_zero_for_empty_input() {
            assert!(true);
        }
    "#;

    fn write_binary_instructor_package(package_dir: &Path) {
        write(&package_dir.join(spec::SPEC_FILE), &binary_private_spec());
        write(&package_dir.join("Cargo.lock"), WC_LOCK);
        write(
            &package_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"harness\", \"wc\"]\n",
        );
        write(
            &package_dir.join("wc/Cargo.toml"),
            "[package]\nname = \"wc\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(
            &package_dir.join("wc/src/main.rs"),
            "pub fn count(s: &str) -> usize { s.split_whitespace().count() }\nfn main() {}\n",
        );
        write(
            &package_dir.join("harness/Cargo.toml"),
            BINARY_HARNESS_MANIFEST,
        );
        write(&package_dir.join("harness/src/main.rs"), "fn main() {}\n");
        write(&package_dir.join("harness/tests/judge.rs"), BINARY_JUDGE_RS);
    }

    #[test]
    fn publish_derives_a_public_binary_judge_alongside_a_separate_harness_dir() {
        let package_dir = tempfile::tempdir().unwrap();
        write_binary_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(out_dir.path().join("harness/tests/judge.rs")).unwrap();
        assert!(judge.contains("fn counts_words"));
        assert!(!judge.contains("counts_zero_for_empty_input"));

        assert_eq!(
            std::fs::read_to_string(out_dir.path().join("harness/Cargo.toml")).unwrap(),
            BINARY_HARNESS_MANIFEST
        );
        assert_eq!(
            std::fs::read_to_string(out_dir.path().join("Cargo.toml")).unwrap(),
            "[workspace]\nmembers = [\"harness\", \"wc\"]\n"
        );
    }

    #[test]
    fn publish_never_stubs_the_binary_judges_test_bodies() {
        let package_dir = tempfile::tempdir().unwrap();
        write_binary_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(out_dir.path().join("harness/tests/judge.rs")).unwrap();
        assert!(judge.contains("assert!(true)"));
        assert!(!judge.contains("todo!"));
    }

    #[test]
    fn cargo_test_at_the_binary_starter_root_runs_the_public_judge() {
        let package_dir = tempfile::tempdir().unwrap();
        write_binary_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();
        publish(package_dir.path(), out_dir.path()).unwrap();

        let test = std::process::Command::new("cargo")
            .arg("test")
            .current_dir(out_dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&test.stdout);
        assert!(
            test.status.success(),
            "cargo test at the binary starter root failed: {}{}",
            stdout,
            String::from_utf8_lossy(&test.stderr)
        );
        assert!(stdout.contains("counts_words"));
    }
}
