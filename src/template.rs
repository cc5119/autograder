//! Single source of truth for every generated-from-a-template file in this
//! crate: `init`'s per-kind package skeletons and `publish`'s GitHub
//! Actions workflow both render through here -- one embed of `templates/`
//! (compiled into the binary via [`include_dir`]), one substitution
//! mechanism, one place to add a new template. Neither caller reads
//! `templates/` any other way.
//!
//! Deliberately does no disk I/O of its own: [`render_file`]/[`render_tree`]
//! return rendered content, not written files -- writing stays the
//! caller's job (via `crate::fs`, same as every other file-writing call
//! site in the crate), so this module's only concern is "resolve a
//! template and substitute placeholders into it," never "how do we report
//! a write failure."

use std::collections::HashMap;
use std::path::PathBuf;

use include_dir::{Dir, File, include_dir};

use crate::error::{Error, Result};

static TEMPLATES: Dir = include_dir!("$CARGO_MANIFEST_DIR/templates");

/// Renders the single template file at `path` (relative to `templates/`,
/// e.g. `"autograde.yml"` -- the `.tmpl` suffix is implied and optional
/// either way), substituting `placeholders` into its contents.
pub fn render_file(path: &str, placeholders: &HashMap<&str, &str>) -> Result<String> {
    let candidates = [format!("{path}.tmpl"), path.to_string()];
    let file = candidates
        .iter()
        .find_map(|candidate| TEMPLATES.get_file(candidate))
        .ok_or_else(|| {
            Error::Other(format!(
                "no template file at templates/{path}(.tmpl) -- this is a bug in autograder \
                 itself, not something a caller can fix"
            ))
        })?;
    Ok(substitute(contents_utf8(file), placeholders))
}

/// Renders every file under the template subdirectory `dir` (relative to
/// `templates/`, e.g. `"library"`), substituting `placeholders` into both
/// each file's path (with the trailing `.tmpl` stripped) and its contents.
/// Returns `(relative_path, rendered_content)` pairs, in no particular
/// order, for the caller to write wherever it wants.
pub fn render_tree(
    dir: &str,
    placeholders: &HashMap<&str, &str>,
) -> Result<Vec<(PathBuf, String)>> {
    let root = TEMPLATES.get_dir(dir).ok_or_else(|| {
        Error::Other(format!(
            "no template directory at templates/{dir}/ -- this is a bug in autograder itself, \
             not something a caller can fix"
        ))
    })?;

    Ok(walk(root)
        .into_iter()
        .map(|file| {
            let rel_path = file
                .path()
                .strip_prefix(root.path())
                .expect("file is under root by construction")
                .with_extension(""); // drop the trailing `.tmpl`
            let rel_path = PathBuf::from(substitute(&rel_path.to_string_lossy(), placeholders));
            let content = substitute(contents_utf8(file), placeholders);
            (rel_path, content)
        })
        .collect())
}

fn contents_utf8<'a>(file: &'a File<'a>) -> &'a str {
    file.contents_utf8()
        .unwrap_or_else(|| panic!("template {} is not valid UTF-8", file.path().display()))
}

/// Every file under `dir`, recursively -- `include_dir`'s `Dir` only
/// exposes direct children (`files()`/`dirs()`), so this walks the tree
/// itself.
fn walk<'a>(dir: &'a Dir<'a>) -> Vec<&'a File<'a>> {
    let mut files: Vec<&File> = dir.files().collect();
    for subdir in dir.dirs() {
        files.extend(walk(subdir));
    }
    files
}

/// Single-pass substitution of `{key}` tokens against `placeholders`: a
/// `{` is only ever treated as the start of a placeholder if the text up
/// to the *next* `}` exactly matches a known key -- anything else (an
/// unrelated `{...}`, e.g. TOML's own inline-table syntax like
/// `{ path = "../{id}" }`) is emitted verbatim, character by character,
/// which is also exactly why a single multi-key pass (not one `.replace`
/// call per key run back-to-back) is what's needed here: chained
/// `.replace` calls risk one substitution's *output* being reinterpreted
/// by the next call, which a single left-to-right scan never does.
fn substitute(text: &str, placeholders: &HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'{'
            && let Some(rel_end) = text[i + 1..].find('}')
        {
            let key = &text[i + 1..i + 1 + rel_end];
            if let Some(value) = placeholders.get(key) {
                out.push_str(value);
                i += rel_end + 2; // past the closing `}`
                continue;
            }
        }
        let ch = text[i..].chars().next().expect("i < text.len()");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_replaces_only_exact_known_keys() {
        let placeholders = HashMap::from([("id", "hw3")]);
        assert_eq!(substitute("id = \"{id}\"", &placeholders), "id = \"hw3\"");
        // TOML's own inline-table braces must survive untouched, with the
        // placeholder nested inside them still substituted.
        assert_eq!(
            substitute("{id} = { path = \"../{id}\" }", &placeholders),
            "hw3 = { path = \"../hw3\" }"
        );
        // An unrelated brace pair that isn't a known key is left alone.
        assert_eq!(
            substitute("println!(\"{}\", x)", &placeholders),
            "println!(\"{}\", x)"
        );
    }

    #[test]
    fn render_file_substitutes_the_named_template() {
        let placeholders = HashMap::from([("base_image", "autograder-base:1.86.0")]);
        let rendered = render_file("autograde.yml", &placeholders).unwrap();
        assert!(rendered.contains("podman pull autograder-base:1.86.0"));
    }

    #[test]
    fn render_file_errors_clearly_for_an_unknown_template() {
        let err = render_file("does-not-exist", &HashMap::new()).unwrap_err();
        assert!(matches!(err, Error::Other(_)));
    }

    #[test]
    fn render_tree_substitutes_every_file_s_path_and_contents() {
        let placeholders = HashMap::from([("id", "hw3"), ("deadline", "2099-01-01T00:00:00Z")]);
        let rendered = render_tree("library", &placeholders).unwrap();

        let rel_paths: Vec<_> = rendered.iter().map(|(path, _)| path.clone()).collect();
        assert!(rel_paths.contains(&PathBuf::from("hw3/Cargo.toml")));
        assert!(rel_paths.contains(&PathBuf::from("harness/tests/judge.rs")));
        assert!(
            rel_paths
                .iter()
                .all(|p| p.extension() != Some("tmpl".as_ref()))
        );

        let spec = rendered
            .iter()
            .find(|(path, _)| path == std::path::Path::new("autograder.toml"))
            .unwrap();
        assert!(spec.1.contains("id = \"hw3\""));
        assert!(spec.1.contains("2099-01-01T00:00:00Z"));
    }

    #[test]
    fn render_tree_errors_clearly_for_an_unknown_kind() {
        let err = render_tree("does-not-exist", &HashMap::new()).unwrap_err();
        assert!(matches!(err, Error::Other(_)));
    }
}
