//! Publishes the starter/template repo for distribution to students from
//! the **private instructor package** in one pass: copy everything real,
//! then strip the sensitive parts in place. No hand-maintained `public/`
//! sibling repo.
//!
//! A test's presence in the published package *is* its visibility -- there
//! is no separate visibility flag or declared test list to consult.
//! [`strip_stub`] (already used for `src/**`) strips `harness/tests/**` and
//! `{id}/tests/**` too, via the exact same `keep`/`stub`/`hide` doc-comment
//! convention `crate::stub` applies to ordinary items: an unmarked
//! `#[test]` fn is private and non-`main`, so it's dropped by default like
//! any other unmarked private item, and only ships when the instructor
//! marks it `keep` or `stub`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ignore::overrides::OverrideBuilder;

use crate::error::{Error, Result};
use crate::fs;
use crate::spec::{self, AssignmentKind, Spec};

#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub out_dir: PathBuf,
}

struct PublishCtx {
    package_dir: PathBuf,
    id: String,
}

struct MatchedFile {
    rel_path: PathBuf,
    content: String,
}

type FileHook = fn(path: &str, file: MatchedFile, ctx: &PublishCtx) -> Result<MatchedFile>;

type GlobHook =
    fn(pattern: &str, matches: Vec<MatchedFile>, ctx: &PublishCtx) -> Result<Vec<MatchedFile>>;

