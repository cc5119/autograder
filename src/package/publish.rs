//! Publishes either the starter/template repo or the reference solution
//! for distribution, from the **private instructor package**, in one pass:
//! copy everything real, then resolve the sensitive parts in place
//! according to [`PublishMode`]. No hand-maintained `public/`/`solution/`
//! sibling repo.
//!
//! [`strip_stub`] resolves `{id}/src/**` and `harness/tests/**` to the
//! student view (`PublishMode::Starter`'s only option for the harness, and
//! `{id}/src/**` too when the mode is `Starter`), via the convention
//! `crate::package::stub` applies to ordinary items: an unmarked `#[test]`
//! fn ships by default, and only one stacked with `#[cfg(not(feature =
//! "student"))]` (alongside its own `#[cfg(test)]`, since that alone isn't
//! a directive -- see that module's doc comment) is dropped, which is how
//! an adversarial test stays hidden. [`unstrip_stub`] is its mirror image,
//! resolving `{id}/src/**` to the real solution view instead --
//! `PublishMode::Solution` uses it for `{id}/src/**`, but `harness/tests/**`
//! always resolves as the student view regardless of mode, since the
//! adversarial judge tests are never meant to ship, even alongside a
//! solution. The judge always lives in `harness/`, a sibling package of
//! `{id}`, for both `library` and `binary` (see `evaluator::library`'s and
//! `evaluator::binary`'s module doc comments).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::overlay::{self, Context, MatchedFile, Rule};
use crate::spec::{self, Spec};
use crate::{deps, package, str_map};

#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub out_dir: PathBuf,
}

/// Which view of the private instructor package `publish` produces.
/// `{harness}/tests/**` is always resolved as the student view regardless
/// of `mode` -- students (and anyone the solution is shared with) never see
/// the adversarial judge tests -- only `{id}/src/**`'s resolution direction
/// changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    /// The starter/template repo: `todo!()`-style stubs, reference-only
    /// helpers and adversarial tests stripped out.
    Starter,
    /// The reference solution: real implementations, reference-only
    /// helpers kept; only the harness's adversarial tests are stripped.
    Solution,
}

fn rules(mode: PublishMode) -> Vec<Rule> {
    let src_hook = match mode {
        PublishMode::Starter => strip_stub,
        PublishMode::Solution => unstrip_stub,
    };
    vec![
        Rule::File("Cargo.toml", None),
        Rule::File("Cargo.lock", None),
        Rule::File("README.md", None),
        Rule::File(spec::SPEC_FILE, None),
        Rule::File(".gitignore", None),
        Rule::File("{id}/Cargo.toml", Some(validate_manifest)),
        Rule::Glob("{id}/src/**", Some(src_hook)),
        Rule::File("{harness}/Cargo.toml", None),
        Rule::Glob("{harness}/src/**", None),
        Rule::Glob("{harness}/tests/**", Some(strip_stub)),
    ]
}

pub fn publish(assignment_dir: &Path, out_dir: &Path, mode: PublishMode) -> Result<PublishOutcome> {
    let private_spec_path = assignment_dir.join(spec::SPEC_FILE);
    if !private_spec_path.is_file() {
        return Err(Error::InvalidSpec(format!(
            "publish requires {} in {} (the private instructor package)",
            spec::SPEC_FILE,
            assignment_dir.display()
        )));
    }
    let spec = Spec::load_file(&private_spec_path)?;
    let id = spec.assignment.id;

    // Refuses to ship a `[allowed-crates]`-equivalent that's gone stale --
    // `Cargo.lock`'s resolved dependency graph *is* the allowlist (see
    // `manifest_check`'s module doc comment), so if it doesn't match the
    // hash `autograder lock` last recorded, publishing would ship students
    // an allowlist inconsistent with what grading actually checks against.
    if let Some(message) = deps::lock::verify(assignment_dir, &spec) {
        return Err(Error::InvalidSpec(format!(
            "refusing to publish: {message}"
        )));
    }

    // Checked ahead of the real compile below so these still get a clear
    // `InvalidSpec`, not an opaque `cargo check` failure standing in for it.
    let solution_dir = assignment_dir.join(id.as_str());
    if !solution_dir.is_dir() {
        return Err(Error::InvalidSpec(format!(
            "no {}/ solution directory found in {} -- [assignment].id must name a sibling \
             directory holding the reference solution crate",
            id.as_str(),
            assignment_dir.display()
        )));
    }
    let manifest_path = solution_dir.join("Cargo.toml");
    validate_package_name(
        &manifest_path,
        &fs::read_to_string(&manifest_path)?,
        id.as_str(),
    )?;

    check_student_view_is_clean(assignment_dir)?;

    let ctx = Context::new(
        assignment_dir,
        str_map! {"id" => id, "harness" => spec.assignment.harness},
    );

    overlay::apply(&ctx, out_dir, &rules(mode))?;

    let workflow_dir = out_dir.join(".github/workflows");
    fs::create_dir_all(&workflow_dir)?;
    let workflow_path = workflow_dir.join("autograde.yml");
    let workflow_yaml = autograde_workflow_yaml()?;
    fs::write(&workflow_path, workflow_yaml)?;

    format_published_tree(out_dir)?;

    Ok(PublishOutcome {
        out_dir: out_dir.to_path_buf(),
    })
}

