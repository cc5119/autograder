//! Publishes either the starter/template repo or the reference solution
//! for distribution, from the **private instructor package**, in one pass:
//! copy everything real, then resolve the sensitive parts in place
//! according to [`PublishMode`]. No hand-maintained `public/`/`solution/`
//! sibling repo.
//!
//! [`strip_stub`] resolves `{id}/src/**`, `harness/src/**` and
//! `harness/tests/**` to the student view (`PublishMode::Starter`'s only
//! option for the harness, and `{id}/src/**` too when the mode is
//! `Starter`), via the convention `crate::package::stub` applies to
//! ordinary items: an unmarked `#[test]` fn ships by default, and only one
//! stacked with `#[cfg(not(feature = "student"))]` (alongside its own
//! `#[cfg(test)]`, since that alone isn't a directive -- see that module's
//! doc comment) is dropped, which is how an adversarial test (or a
//! confidential helper under `harness/src`) stays hidden. [`unstrip_stub`]
//! is its mirror image, resolving `{id}/src/**` to the real solution view
//! instead -- `PublishMode::Solution` uses it for `{id}/src/**`, but
//! `harness/src/**` and `harness/tests/**` always resolve as the student
//! view regardless of mode, since the harness itself is never a solution to
//! ship a variant of, and its adversarial parts are never meant to ship at
//! all. The judge always lives in `harness/`, a sibling package of `{id}`,
//! for both `library` and `binary` (see `evaluator::library`'s and
//! `evaluator::binary`'s module doc comments).
//!
//! A workspace may carry further instructor-owned packages beyond the
//! harness -- `[assignment].extra-packages` -- and those ship verbatim in
//! both modes: a support library has no student view distinct from the
//! instructor's, so there is nothing to resolve in either direction.
//!
//! Anything else the starter needs -- an image the README embeds, say --
//! ships only if `publish.toml`'s `include` names it (see
//! [`resolve_includes`]): the rule table above stays the only thing that
//! decides what leaves the private package by default, so a stray file
//! never ships by accident. [`check_readme_links`] then catches the
//! opposite mistake, a README pointing at something that didn't ship.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::exec::fs;
use crate::exec::overlay::{self, Context, MatchedFile, Rule};
use crate::package::PublishMode;
use crate::spec::{self, Spec};
use crate::{deps, package, str_map};
use ignore::overrides::OverrideBuilder;

#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub out_dir: PathBuf,
}

fn rules(mode: PublishMode) -> Vec<Rule> {
    let src_hook = match mode {
        PublishMode::Starter => strip_stub,
        PublishMode::Solution => unstrip_stub,
    };
    let mut rules = vec![
        Rule::File("Cargo.toml", None),
        Rule::File("Cargo.lock", None),
        Rule::Glob("README.md", None),
        Rule::File(spec::SPEC_FILE, None),
        Rule::File(".gitignore", None),
        Rule::File("{id}/Cargo.toml", Some(validate_manifest)),
        Rule::Glob("{id}/src/**", Some(src_hook)),
    ];
    rules.extend(student_harness_rules());
    rules
}

/// The student view of the harness, the same in both modes (see this
/// module's doc comment). Also what `evaluate`'s stage 1 mounts in place of
/// the real harness: public already, so a student `build.rs` may read it,
/// and a complete package Cargo can load.
pub(crate) fn student_harness_rules() -> Vec<Rule> {
    vec![
        Rule::File("{harness}/Cargo.toml", None),
        Rule::Glob("{harness}/src/**", Some(strip_stub)),
        Rule::Glob("{harness}/tests/**", Some(strip_stub)),
    ]
}

