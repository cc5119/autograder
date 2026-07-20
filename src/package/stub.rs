//! Strips a private reference solution down to a "starter stub": items
//! marked `#[cfg(not(feature = "student"))]` are dropped, `cfg_select!`
//! calls are resolved to their winning arm, and anything unmarked ships
//! as-is. `cfg`/`cfg_select!` alone only gate *compilation*, not text
//! visibility -- an excluded branch is still sitting in the file as plain
//! text -- so this module does the actual text-level deletion instead of
//! just leaving the markers for `rustc` to skip.
//!
//! It repeatedly finds the outermost unresolved directive, applies it as
//! a text edit, and reparses -- reparsing is what turns a freshly
//! spliced-in `cfg_select!` arm from opaque macro-input tokens into real
//! syntax, so directives nested inside an arm (or vice versa) need no
//! special-casing. Only predicates expressible purely in `feature =
//! "student"`/`not`/`all`/`any` terms are treated as directives; anything
//! else (`#[cfg(test)]`, `#[cfg(unix)]`, ...) passes through untouched for
//! the student's own compiler to resolve later.
//!
//! Whitespace left behind by a deletion is cleaned up by the caller's
//! `rustfmt` pass, not here.

use std::collections::HashSet;

use ra_ap_syntax::{ast, ast::AstNode, Edition, NodeOrToken, SourceFile, SyntaxNode};

use crate::error::{Error, Result};

const STUDENT_FEATURE: &str = "student";

pub fn strip_to_stub(source: &str) -> Result<String> {
    let enabled: HashSet<&str> = [STUDENT_FEATURE].into_iter().collect();
    let mut source = source.to_string();
    loop {
        let parse = SourceFile::parse(&source, Edition::CURRENT);
        if !parse.errors().is_empty() {
            return Err(Error::Other(format!(
                "failed to parse solution source: {:?}",
                parse.errors()
            )));
        }
        let Some(edit) = find_edit(parse.tree().syntax(), &enabled)? else {
            return Ok(source);
        };
        let (start, end, replacement) = edit;
        source.replace_range(start..end, &replacement);
    }
}

/// `None` if `attr` isn't a `#[cfg(..)]` attribute at all.
fn cfg_predicate_text(attr: &ast::Attr) -> Option<String> {
    let ast::Meta::CfgMeta(meta) = attr.meta()? else {
        return None;
    };
    Some(meta.cfg_predicate()?.syntax().text().to_string())
}

/// `None` means the predicate touches something other than `feature =
/// ".."` (e.g. `test`) -- not a directive this tool resolves.
fn eval(predicate_text: &str, enabled: &HashSet<&str>) -> Result<Option<bool>> {
    if predicate_text.trim() == "_" {
        return Ok(Some(true)); // cfg_select!'s wildcard fallback arm
    }
    let expr = cfg_expr::Expression::parse(predicate_text)
        .map_err(|e| Error::Other(format!("invalid cfg predicate {predicate_text:?}: {e}")))?;
    let mut only_features = true;
    let result = expr.eval(|pred| match pred {
        cfg_expr::Predicate::Feature(f) => enabled.contains(f),
        _ => {
            only_features = false;
            false
        }
    });
    Ok(only_features.then_some(result))
}

/// Splits a `cfg_select! { pred => { .. } pred => { .. } _ => { .. } }`
/// token tree into `(predicate_text, body_node)` pairs, in source order.
fn cfg_select_arms(tt: &ast::TokenTree) -> Vec<(String, SyntaxNode)> {
    let mut arms = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for child in tt.syntax().children_with_tokens() {
        match child {
            NodeOrToken::Token(tok) => match tok.kind() {
                ra_ap_syntax::SyntaxKind::L_CURLY
                | ra_ap_syntax::SyntaxKind::R_CURLY
                | ra_ap_syntax::SyntaxKind::WHITESPACE => {}
                _ => pending.push(tok.text().to_string()),
            },
            NodeOrToken::Node(body) => {
                // Drop the `=>` (two separate tokens) preceding this body.
                pending.pop(); // '>'
                pending.pop(); // '='
                arms.push((pending.join(" "), body));
                pending.clear();
            }
        }
    }
    arms
}

