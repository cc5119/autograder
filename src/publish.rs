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

/// Everything else about `publish` -- the actual end-to-end tree it
/// produces, stripping, real `cargo build`/`cargo test` on the output --
/// only touches its public API and lives in `tests/publish.rs` as an
/// integration test instead (see that file's doc comment). These stay
/// here because `validate_manifest`/`strip_stub` are private: there's no
/// way to reach them from outside the crate.
#[cfg(test)]
mod tests {
    use super::*;

    fn matched_file(rel_path: &str, content: &str) -> MatchedFile {
        MatchedFile {
            rel_path: rel_path.into(),
            content: content.to_string(),
        }
    }

    fn ctx_with_id(id: &str) -> Context {
        Context {
            source_root: PathBuf::new(),
            substitutions: HashMap::from([("id", id.to_string())]),
        }
    }

    #[test]
    fn validate_manifest_accepts_a_matching_package_name() {
        let file = matched_file("hw3/Cargo.toml", "[package]\nname = \"hw3\"\n");
        let ctx = ctx_with_id("hw3");

        let result = validate_manifest("hw3/Cargo.toml", file, &ctx).unwrap();
        assert_eq!(result.content, "[package]\nname = \"hw3\"\n");
    }

    #[test]
    fn validate_manifest_rejects_a_mismatched_package_name() {
        let file = matched_file("hw3/Cargo.toml", "[package]\nname = \"wrong-name\"\n");
        let ctx = ctx_with_id("hw3");

        let err = validate_manifest("hw3/Cargo.toml", file, &ctx).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
        assert!(err.to_string().contains("wrong-name"));
    }

    #[test]
    fn validate_manifest_rejects_a_manifest_missing_the_package_table() {
        let file = matched_file("hw3/Cargo.toml", "[dependencies]\n");
        let ctx = ctx_with_id("hw3");

        let err = validate_manifest("hw3/Cargo.toml", file, &ctx).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
        assert!(err.to_string().contains("<missing>"));
    }

    #[test]
    fn validate_manifest_rejects_a_package_table_missing_the_name_key() {
        let file = matched_file("hw3/Cargo.toml", "[package]\nversion = \"0.0.0\"\n");
        let ctx = ctx_with_id("hw3");

        let err = validate_manifest("hw3/Cargo.toml", file, &ctx).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn strip_stub_strips_rust_source_but_leaves_other_files_untouched() {
        let ctx = ctx_with_id("hw3");
        let matches = vec![
            matched_file(
                "hw3/src/lib.rs",
                "/// autograder: keep\npub fn kept() {}\nfn private() {}\n",
            ),
            matched_file("hw3/Cargo.toml", "[package]\nname = \"hw3\"\n"),
        ];

        let stripped = strip_stub("hw3/src/**", matches, &ctx).unwrap();

        let rs = stripped.iter().find(|f| f.rel_path.ends_with("lib.rs")).unwrap();
        assert!(rs.content.contains("pub fn kept"));
        assert!(!rs.content.contains("fn private"));

        let toml = stripped
            .iter()
            .find(|f| f.rel_path.ends_with("Cargo.toml"))
            .unwrap();
        assert_eq!(toml.content, "[package]\nname = \"hw3\"\n");
    }
}
