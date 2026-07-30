//! Declarative file overlay: copy files matching a small rule table from
//! one source tree onto a destination, with an optional per-rule hook to
//! transform matched content (or reject it) before it's written. Used by
//! `publish` (private instructor package -> public starter) and
//! `pipeline::grade_batch` (student checkout + instructor package ->
//! ephemeral grading directory) -- both are "copy this subset of one real
//! tree onto another," just with different sources, hooks, and content
//! rules.
//!
//! A `{key}` token in a `Rule`'s path/pattern is resolved against
//! `Context::substitutions` before it's matched or read (e.g. `"{id}/src/**"`
//! with `substitutions = {"id": "hw3"}` becomes `"hw3/src/**"`). Every rule
//! in one `apply` call reads from the same `Context::source_root` -- a
//! caller that needs to pull from two different trees (grading's checkout
//! + instructor package) calls `apply` once per tree, in order.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ignore::overrides::OverrideBuilder;

use crate::error::{Error, Result};
use crate::exec::fs;

#[derive(Debug)]
pub struct MatchedFile {
    pub rel_path: PathBuf,
    pub content: String,
}

/// Where a rule set reads from, and the `{key}` -> value substitutions its
/// paths/patterns may reference. One concrete type for every caller -- no
/// hook needs anything this doesn't already carry.
pub struct Context {
    pub source_root: PathBuf,
    pub substitutions: HashMap<String, String>,
}

impl Context {
    pub fn new(source_root: &Path, substitutions: HashMap<String, String>) -> Self {
        Self {
            source_root: source_root.to_path_buf(),
            substitutions,
        }
    }

    fn resolve(&self, template: &str) -> String {
        let mut resolved = template.to_string();
        for (key, value) in &self.substitutions {
            resolved = resolved.replace(&format!("{{{key}}}"), value);
        }
        resolved
    }
}

pub type FileHook = fn(path: &str, file: MatchedFile, ctx: &Context) -> Result<MatchedFile>;
pub type GlobHook =
    fn(pattern: &str, matches: Vec<MatchedFile>, ctx: &Context) -> Result<Vec<MatchedFile>>;

