//! Publishes the starter/template repo for distribution to students from
//! the **private instructor package** in one pass: copy everything real,
//! then strip the sensitive parts in place. No hand-maintained `public/`
//! sibling repo.
//!
//! [`strip_stub`] (already used for `src/**`) strips `harness/tests/**`
//! too, via the same convention `crate::package::stub` applies to ordinary
//! items: an unmarked `#[test]` fn ships by default, and only one stacked
//! with `#[cfg(not(feature = "student"))]` (alongside its own `#[cfg(test)]`,
//! since that alone isn't a directive -- see that module's doc comment) is
//! dropped, which is how an adversarial test stays hidden. The judge
//! always lives in `harness/`, a sibling package of `{id}`, for both
//! `library` and `binary` (see `evaluator::library`'s and
//! `evaluator::binary`'s module doc comments).
//!
//! Before any of that, [`check_student_view_is_clean`] compiles the
//! private repo's own `{id}` crate with `--features student` and refuses
//! to publish on any warning -- checked against the solution source
//! directly, rather than by running `cargo fix` over generated output the
//! way this used to work.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::overlay::{self, Context, MatchedFile, Rule};
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
        Rule::File(".gitignore", None),
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
    if let Some(message) = crate::deps::lock::verify(package_dir, &spec) {
        return Err(Error::InvalidSpec(format!(
            "refusing to publish: {message}"
        )));
    }

    // Checked ahead of the real compile below so these still get a clear
    // `InvalidSpec`, not an opaque `cargo check` failure standing in for it.
    let solution_dir = package_dir.join(id.as_str());
    if !solution_dir.is_dir() {
        return Err(Error::InvalidSpec(format!(
            "no {}/ solution directory found in {} -- [assignment].id must name a sibling \
             directory holding the reference solution crate",
            id.as_str(),
            package_dir.display()
        )));
    }
    let manifest_path = solution_dir.join("Cargo.toml");
    validate_package_name(
        &manifest_path,
        &fs::read_to_string(&manifest_path)?,
        id.as_str(),
    )?;

    check_student_view_is_clean(package_dir, id.as_str())?;

    let ctx = Context {
        source_root: package_dir.to_path_buf(),
        substitutions: HashMap::from([
            ("id", id.to_string()),
            ("harness", spec.assignment.harness.clone()),
        ]),
    };

    overlay::apply(&ctx, out_dir, &rules())?;

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
    let id = ctx
        .substitutions
        .get("id")
        .map(String::as_str)
        .unwrap_or_default();
    validate_package_name(&manifest_path, &file.content, id)?;
    Ok(file)
}

fn validate_package_name(manifest_path: &Path, content: &str, id: &str) -> Result<()> {
    let value: toml::Value = toml::from_str(content).map_err(|source| Error::Toml {
        path: manifest_path.to_path_buf(),
        source: Box::new(source),
    })?;
    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    if package_name != Some(id) {
        return Err(Error::InvalidSpec(format!(
            "{} has [package].name = {:?}, expected {:?} to match [assignment].id -- \
             rename the package, or the solution directory/id, so they agree",
            manifest_path.display(),
            package_name.unwrap_or("<missing>"),
            id
        )));
    }
    Ok(())
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
                let stripped = crate::package::stub::strip_to_stub(&file.content)?;
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
    crate::package::template::render_file(
        "autograde.yml",
        &HashMap::from([("base_image", base_image)]),
    )
}

/// `unused_variables`/`dead_code` to allow stubbed body to contain `todo!()`
const ALLOWED_WARNING_LINTS: [&str; 2] = ["unused_variables", "dead_code"];

/// E.g. catches a `use` that's only reachable from a `cfg_select!` arm
/// that doesn't survive into the student build. Requires the crate to
/// declare `[features] student = []`.
fn check_student_view_is_clean(package_dir: &Path, id: &str) -> Result<()> {
    let solution_dir = package_dir.join(id);
    let output = std::process::Command::new("cargo")
        .args(["check", "--features", "student", "--message-format=json"])
        .current_dir(&solution_dir)
        .output()
        .map_err(|source| Error::Io {
            path: solution_dir.clone(),
            source,
        })?;

    if !output.status.success() {
        return Err(Error::Other(format!(
            "cargo check --features student failed at {}:\n{}",
            solution_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let warnings: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-message"))
        .filter_map(|msg| {
            let message = msg.get("message")?;
            if message.get("level")?.as_str()? != "warning" {
                return None;
            }
            let lint = message
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(|c| c.as_str());
            if lint.is_some_and(|lint| ALLOWED_WARNING_LINTS.contains(&lint)) {
                return None;
            }
            message.get("rendered")?.as_str().map(str::to_string)
        })
        .collect();

    if !warnings.is_empty() {
        return Err(Error::Other(format!(
            "refusing to publish: `cargo check --features student` reported {} warning(s) in {} \
             -- fix them before publishing, since they'd ship to students exactly as they \
             are:\n\n{}",
            warnings.len(),
            solution_dir.display(),
            warnings.join("\n")
        )));
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
                "pub fn kept() {}\n\n#[cfg(not(feature = \"student\"))]\nfn hidden() {}\n",
            ),
            matched_file("hw3/Cargo.toml", "[package]\nname = \"hw3\"\n"),
        ];

        let stripped = strip_stub("hw3/src/**", matches, &ctx).unwrap();

        let rs = stripped
            .iter()
            .find(|f| f.rel_path.ends_with("lib.rs"))
            .unwrap();
        assert!(rs.content.contains("pub fn kept"));
        assert!(!rs.content.contains("fn hidden"));

        let toml = stripped
            .iter()
            .find(|f| f.rel_path.ends_with("Cargo.toml"))
            .unwrap();
        assert_eq!(toml.content, "[package]\nname = \"hw3\"\n");
    }
}