/// `strip_stub`'s text splicing leaves whitespace/indentation artifacts
/// behind (e.g. a spliced-in `cfg_select!` arm keeping its original
/// indentation at a different nesting depth) -- `cargo fmt` over the whole
/// published tree in one pass cleans that up, rather than formatting each
/// stripped file's string individually.
fn format_published_tree(out_dir: &Path) -> Result<()> {
    let output = std::process::Command::new("cargo")
        .arg("fmt")
        .current_dir(out_dir)
        .output()
        .map_err(|source| Error::Io {
            path: out_dir.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "cargo fmt failed on the published tree at {}:\n{}",
            out_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
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
    resolve_rust_sources(matches, &HashSet::from(["student"]))
}

/// The mirror of [`strip_stub`]: resolves `{id}/src/**` to the real
/// solution view (`enabled` empty) for `PublishMode::Solution`, instead of
/// the student view.
fn unstrip_stub(
    _pattern: &str,
    matches: Vec<MatchedFile>,
    _ctx: &Context,
) -> Result<Vec<MatchedFile>> {
    resolve_rust_sources(matches, &HashSet::new())
}

fn resolve_rust_sources(
    matches: Vec<MatchedFile>,
    enabled: &HashSet<&str>,
) -> Result<Vec<MatchedFile>> {
    matches
        .into_iter()
        .map(|file| {
            if file.rel_path.extension().is_some_and(|ext| ext == "rs") {
                let resolved = package::stub::strip_to_stub(&file.content, enabled)
                    .map_err(|e| Error::Other(format!("{e} in {}", file.rel_path.display())))?;
                Ok(MatchedFile {
                    content: resolved,
                    ..file
                })
            } else {
                Ok(file)
            }
        })
        .collect()
}

fn autograde_workflow_yaml() -> Result<String> {
    package::template::render_file("autograde.yml", &str_map! {})
}

const PUBLISH_CONFIG_FILE: &str = "publish.toml";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct PublishConfig {
    #[serde(default)]
    allowed_warnings: Vec<String>,
    #[serde(default)]
    allowed_errors: Vec<String>,
}

fn load_publish_config(assignment_dir: &Path) -> Result<PublishConfig> {
    let path = assignment_dir.join(PUBLISH_CONFIG_FILE);
    if !path.is_file() {
        return Ok(PublishConfig::default());
    }
    let content = fs::read_to_string(&path)?;
    toml::from_str(&content).map_err(|source| Error::Toml {
        path,
        source: Box::new(source),
    })
}

fn check_student_view_is_clean(assignment_dir: &Path) -> Result<()> {
    let config = load_publish_config(assignment_dir)?;
    let output = std::process::Command::new("cargo")
        .args(["check", "--features", "student", "--message-format=json"])
        .current_dir(assignment_dir)
        .output()
        .map_err(|source| Error::Io {
            path: assignment_dir.to_path_buf(),
            source,
        })?;

    let problems: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-message"))
        .filter_map(|msg| {
            let message = msg.get("message")?;
            let allowed = match message.get("level")?.as_str()? {
                "warning" => &config.allowed_warnings,
                "error" => &config.allowed_errors,
                _ => return None,
            };
            let code = message
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(|c| c.as_str());
            if code.is_some_and(|code| allowed.iter().any(|a| a == code)) {
                return None;
            }
            message.get("rendered")?.as_str().map(str::to_string)
        })
        .collect();

    if !output.status.success() && problems.is_empty() {
        return Err(Error::Other(format!(
            "cargo check --features student failed at {}:\n{}",
            assignment_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    if !problems.is_empty() {
        return Err(Error::Other(format!(
            "refusing to publish: `cargo check --features student` reported {} problem(s) in {} \
             -- fix them before publishing, since they'd ship to students exactly as they \
             are:\n\n{}",
            problems.len(),
            assignment_dir.display(),
            problems.join("\n")
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
        Context::new(&PathBuf::new(), str_map! {"id" => id})
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