pub enum Rule {
    /// A single required file. Errors if it's missing from `source_root`.
    File(&'static str, Option<FileHook>),
    /// Every file under `source_root` matching this glob (`ignore`-crate
    /// syntax); zero matches is not an error.
    Glob(&'static str, Option<GlobHook>),
}

pub fn apply(ctx: &Context, dest: &Path, rules: &[Rule]) -> Result<()> {
    let all_files = fs::walk_regular_files(&ctx.source_root)?;

    for rule in rules {
        match rule {
            Rule::File(path, hook) => {
                let rel_path = PathBuf::from(ctx.resolve(path));
                if !ctx.source_root.join(&rel_path).is_file() {
                    return Err(Error::InvalidSpec(format!(
                        "overlay requires {} under {} -- there is nothing to copy `{}` from",
                        rel_path.display(),
                        ctx.source_root.display(),
                        rel_path.display()
                    )));
                }
                let file = read_file(&ctx.source_root, rel_path)?;
                let file = match hook {
                    Some(hook) => hook(path, file, ctx)?,
                    None => file,
                };
                write_file(dest, file)?;
            }
            Rule::Glob(pattern, hook) => {
                let pattern = ctx.resolve(pattern);
                // `"."`, not `&ctx.source_root`: `all_files` entries are
                // already relative to `source_root` (stripped by
                // `fs::walk_regular_files`), so `ignore`'s own root-stripping in
                // `matched()` must be a no-op here -- passing the real
                // `source_root` would make it strip a *second* time, which
                // silently corrupts the match whenever `source_root` is a
                // relative path whose own basename happens to prefix-match
                // a candidate (e.g. `autograder init --id hw0 hw0`, where
                // the package dir and `{id}` share the name `hw0`).
                let override_ = OverrideBuilder::new(".")
                    .add(&pattern)
                    .unwrap()
                    .build()
                    .unwrap();

                let mut matches = Vec::new();
                for rel_path in &all_files {
                    if override_.matched(rel_path, false).is_whitelist() {
                        matches.push(read_file(&ctx.source_root, rel_path.clone())?);
                    }
                }

                let matches = match hook {
                    Some(hook) => hook(&pattern, matches, ctx)?,
                    None => matches,
                };
                for file in matches {
                    write_file(dest, file)?;
                }
            }
        }
    }

    Ok(())
}

fn read_file(source_root: &Path, rel_path: PathBuf) -> Result<MatchedFile> {
    let full_path = source_root.join(&rel_path);
    let content = fs::read_to_string(&full_path)?;
    Ok(MatchedFile { rel_path, content })
}

fn write_file(dest: &Path, file: MatchedFile) -> Result<()> {
    let dst = dest.join(&file.rel_path);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dst, file.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn ctx(source_root: &Path, substitutions: &[(&'static str, &str)]) -> Context {
        Context::new(
            source_root,
            substitutions
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    /// Reproduces `autograder init --id hw0 hw0` followed by a relative
    /// `autograder publish --out hw0-starter hw0`: the package dir's own
    /// basename ("hw0") coincides with `{id}`'s subdirectory name. `ignore`
    /// expects paths passed to `matched()` to still carry the root prefix
    /// it can strip; `all_files` here is already root-relative, so a
    /// `source_root` whose basename prefix-matches a candidate used to make
    /// it strip a second time and silently drop the match (fixed by always
    /// building the `OverrideBuilder` against `"."`, not `source_root`).
    #[test]
    fn glob_rule_matches_when_the_relative_source_root_shares_a_name_with_the_glob_prefix() {
        let cwd = std::env::current_dir().unwrap();
        let base = tempfile::tempdir_in(&cwd).unwrap();
        let name = base
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        write(&base.path().join(&name).join("src/lib.rs"), "pub fn f() {}");

        let dest = tempfile::tempdir().unwrap();
        let relative_source_root = PathBuf::from(&name);
        apply(
            &ctx(&relative_source_root, &[("id", &name)]),
            dest.path(),
            &[Rule::Glob("{id}/src/**", None)],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.path().join(&name).join("src/lib.rs")).unwrap(),
            "pub fn f() {}"
        );
    }

    #[test]
    fn glob_rule_copies_every_matching_file_preserving_relative_path() {
        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("hw3/src/lib.rs"), "pub fn f() {}");
        write(&src.path().join("hw3/Cargo.toml"), "[package]\n");
        write(&src.path().join("other/ignored.rs"), "ignored");

        let dest = tempfile::tempdir().unwrap();
        apply(
            &ctx(src.path(), &[]),
            dest.path(),
            &[Rule::Glob("hw3/**", None)],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.path().join("hw3/src/lib.rs")).unwrap(),
            "pub fn f() {}"
        );
        assert!(dest.path().join("hw3/Cargo.toml").is_file());
        assert!(!dest.path().join("other").exists());
    }

    #[test]
    fn substitutions_resolve_key_tokens_in_patterns() {
        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("hw3/src/lib.rs"), "pub fn f() {}");

        let dest = tempfile::tempdir().unwrap();
        apply(
            &ctx(src.path(), &[("id", "hw3")]),
            dest.path(),
            &[Rule::Glob("{id}/src/**", None)],
        )
        .unwrap();

        assert!(dest.path().join("hw3/src/lib.rs").is_file());
    }

    #[test]
    fn file_rule_errors_clearly_when_the_source_is_missing() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        let err = apply(
            &ctx(src.path(), &[]),
            dest.path(),
            &[Rule::File("Cargo.toml", None)],
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn hook_can_transform_matched_content_before_it_is_written() {
        fn uppercase(
            _pattern: &str,
            matches: Vec<MatchedFile>,
            _ctx: &Context,
        ) -> Result<Vec<MatchedFile>> {
            Ok(matches
                .into_iter()
                .map(|f| MatchedFile {
                    content: f.content.to_uppercase(),
                    ..f
                })
                .collect())
        }

        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("hw3/src/lib.rs"), "pub fn f() {}");

        let dest = tempfile::tempdir().unwrap();
        apply(
            &ctx(src.path(), &[]),
            dest.path(),
            &[Rule::Glob("hw3/**", Some(uppercase))],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.path().join("hw3/src/lib.rs")).unwrap(),
            "PUB FN F() {}"
        );
    }
}