/// Finds the first (outermost, in source order) directive this tool
/// understands and returns the `(start, end, replacement)` text edit that
/// resolves it, or `None` once the tree has none left.
fn find_edit(root: &SyntaxNode, enabled: &HashSet<&str>) -> Result<Option<(usize, usize, String)>> {
    for event in root.preorder() {
        let ra_ap_syntax::WalkEvent::Enter(node) = event else {
            continue;
        };

        if let Some(attr) = ast::Attr::cast(node.clone()) {
            if let Some(pred) = cfg_predicate_text(&attr) {
                let Some(matches) = eval(&pred, enabled)? else {
                    continue; // not one of our directives -- leave it alone
                };
                let owner = attr
                    .syntax()
                    .parent()
                    .ok_or_else(|| Error::Other("#[cfg(..)] attribute has no owner".to_string()))?;
                let range = if matches {
                    attr.syntax().text_range() // keep the owner, strip just the marker
                } else {
                    owner.text_range() // drop the whole thing
                };
                return Ok(Some((range.start().into(), range.end().into(), String::new())));
            }
            continue;
        }

        if let Some(call) = ast::MacroCall::cast(node.clone()) {
            let is_cfg_select = call.path().is_some_and(|p| p.syntax().text() == "cfg_select");
            if is_cfg_select {
                let tt = call
                    .token_tree()
                    .ok_or_else(|| Error::Other("cfg_select! call has no token tree".to_string()))?;
                let mut winner = None;
                for (pred, body) in cfg_select_arms(&tt) {
                    if eval(&pred, enabled)?.unwrap_or(false) {
                        winner = Some(body);
                        break;
                    }
                }
                let winner = winner.ok_or_else(|| {
                    Error::Other(
                        "cfg_select! has no arm matching the student build and no `_` fallback"
                            .to_string(),
                    )
                })?;
                let range = call.syntax().text_range();
                return Ok(Some((
                    range.start().into(),
                    range.end().into(),
                    winner.text().to_string(),
                )));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unannotated_items_ship_exactly_as_written() {
        let source = r#"
            pub struct Stack<T> {
                items: Vec<T>,
            }

            fn helper() -> i32 {
                42
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("pub struct Stack"));
        assert!(stub.contains("fn helper"));
        assert!(stub.contains("42"));
    }

    #[test]
    fn cfg_not_student_hides_an_item_entirely() {
        let source = r#"
            #[cfg(not(feature = "student"))]
            fn reference_only_helper() -> i32 {
                42
            }

            pub fn exposed() -> i32 {
                1
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(!stub.contains("reference_only_helper"));
        assert!(stub.contains("pub fn exposed"));
    }

    #[test]
    fn cfg_student_marker_is_stripped_when_the_item_is_kept() {
        let source = r#"
            #[cfg(not(feature = "student"))]
            fn hidden() {}

            #[cfg(feature = "student")]
            fn only_for_students() -> i32 {
                0
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("fn only_for_students"));
        assert!(!stub.contains("feature"));
        assert!(!stub.contains("cfg"));
    }

    #[test]
    fn cfg_select_swaps_a_function_body_for_the_student_view() {
        let source = r#"
            pub fn push(items: &mut Vec<i32>, value: i32) {
                cfg_select! {
                    feature = "student" => {
                        todo!()
                    }
                    _ => {
                        items.push(value);
                    }
                }
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("pub fn push"));
        assert!(stub.contains("todo!()"));
        assert!(!stub.contains("items.push(value)"));

        let parsed = SourceFile::parse(&stub, Edition::CURRENT);
        assert!(parsed.errors().is_empty(), "stub must still be valid Rust");
    }

    #[test]
    fn cfg_select_can_swap_just_one_statement_inside_a_body() {
        let source = r#"
            pub fn checksum(data: &[u8]) -> u32 {
                if data.is_empty() {
                    return 0;
                }

                cfg_select! {
                    feature = "student" => {
                        todo!()
                    }
                    _ => {
                        data.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32))
                    }
                }
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(!stub.contains("fold"));
        assert!(stub.contains("return 0"));

        let parsed = SourceFile::parse(&stub, Edition::CURRENT);
        assert!(parsed.errors().is_empty(), "stub must still be valid Rust");
    }

    #[test]
    fn cfg_test_alone_is_left_untouched_for_the_students_own_compiler() {
        let source = r#"
            #[cfg(test)]
            mod tests {
                #[test]
                fn a_public_example_test() {
                    assert_eq!(2 + 2, 4);
                }
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("mod tests"));
        assert!(stub.contains("a_public_example_test"));
    }

    #[test]
    fn stacking_cfg_test_with_cfg_not_student_hides_an_adversarial_test_module() {
        let source = r#"
            pub fn add(a: i32, b: i32) -> i32 {
                a + b
            }

            #[cfg(test)]
            #[cfg(not(feature = "student"))]
            mod tests {
                #[test]
                fn adversarial_case_that_hints_at_the_edge_case_being_graded() {
                    assert_eq!(super::add(2, 2), 4);
                }
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("pub fn add"));
        assert!(!stub.contains("adversarial_case"));
    }

    #[test]
    fn nested_cfg_inside_a_surviving_cfg_select_arm_is_still_resolved() {
        // A single `_` arm always wins for the student build we always
        // strip for, so its (initially opaque, macro-input) body is what
        // must get reparsed and re-walked for the nested `#[cfg(..)]`
        // inside it to be discovered at all.
        let source = r#"
            pub fn compute() -> i32 {
                cfg_select! {
                    _ => {
                        #[cfg(not(feature = "student"))]
                        let bonus = extra_credit_hint();

                        base_value()
                    }
                }
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(!stub.contains("extra_credit_hint"));
        assert!(stub.contains("base_value()"));

        let parsed = SourceFile::parse(&stub, Edition::CURRENT);
        assert!(parsed.errors().is_empty(), "stub must still be valid Rust");
    }

    #[test]
    fn comments_outside_touched_regions_are_preserved_verbatim() {
        let source = "// A stack.\npub struct Stack<T> {\n    items: Vec<T>,\n}\n";

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("// A stack."));
    }

    #[test]
    fn cfg_select_with_no_matching_arm_and_no_fallback_is_an_error() {
        let source = r#"
            pub fn compute() -> i32 {
                cfg_select! {
                    feature = "instructor_only" => {
                        1
                    }
                }
            }
        "#;

        let err = strip_to_stub(source).unwrap_err();
        assert!(err.to_string().contains("no arm matching"));
    }
}
