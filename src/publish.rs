//! Derives the public-facing spec + harness test files straight from the
//! private instructor package, so nothing needs a hand-maintained `public/`
//! sibling repo that can silently drift out of sync with the private one.
//!
//! Two independent, mechanical transforms — no name resolution, no
//! judgment calls about what's "safe," just structural filtering against
//! data the private spec already carries:
//!
//! - [`derive_public_spec_toml`]: strips `points` and any non-`public`
//!   `[[scoring.tests]]` entries out of the private spec's raw TOML via a
//!   generic `toml::Value` edit — no need to round-trip through `Spec`'s
//!   typed fields, since points/visibility are already there in the text.
//! - [`keep_only_named_tests`]: parses a harness test file with `syn` and
//!   drops any `#[test]` fn not named in the public test list returned by
//!   the spec transform above — the same "only what's explicitly
//!   known-safe survives" shape as [`crate::stub`], applied to test
//!   functions instead of solution code.

use std::collections::HashSet;
use std::path::PathBuf;

use syn::Item;

use crate::error::{Error, Result};

/// Returns the public spec TOML text plus the set of test names it kept
/// (visibility = "public"), so the caller can filter harness test files to
/// match.
pub fn derive_public_spec_toml(private_toml: &str) -> Result<(String, HashSet<String>)> {
    let mut value: toml::Value = toml::from_str(private_toml).map_err(|source| Error::Toml {
        path: PathBuf::from("<private spec>"),
        source: Box::new(source),
    })?;

    let mut public_names = HashSet::new();
    if let Some(tests) = value
        .get_mut("scoring")
        .and_then(|s| s.get_mut("tests"))
        .and_then(|t| t.as_array_mut())
    {
        tests.retain(|t| {
            let is_public = t.get("visibility").and_then(|v| v.as_str()) == Some("public");
            if is_public {
                if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                    public_names.insert(name.to_string());
                }
            }
            is_public
        });
        for t in tests.iter_mut() {
            if let Some(table) = t.as_table_mut() {
                table.remove("points");
            }
        }
    }

    let rendered = toml::to_string_pretty(&value)
        .map_err(|e| Error::Other(format!("failed to render public spec: {e}")))?;
    Ok((rendered, public_names))
}

/// Strips a top-level table (e.g. `[patch.crates-io]`) out of a TOML
/// document. Used to drop the instructor harness's default `[patch]`
/// pointing at `../solution` — a path that doesn't exist in the public
/// repo and would only reveal the solution directory's name.
pub fn strip_toml_table(source: &str, key: &str) -> Result<String> {
    let mut value: toml::Value = toml::from_str(source).map_err(|source| Error::Toml {
        path: PathBuf::from("<harness manifest>"),
        source: Box::new(source),
    })?;
    if let Some(table) = value.as_table_mut() {
        table.remove(key);
    }
    toml::to_string_pretty(&value)
        .map_err(|e| Error::Other(format!("failed to render manifest: {e}")))
}

/// Drops any `#[test]` fn not named in `keep_names`; everything else
/// (helper structs/impls, `use`s, non-test fns) passes through untouched.
pub fn keep_only_named_tests(source: &str, keep_names: &HashSet<String>) -> Result<String> {
    let file = syn::parse_file(source)
        .map_err(|e| Error::Other(format!("failed to parse harness test file: {e}")))?;

    let items: Vec<Item> = file
        .items
        .into_iter()
        .filter(|item| match item {
            Item::Fn(f) => {
                let is_test = f.attrs.iter().any(|a| a.path().is_ident("test"));
                !is_test || keep_names.contains(&f.sig.ident.to_string())
            }
            _ => true,
        })
        .collect();

    let filtered = syn::File {
        shebang: file.shebang,
        attrs: file.attrs,
        items,
    };
    Ok(prettyplease::unparse(&filtered))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIVATE_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00"


[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]
serde = "1"

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
model = "weighted"

[[scoring.tests]]
name = "insert_basic"
points = 10
visibility = "public"

[[scoring.tests]]
name = "balance_adversarial"
points = 20
visibility = "private"
"#;

    #[test]
    fn derive_public_spec_toml_drops_points_and_hidden_tests() {
        let (public_toml, names) = derive_public_spec_toml(PRIVATE_TOML).unwrap();
        assert!(!public_toml.contains("points"));
        assert!(!public_toml.contains("balance_adversarial"));
        assert!(public_toml.contains("insert_basic"));
        assert_eq!(names, HashSet::from(["insert_basic".to_string()]));
    }

    #[test]
    fn strip_toml_table_removes_the_named_table() {
        let manifest = "[package]\nname = \"driver\"\n\n[patch.crates-io]\nstack = { path = \"../solution\" }\n";
        let stripped = strip_toml_table(manifest, "patch").unwrap();
        assert!(!stripped.contains("patch"));
        assert!(stripped.contains("name = \"driver\""));
    }

    #[test]
    fn keep_only_named_tests_drops_unlisted_tests_but_keeps_everything_else() {
        let source = r#"
            struct Session;

            impl Session {
                fn start() -> Self { Session }
            }

            #[test]
            fn insert_basic() {
                assert!(true);
            }

            #[test]
            fn balance_adversarial() {
                assert!(true);
            }
        "#;
        let keep = HashSet::from(["insert_basic".to_string()]);
        let filtered = keep_only_named_tests(source, &keep).unwrap();
        assert!(filtered.contains("fn insert_basic"));
        assert!(!filtered.contains("balance_adversarial"));
        assert!(filtered.contains("struct Session"));
    }
}