enum Rule {
    File(&'static str, Option<FileHook>),
    Glob(&'static str, Option<GlobHook>),
}

fn rules(kind: AssignmentKind) -> Vec<Rule> {
    match kind {
        AssignmentKind::Library => vec![
            Rule::File("Cargo.toml", None),
            Rule::File("{id}/Cargo.toml", Some(validate_manifest)),
            Rule::Glob("{id}/src/**", Some(strip_stub)),
            Rule::File("harness/Cargo.toml", None),
            Rule::Glob("harness/src/**", None),
            Rule::Glob("harness/tests/**", Some(strip_stub)),
        ],
        AssignmentKind::Binary => vec![
            Rule::File("Cargo.toml", None),
            Rule::File("{id}/Cargo.toml", Some(validate_manifest)),
            Rule::Glob("{id}/src/**", Some(strip_stub)),
            Rule::Glob("{id}/tests/**", Some(strip_stub)),
        ],
    }
}

pub fn publish(package_dir: &Path, out_dir: &Path) -> Result<PublishOutcome> {
    let private_spec_path = package_dir.join(spec::PRIVATE_SPEC_FILE);
    if !private_spec_path.is_file() {
        return Err(Error::InvalidSpec(format!(
            "publish requires {} in {} (the private instructor package)",
            spec::PRIVATE_SPEC_FILE,
            package_dir.display()
        )));
    }
    let spec = Spec::load_file(&private_spec_path)?;
    let id = spec.assignment.id.clone();

    let ctx = PublishCtx {
        package_dir: package_dir.to_path_buf(),
        id: id.clone(),
    };

    copy_matching(&ctx, out_dir, &rules(spec.assignment.kind))?;

    let student_dir = out_dir.join(&id);
    run_cargo_fix(&student_dir)?;

    let public_spec_path = out_dir.join(spec::PUBLIC_SPEC_FILE);
    fs::copy(&private_spec_path, &public_spec_path)?;

    let workflow_dir = out_dir.join(".github/workflows");
    fs::create_dir_all(&workflow_dir)?;
    let workflow_path = workflow_dir.join("autograde.yml");
    let workflow_yaml = autograde_workflow_yaml(&spec.sandbox.image)?;
    fs::write(&workflow_path, workflow_yaml)?;

    Ok(PublishOutcome {
        out_dir: out_dir.to_path_buf(),
    })
}

fn copy_matching(ctx: &PublishCtx, out_dir: &Path, rules: &[Rule]) -> Result<()> {
    let all_files = fs::walk_files(&ctx.package_dir)?;

    for rule in rules {
        let output_files = match rule {
            Rule::File(path, hook) => {
                let rel_path = PathBuf::from(path.replace("{id}", &ctx.id));
                if !ctx.package_dir.join(&rel_path).is_file() {
                    return Err(Error::InvalidSpec(format!(
                        "publish requires {} under {} (the private instructor package) -- \
                         there is nothing to copy the starter's `{}` from",
                        rel_path.display(),
                        ctx.package_dir.display(),
                        rel_path.display()
                    )));
                }
                let file = read_file(&ctx.package_dir, rel_path)?;
                match hook {
                    Some(hook) => vec![hook(path, file, ctx)?],
                    None => vec![file],
                }
            }
            Rule::Glob(pattern, hook) => {
                let pattern = pattern.replace("{id}", &ctx.id);
                let override_ = OverrideBuilder::new(&ctx.package_dir)
                    .add(&pattern)
                    .unwrap()
                    .build()
                    .unwrap();

                let mut matches = Vec::new();
                for rel_path in &all_files {
                    if override_.matched(rel_path, false).is_whitelist() {
                        matches.push(read_file(&ctx.package_dir, rel_path.clone())?);
                    }
                }

                match hook {
                    Some(hook) => hook(&pattern, matches, ctx)?,
                    None => matches,
                }
            }
        };

        for file in output_files {
            write_file(out_dir, file)?;
        }
    }

    Ok(())
}

fn read_file(package_dir: &Path, rel_path: PathBuf) -> Result<MatchedFile> {
    let full_path = package_dir.join(&rel_path);
    let content = fs::read_to_string(&full_path)?;
    Ok(MatchedFile { rel_path, content })
}

fn write_file(out_dir: &Path, file: MatchedFile) -> Result<()> {
    let dst = out_dir.join(&file.rel_path);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dst, file.content)
}

fn validate_manifest(path: &str, file: MatchedFile, ctx: &PublishCtx) -> Result<MatchedFile> {
    let manifest_path = ctx.package_dir.join(path);
    let value: toml::Value = toml::from_str(&file.content).map_err(|source| Error::Toml {
        path: manifest_path.clone(),
        source: Box::new(source),
    })?;
    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    if package_name != Some(ctx.id.as_str()) {
        return Err(Error::InvalidSpec(format!(
            "{} has [package].name = {:?}, expected {:?} to match [assignment].id -- \
             rename the package, or the solution directory/id, so they agree",
            manifest_path.display(),
            package_name.unwrap_or("<missing>"),
            ctx.id
        )));
    }
    Ok(file)
}

fn strip_stub(
    _pattern: &str,
    matches: Vec<MatchedFile>,
    _ctx: &PublishCtx,
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
/// removes the `target/` dir it leaves behind.
fn run_cargo_fix(student_dir: &Path) -> Result<()> {
    let output = std::process::Command::new("cargo")
        .args(["fix", "--allow-dirty", "--allow-staged", "--allow-no-vcs"])
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

    const PRIVATE_SPEC: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"


[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]
serde = "1"
rand  = "0.8"

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
"#;

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
        write(&package_dir.join(spec::PRIVATE_SPEC_FILE), PRIVATE_SPEC);
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

        assert!(outcome.out_dir.join(spec::PUBLIC_SPEC_FILE).is_file());
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
    fn publish_derives_a_public_spec_matching_the_private_spec() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let public_spec =
            std::fs::read_to_string(out_dir.path().join(spec::PUBLIC_SPEC_FILE)).unwrap();
        assert_eq!(public_spec, PRIVATE_SPEC);
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
        write(
            &package_dir.path().join(spec::PRIVATE_SPEC_FILE),
            PRIVATE_SPEC,
        );
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

    const BINARY_PRIVATE_SPEC: &str = r#"
[assignment]
id = "wc"
name = "Word count"
kind = "binary"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"

[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]

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
"#;

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
        write(
            &package_dir.join(spec::PRIVATE_SPEC_FILE),
            BINARY_PRIVATE_SPEC,
        );
        write(
            &package_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"wc\"]\n",
        );
        write(
            &package_dir.join("wc/Cargo.toml"),
            "[package]\nname = \"wc\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(
            &package_dir.join("wc/src/main.rs"),
            "pub fn count(s: &str) -> usize { s.split_whitespace().count() }\nfn main() {}\n",
        );
        write(&package_dir.join("wc/tests/judge.rs"), BINARY_JUDGE_RS);
    }

    #[test]
    fn publish_derives_a_public_binary_judge_with_no_separate_harness_dir() {
        let package_dir = tempfile::tempdir().unwrap();
        write_binary_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(out_dir.path().join("wc/tests/judge.rs")).unwrap();
        assert!(judge.contains("fn counts_words"));
        assert!(!judge.contains("counts_zero_for_empty_input"));

        assert!(!out_dir.path().join("harness").exists());
        assert_eq!(
            std::fs::read_to_string(out_dir.path().join("Cargo.toml")).unwrap(),
            "[workspace]\nmembers = [\"wc\"]\n"
        );
    }

    #[test]
    fn publish_never_stubs_the_binary_judges_test_bodies() {
        let package_dir = tempfile::tempdir().unwrap();
        write_binary_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(out_dir.path().join("wc/tests/judge.rs")).unwrap();
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