/// One `[assignment].extra-packages` package, resolved against `{extra}`.
/// Shipped verbatim: a support library is not a solution, so there is
/// nothing to strip -- the student view and the instructor's are the same
/// file. Applied once per package (see [`Rule`]'s `{key}` substitution).
fn extra_package_rules() -> Vec<Rule> {
    vec![
        Rule::File("{extra}/Cargo.toml", None),
        Rule::Glob("{extra}/src/**", None),
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
        return Err(Error::StaleLock(format!("refusing to publish: {message}")));
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

    // Same courtesy for the support packages: `overlay::apply` would fail
    // on the missing `Cargo.toml` anyway, but naming the spec key that
    // asked for it turns that into a fixable message.
    for package in &spec.assignment.extra_packages {
        if !assignment_dir.join(package).is_dir() {
            return Err(Error::InvalidSpec(format!(
                "no {package}/ directory found in {} -- every name in \
                 [assignment].extra-packages must be a sibling package directory",
                assignment_dir.display()
            )));
        }
    }

    let config = load_publish_config(assignment_dir)?;
    check_student_view_is_clean(assignment_dir, &config)?;

    // Resolved before anything is written, so a bad pattern leaves `out_dir`
    // untouched like the other spec errors above.
    let packages: Vec<&str> = [id.as_str(), spec.assignment.harness.as_str()]
        .into_iter()
        .chain(spec.assignment.extra_packages.iter().map(String::as_str))
        .collect();
    let included = resolve_includes(assignment_dir, &config.include, &packages)?;

    let ctx = Context::new(
        assignment_dir,
        str_map! {"id" => id, "harness" => spec.assignment.harness},
    );

    let mut published: HashSet<PathBuf> = HashSet::new();
    published.extend(overlay::apply(&ctx, out_dir, &rules(mode))?);

    let extra_rules = extra_package_rules();
    for package in &spec.assignment.extra_packages {
        let ctx = Context::new(assignment_dir, str_map! {"extra" => package});
        published.extend(overlay::apply(&ctx, out_dir, &extra_rules)?);
    }

    // Copied as bytes, not through `overlay`'s `String` content, so binary
    // files (a PNG, a PDF) ship intact.
    for rel_path in included {
        let dst = out_dir.join(&rel_path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&assignment_dir.join(&rel_path), &dst)?;
        published.insert(rel_path);
    }

    let workflow_dir = out_dir.join(".github/workflows");
    fs::create_dir_all(&workflow_dir)?;
    let workflow_path = workflow_dir.join("autograde.yml");
    let workflow_yaml = autograde_workflow_yaml()?;
    fs::write(&workflow_path, workflow_yaml)?;

    check_readme_links(assignment_dir, out_dir, &published)?;

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
    /// Listing an error code here also accepts cargo's non-zero exit: a
    /// starter that's meant not to compile is a legitimate assignment.
    #[serde(default)]
    allowed_errors: Vec<String>,
    /// Extra files to ship verbatim, as globs relative to the assignment
    /// directory (gitignore syntax, like the rule table's). See
    /// [`resolve_includes`].
    #[serde(default)]
    include: Vec<String>,
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

fn check_student_view_is_clean(assignment_dir: &Path, config: &PublishConfig) -> Result<()> {
    let output = std::process::Command::new("cargo")
        .args(["check", "--features", "student", "--message-format=json"])
        .current_dir(assignment_dir)
        .output()
        .map_err(|source| Error::Io {
            path: assignment_dir.to_path_buf(),
            source,
        })?;

    let diagnostics: Vec<Problem> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-message"))
        .filter_map(|msg| {
            let message = msg.get("message")?;
            let is_warning = match message.get("level")?.as_str()? {
                "warning" => true,
                "error" => false,
                _ => return None,
            };
            Some(Problem {
                is_warning,
                code: message
                    .get("code")
                    .and_then(|c| c.get("code"))
                    .and_then(|c| c.as_str())
                    .map(str::to_string),
                rendered: message.get("rendered")?.as_str()?.to_string(),
            })
        })
        .collect();

    let saw_errors = diagnostics.iter().any(|d| !d.is_warning);
    let problems: Vec<Problem> = diagnostics
        .into_iter()
        .filter(|d| {
            let allowed = if d.is_warning {
                &config.allowed_warnings
            } else {
                &config.allowed_errors
            };
            !d.code
                .as_deref()
                .is_some_and(|code| allowed.iter().any(|a| a == code))
        })
        .collect();

    // A non-zero exit with every error allowlisted is the expected shape of
    // an assignment whose starter is *meant* not to compile -- fixing the
    // errors is the exercise. Only a failure rustc reported no errors for is
    // a real one: cargo couldn't run the check at all.
    if !output.status.success() && !saw_errors {
        return Err(Error::Other(format!(
            "`cargo check --features student` failed in {} without reporting any diagnostics -- \
             this is a cargo/manifest problem, not student-view code. Is there a `student` \
             feature in Cargo.toml?\n\n{}",
            assignment_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    if !problems.is_empty() {
        let rendered: Vec<&str> = problems.iter().map(|p| p.rendered.as_str()).collect();
        return Err(Error::Other(format!(
            "refusing to publish: `cargo check --features student` reported {} problem(s) in {} \
             -- fix them before publishing, since they'd ship to students exactly as they \
             are:\n\n{}\n{}",
            problems.len(),
            assignment_dir.display(),
            rendered.join("\n"),
            how_to_fix(assignment_dir, &problems)
        )));
    }

    Ok(())
}

/// Resolves `publish.toml`'s `include` patterns to the files they name,
/// relative to `assignment_dir`. Each pattern must match at least one file
/// -- one matching nothing is almost certainly a typo, and failing silently
/// would ship a starter missing the very file it was added for.
///
/// No pattern may reach into a package directory (`packages`: `{id}`, the
/// harness, the extra packages): those ship through the rule table, which
/// strips the solution and the hidden tests, and a verbatim copy on top
/// would undo exactly that. `target/` and dot-directories (`.git`, `.jj`)
/// are never candidates, so a broad pattern like `*.svg` can't drag build
/// output or VCS internals along.
fn resolve_includes(
    assignment_dir: &Path,
    patterns: &[String],
    packages: &[&str],
) -> Result<Vec<PathBuf>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    let candidates: Vec<PathBuf> = fs::walk_regular_files(assignment_dir)?
        .into_iter()
        .filter(|rel_path| {
            let first = rel_path.components().next();
            !matches!(first, Some(Component::Normal(name))
                if name == "target" || name.to_string_lossy().starts_with('.'))
        })
        .collect();

    let mut included = Vec::new();
    for pattern in patterns {
        if pattern.starts_with('!') || pattern.split('/').any(|part| part == "..") {
            return Err(Error::InvalidSpec(format!(
                "{PUBLISH_CONFIG_FILE}: include pattern {pattern:?} must name files inside \
                 {} -- negated patterns and `..` are not supported",
                assignment_dir.display()
            )));
        }
        // `"."` for the same reason as in `overlay::apply`: `candidates` are
        // already relative to `assignment_dir`.
        let matcher = OverrideBuilder::new(".")
            .add(pattern)
            .and_then(|builder| builder.build())
            .map_err(|e| {
                Error::InvalidSpec(format!(
                    "{PUBLISH_CONFIG_FILE}: invalid include pattern {pattern:?}: {e}"
                ))
            })?;
        let matches: Vec<&PathBuf> = candidates
            .iter()
            .filter(|rel_path| matcher.matched(rel_path, false).is_whitelist())
            .collect();
        if matches.is_empty() {
            return Err(Error::InvalidSpec(format!(
                "{PUBLISH_CONFIG_FILE}: include pattern {pattern:?} matches no file in {}",
                assignment_dir.display()
            )));
        }
        for rel_path in matches {
            if let Some(package) = packages.iter().find(|p| rel_path.starts_with(p)) {
                return Err(Error::InvalidSpec(format!(
                    "{PUBLISH_CONFIG_FILE}: include pattern {pattern:?} matches {}, inside the \
                     `{package}` package -- packages ship through publish's own rules (which \
                     strip the solution and hidden tests), so they can't be included verbatim. \
                     Anchor the pattern with a leading `/` if it was meant for a top-level file.",
                    rel_path.display()
                )));
            }
            included.push(rel_path.clone());
        }
    }
    included.sort();
    included.dedup();
    Ok(included)
}

/// Refuses to publish a README whose relative links point at something
/// that didn't ship -- typically an image or diagram left out of
/// `include`. Checked against `published` (what this run wrote), not
/// `out_dir`'s contents, since a re-publish over an older tree would
/// otherwise find stale files there and pass.
///
/// A link to a directory is fine as long as some published file is under
/// it. External links (`scheme://`, `mailto:`), in-page anchors and
/// root-absolute paths are skipped.
fn check_readme_links(
    assignment_dir: &Path,
    out_dir: &Path,
    published: &HashSet<PathBuf>,
) -> Result<()> {
    let readme = out_dir.join("README.md");
    if !readme.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&readme)?;

    let mut broken: Vec<String> = readme_link_targets(&content)
        .into_iter()
        .filter(|target| match normalize_link(target) {
            Some(rel_path) => !published.iter().any(|p| p.starts_with(&rel_path)),
            None => true,
        })
        .collect();
    broken.sort();
    broken.dedup();
    if broken.is_empty() {
        return Ok(());
    }

    let list: Vec<String> = broken.iter().map(|t| format!("  - {t}")).collect();
    let mut message = format!(
        "refusing to publish: README.md links to {} path(s) that the published tree doesn't \
         contain:\n\n{}\n",
        broken.len(),
        list.join("\n")
    );
    let includable: Vec<String> = broken
        .iter()
        .filter_map(|t| normalize_link(t))
        .filter(|rel_path| assignment_dir.join(rel_path).exists())
        .map(|rel_path| format!("{:?}", rel_path.to_string_lossy()))
        .collect();
    if !includable.is_empty() {
        message.push_str(&format!(
            "\nif they should ship, list them in {}:\n\n    include = [{}]\n",
            assignment_dir.join(PUBLISH_CONFIG_FILE).display(),
            includable.join(", ")
        ));
    }
    Err(Error::Other(message))
}

/// The link targets in `markdown` that refer to a file in the repository:
/// inline links and images (`[text](target)`, `![alt](target)`) and
/// reference definitions (`[label]: target`). Fenced code blocks and inline
/// code spans are skipped, since they show syntax rather than link.
fn readme_link_targets(markdown: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // Drop inline code spans: every other backtick-delimited segment.
        let line: String = line.split('`').step_by(2).collect::<Vec<_>>().join(" ");

        if let Some(rest) = line.trim_start().strip_prefix('[')
            && let Some((_, after)) = rest.split_once("]:")
            && let Some(target) = after.split_whitespace().next()
        {
            targets.push(target.to_string());
            continue;
        }

        let mut rest = line.as_str();
        while let Some(start) = rest.find("](") {
            rest = &rest[start + 2..];
            let end = rest.find(')').unwrap_or(rest.len());
            if let Some(target) = rest[..end].split_whitespace().next() {
                targets.push(target.to_string());
            }
            rest = &rest[end..];
        }
    }
    targets
        .into_iter()
        .map(|t| t.trim_start_matches('<').trim_end_matches('>').to_string())
        .filter(|t| {
            !(t.is_empty()
                || t.starts_with('#')
                || t.starts_with('/')
                || t.contains("://")
                || t.starts_with("mailto:"))
        })
        .collect()
}

/// `target` as a path relative to the repository root, without its
/// `#fragment`/`?query` or `.` components. `None` if it climbs out of the
/// root with `..`, which can't point at anything that ships.
fn normalize_link(target: &str) -> Option<PathBuf> {
    let path = target.split(['#', '?']).next().unwrap_or_default();
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

struct Problem {
    is_warning: bool,
    /// `None` for messages rustc emits without a lint name or error code.
    code: Option<String>,
    rendered: String,
}

/// Spells out the two ways forward, and pre-writes the `publish.toml` the
/// second one needs so it doesn't have to be reconstructed from
/// [`PublishConfig`]'s field names by hand.
fn how_to_fix(assignment_dir: &Path, problems: &[Problem]) -> String {
    let codes = |warnings: bool| {
        let mut codes: Vec<&str> = problems
            .iter()
            .filter(|p| p.is_warning == warnings)
            .filter_map(|p| p.code.as_deref())
            .collect();
        codes.sort_unstable();
        codes.dedup();
        codes
    };
    let toml_list = |codes: &[&str]| {
        codes
            .iter()
            .map(|code| format!("{code:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut how = format!(
        "\nhow to fix: edit the sources under {} so the student view compiles cleanly -- \
         a stub that's `todo!()` in the student view usually needs `#[allow(...)]`, or the \
         helper it leaves unused stripped alongside it.",
        assignment_dir.display()
    );

    let (warnings, errors) = (codes(true), codes(false));
    if warnings.is_empty() && errors.is_empty() {
        how.push_str(
            "\n\nnone of these carry a lint name or error code, so they can't be allowlisted \
             in publish.toml -- they have to be fixed at the source.",
        );
        return how;
    }

    how.push_str(&format!(
        "\n\nif a problem is expected in the starter instead, allow it by code in {}:\n\n",
        assignment_dir.join(PUBLISH_CONFIG_FILE).display()
    ));
    if !warnings.is_empty() {
        how.push_str(&format!(
            "    allowed-warnings = [{}]\n",
            toml_list(&warnings)
        ));
    }
    if !errors.is_empty() {
        how.push_str(&format!("    allowed-errors = [{}]\n", toml_list(&errors)));
    }
    how
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
    fn readme_link_targets_finds_inline_links_images_and_reference_definitions() {
        let markdown = "\
# Title

See [`main.rs`](hw3/src/main.rs) and the [harness](harness).

![diagram](diagram.svg \"Strategy\")

[ref]: docs/notes.pdf
";
        assert_eq!(
            readme_link_targets(markdown),
            [
                "hw3/src/main.rs",
                "harness",
                "diagram.svg",
                "docs/notes.pdf"
            ]
        );
    }

    #[test]
    fn readme_link_targets_skips_external_links_anchors_and_code() {
        let markdown = "\
[Wikipedia](https://es.wikipedia.org/wiki/Conjunto_de_Mandelbrot)
[mail](mailto:someone@example.com) [section](#pruebas) [abs](/root)
Inline code `[a](not-a-link.rs)` is not a link.

```md
[inside](fenced.rs)
```
";
        assert!(readme_link_targets(markdown).is_empty());
    }

    #[test]
    fn normalize_link_drops_fragments_and_dot_components() {
        assert_eq!(
            normalize_link("./docs/../img/a.svg#top"),
            Some(PathBuf::from("img/a.svg"))
        );
        assert_eq!(normalize_link("harness/"), Some(PathBuf::from("harness")));
    }

    #[test]
    fn normalize_link_rejects_paths_that_climb_out_of_the_root() {
        assert_eq!(normalize_link("../other/README.md"), None);
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
