//! Strips a private reference solution down to a "starter stub": every
//! unrestricted `pub` item keeps its signature with a `todo!()` body;
//! everything else is dropped. Override the default per item (fn, struct,
//! enum, union, type alias, const, static, trait, impl block/method, or
//! trait default method) with `#[doc = "autograder: keep"]` (ship as-is),
//! `#[doc = "autograder: stub"]` (force a `todo!()` even on a private
//! item), or `#[doc = "autograder: hide"]` (drop even if `pub`) -- the
//! marker itself is always stripped from the output.
//!
//! `keep` on a fn/method also unlocks the same three markers on individual
//! statements inside its body (only inside a `keep`-marked body -- a
//! `stub`/`hide` body has nothing left to mark). Wrap several statements in
//! a `{ ... }` block and mark that to hide/stub them together. `use`
//! statements always pass through untouched; the caller runs `cargo fix`
//! on the result to prune whatever became unused (see
//! `publish::run_cargo_fix`) rather than this module reimplementing name
//! resolution. The shebang and crate-level attributes are carried over
//! verbatim.

use std::collections::HashSet;

use syn::{
    Attribute, Block, Expr, ExprLit, ImplItem, Item, Lit, Local, Meta, Stmt, StmtMacro, TraitItem,
    Visibility,
};

use crate::error::{Error, Result};

pub fn strip_to_stub(source: &str) -> Result<String> {
    let file = syn::parse_file(source)
        .map_err(|e| Error::Other(format!("failed to parse solution source: {e}")))?;

    let pub_type_names = collect_pub_type_names(&file);

    let items = file
        .items
        .into_iter()
        .filter_map(|item| strip_item(item, &pub_type_names))
        .collect();

    let stripped = syn::File {
        shebang: file.shebang,
        attrs: file.attrs,
        items,
    };

    Ok(prettyplease::unparse(&stripped))
}

fn is_pub(vis: &Visibility) -> bool {
    matches!(vis, Visibility::Public(_))
}

fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

fn collect_pub_type_names(file: &syn::File) -> HashSet<String> {
    file.items
        .iter()
        .filter_map(|item| match item {
            Item::Struct(s) if is_pub(&s.vis) => Some(s.ident.to_string()),
            Item::Enum(e) if is_pub(&e.vis) => Some(e.ident.to_string()),
            Item::Union(u) if is_pub(&u.vis) => Some(u.ident.to_string()),
            Item::Type(t) if is_pub(&t.vis) => Some(t.ident.to_string()),
            Item::Trait(t) if is_pub(&t.vis) => Some(t.ident.to_string()),
            _ => None,
        })
        .collect()
}

fn todo_block() -> syn::Block {
    syn::parse_quote! {{ todo!() }}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Directive {
    Keep,
    Stub,
    Hide,
}

/// Removes and returns the first recognized marker in `attrs`, if any.
fn take_directive(attrs: &mut Vec<Attribute>) -> Option<Directive> {
    let idx = attrs.iter().position(|a| directive_of(a).is_some())?;
    let attr = attrs.remove(idx);
    directive_of(&attr)
}

fn directive_of(attr: &Attribute) -> Option<Directive> {
    if !attr.path().is_ident("doc") {
        return None;
    }
    let Meta::NameValue(name_value) = &attr.meta else {
        return None;
    };
    let Expr::Lit(ExprLit {
        lit: Lit::Str(s), ..
    }) = &name_value.value
    else {
        return None;
    };
    let rest = s
        .value()
        .trim()
        .strip_prefix("autograder:")?
        .trim()
        .to_string();
    match rest.as_str() {
        "keep" => Some(Directive::Keep),
        "stub" => Some(Directive::Stub),
        "hide" => Some(Directive::Hide),
        _ => None,
    }
}

fn strip_item(mut item: Item, pub_type_names: &HashSet<String>) -> Option<Item> {
    let directive = item_attrs_mut(&mut item).and_then(take_directive);

    match item {
        Item::Use(_) => Some(item),
        Item::Fn(mut f) => match directive {
            Some(Directive::Hide) => None,
            Some(Directive::Keep) => {
                f.block = Box::new(strip_block_statements(*f.block));
                Some(Item::Fn(f))
            }
            Some(Directive::Stub) => {
                f.block = Box::new(todo_block());
                Some(Item::Fn(f))
            }
            // `fn main` is kept even though it's never `pub` -- for a
            // `binary`-kind assignment it's the crate's mandatory entry
            // point; dropping it (as a plain private fn would be) leaves
            // a starter that doesn't compile at all (`E0601`).
            None if is_pub(&f.vis) || f.sig.ident == "main" => {
                f.block = Box::new(todo_block());
                Some(Item::Fn(f))
            }
            None => None,
        },
        Item::Struct(s) => keep_or_drop(directive, is_pub(&s.vis), Item::Struct(s)),
        Item::Enum(e) => keep_or_drop(directive, is_pub(&e.vis), Item::Enum(e)),
        Item::Union(u) => keep_or_drop(directive, is_pub(&u.vis), Item::Union(u)),
        Item::Type(t) => keep_or_drop(directive, is_pub(&t.vis), Item::Type(t)),
        Item::Const(c) => keep_or_drop(directive, is_pub(&c.vis), Item::Const(c)),
        Item::Static(s) => keep_or_drop(directive, is_pub(&s.vis), Item::Static(s)),
        Item::Trait(mut t) => {
            if directive == Some(Directive::Hide) || !(directive.is_some() || is_pub(&t.vis)) {
                return None;
            }
            for trait_item in &mut t.items {
                if let TraitItem::Fn(m) = trait_item {
                    match take_directive(&mut m.attrs) {
                        // No body to hide outright for a required (no
                        // default) method -- dropping the default is the
                        // closest equivalent for a method that has one.
                        Some(Directive::Hide) => m.default = None,
                        Some(Directive::Keep) => {}
                        Some(Directive::Stub) | None => {
                            if m.default.is_some() {
                                m.default = Some(todo_block());
                            }
                        }
                    }
                }
            }
            Some(Item::Trait(t))
        }
        Item::Impl(mut imp) => {
            if directive == Some(Directive::Hide) {
                return None;
            }
            let force_include = matches!(directive, Some(Directive::Keep | Directive::Stub));

            let self_name = type_name(imp.self_ty.as_ref());
            let trait_name = imp
                .trait_
                .as_ref()
                .and_then(|(_, path, _)| path.segments.last())
                .map(|s| s.ident.to_string());
            let is_trait_impl = imp.trait_.is_some();

            let self_is_pub = self_name.is_some_and(|n| pub_type_names.contains(&n));
            let trait_is_pub = trait_name.is_some_and(|n| pub_type_names.contains(&n));

            if !force_include && !self_is_pub && !(is_trait_impl && trait_is_pub) {
                return None;
            }

            imp.items = imp
                .items
                .into_iter()
                .filter_map(|impl_item| match impl_item {
                    ImplItem::Fn(mut m) => match take_directive(&mut m.attrs) {
                        Some(Directive::Hide) => None,
                        Some(Directive::Keep) => {
                            m.block = strip_block_statements(m.block);
                            Some(ImplItem::Fn(m))
                        }
                        Some(Directive::Stub) => {
                            m.block = todo_block();
                            Some(ImplItem::Fn(m))
                        }
                        // Unmarked: an impl-level `keep` means "this whole
                        // impl is real code", so unmarked methods inherit it.
                        None if is_trait_impl || is_pub(&m.vis) || force_include => {
                            m.block = if directive == Some(Directive::Keep) {
                                strip_block_statements(m.block)
                            } else {
                                todo_block()
                            };
                            Some(ImplItem::Fn(m))
                        }
                        None => None,
                    },
                    other => Some(other),
                })
                .collect();

            Some(Item::Impl(imp))
        }
        _ => None,
    }
}

fn keep_or_drop(directive: Option<Directive>, is_pub: bool, item: Item) -> Option<Item> {
    match directive {
        Some(Directive::Hide) => None,
        Some(Directive::Keep) | Some(Directive::Stub) => Some(item),
        None if is_pub => Some(item),
        None => None,
    }
}

fn item_attrs_mut(item: &mut Item) -> Option<&mut Vec<Attribute>> {
    match item {
        Item::Fn(i) => Some(&mut i.attrs),
        Item::Struct(i) => Some(&mut i.attrs),
        Item::Enum(i) => Some(&mut i.attrs),
        Item::Union(i) => Some(&mut i.attrs),
        Item::Type(i) => Some(&mut i.attrs),
        Item::Const(i) => Some(&mut i.attrs),
        Item::Static(i) => Some(&mut i.attrs),
        Item::Trait(i) => Some(&mut i.attrs),
        Item::Impl(i) => Some(&mut i.attrs),
        _ => None,
    }
}

fn strip_block_statements(mut block: Block) -> Block {
    block.stmts = block
        .stmts
        .into_iter()
        .filter_map(|mut stmt| {
            let directive = stmt_attrs_mut(&mut stmt).and_then(take_directive);
            match directive {
                Some(Directive::Hide) => None,
                Some(Directive::Stub) => Some(todo_stmt(&stmt)),
                Some(Directive::Keep) | None => Some(stmt),
            }
        })
        .collect();
    block
}

/// `Stmt::Item` (a nested item declaration) can't carry a marker.
fn stmt_attrs_mut(stmt: &mut Stmt) -> Option<&mut Vec<Attribute>> {
    match stmt {
        Stmt::Local(Local { attrs, .. }) => Some(attrs),
        Stmt::Macro(StmtMacro { attrs, .. }) => Some(attrs),
        Stmt::Expr(expr, _) => expr_attrs_mut(expr),
        Stmt::Item(_) => None,
    }
}

/// `syn::Expr` has no blanket `.attrs` accessor, so this covers each
/// variant by hand (`_ => None` is required since `Expr` is
/// `#[non_exhaustive]`; that arm just means that kind can't carry a marker).
fn expr_attrs_mut(expr: &mut Expr) -> Option<&mut Vec<Attribute>> {
    match expr {
        Expr::Array(e) => Some(&mut e.attrs),
        Expr::Assign(e) => Some(&mut e.attrs),
        Expr::Async(e) => Some(&mut e.attrs),
        Expr::Await(e) => Some(&mut e.attrs),
        Expr::Binary(e) => Some(&mut e.attrs),
        Expr::Block(e) => Some(&mut e.attrs),
        Expr::Break(e) => Some(&mut e.attrs),
        Expr::Call(e) => Some(&mut e.attrs),
        Expr::Cast(e) => Some(&mut e.attrs),
        Expr::Closure(e) => Some(&mut e.attrs),
        Expr::Const(e) => Some(&mut e.attrs),
        Expr::Continue(e) => Some(&mut e.attrs),
        Expr::Field(e) => Some(&mut e.attrs),
        Expr::ForLoop(e) => Some(&mut e.attrs),
        Expr::Group(e) => Some(&mut e.attrs),
        Expr::If(e) => Some(&mut e.attrs),
        Expr::Index(e) => Some(&mut e.attrs),
        Expr::Let(e) => Some(&mut e.attrs),
        Expr::Lit(e) => Some(&mut e.attrs),
        Expr::Loop(e) => Some(&mut e.attrs),
        Expr::Macro(e) => Some(&mut e.attrs),
        Expr::Match(e) => Some(&mut e.attrs),
        Expr::MethodCall(e) => Some(&mut e.attrs),
        Expr::Paren(e) => Some(&mut e.attrs),
        Expr::Path(e) => Some(&mut e.attrs),
        Expr::Range(e) => Some(&mut e.attrs),
        Expr::Reference(e) => Some(&mut e.attrs),
        Expr::Repeat(e) => Some(&mut e.attrs),
        Expr::Return(e) => Some(&mut e.attrs),
        Expr::Struct(e) => Some(&mut e.attrs),
        Expr::Try(e) => Some(&mut e.attrs),
        Expr::TryBlock(e) => Some(&mut e.attrs),
        Expr::Tuple(e) => Some(&mut e.attrs),
        Expr::Unary(e) => Some(&mut e.attrs),
        Expr::Unsafe(e) => Some(&mut e.attrs),
        Expr::While(e) => Some(&mut e.attrs),
        Expr::Yield(e) => Some(&mut e.attrs),
        _ => None,
    }
}

/// Replaces a statement with `todo!()`, preserving tail position (no `;`)
/// so its bottom type still unifies with whatever the block returns.
fn todo_stmt(original: &Stmt) -> Stmt {
    let has_semi = match original {
        Stmt::Local(_) => true,
        Stmt::Macro(m) => m.semi_token.is_some(),
        Stmt::Expr(_, semi) => semi.is_some(),
        Stmt::Item(_) => true,
    };
    if has_semi {
        syn::parse_quote! { todo!(); }
    } else {
        // A bare `todo!()` doesn't parse as a standalone `Stmt`; parsing a
        // one-statement `Block` instead and unwrapping it does.
        let block: Block = syn::parse_quote! {{ todo!() }};
        block
            .stmts
            .into_iter()
            .next()
            .expect("block has exactly one statement")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_pub_signatures_and_stubs_bodies() {
        let source = r#"
            use std::collections::VecDeque;

            /// A stack.
            pub struct Stack<T> {
                items: VecDeque<T>,
            }

            impl<T> Stack<T> {
                pub fn new() -> Self {
                    Stack { items: VecDeque::new() }
                }

                pub fn push(&mut self, value: T) {
                    self.items.push_back(value);
                }

                fn helper(&self) -> usize {
                    self.items.len() * 2
                }
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("pub struct Stack"));
        assert!(stub.contains("pub fn new"));
        assert!(stub.contains("pub fn push"));
        assert!(stub.contains("todo!()"));
        assert!(!stub.contains("VecDeque::new"));
        assert!(!stub.contains("push_back"));
        assert!(!stub.contains("fn helper"));
    }

    #[test]
    fn drops_private_items_entirely() {
        let source = r#"
            fn private_helper() -> i32 { 42 }

            struct Internal {
                secret: i32,
            }

            pub fn exposed() -> i32 {
                private_helper()
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("pub fn exposed"));
        assert!(!stub.contains("private_helper"));
        assert!(!stub.contains("Internal"));
        assert!(!stub.contains("secret"));
    }

    #[test]
    fn keeps_main_even_though_it_is_never_pub() {
        let source = r#"
            use std::env;

            fn helper() -> i32 { 42 }

            fn main() {
                println!("{}", helper());
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("fn main"));
        assert!(stub.contains("todo!()"));
        assert!(!stub.contains("fn helper"));
        assert!(!stub.contains("println"));
    }

    #[test]
    fn drops_test_modules() {
        let source = r#"
            pub fn add(a: i32, b: i32) -> i32 { a + b }

            #[cfg(test)]
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
    fn keep_directive_ships_a_private_fn_with_its_real_body() {
        let source = r#"
            #[doc = "autograder: keep"]
            fn shared_helper() -> i32 { 42 }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("fn shared_helper"));
        assert!(stub.contains("42"));
        assert!(!stub.contains("todo!()"));
        assert!(!stub.contains("autograder"));
    }

    #[test]
    fn stub_directive_forces_a_private_fn_to_become_a_fill_in_the_blank() {
        let source = r#"
            #[doc = "autograder: stub"]
            fn compute_checksum(data: &[u8]) -> u32 { 0 }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("fn compute_checksum"));
        assert!(stub.contains("todo!()"));
        assert!(!stub.contains(" 0 "));
    }

    #[test]
    fn hide_directive_drops_an_otherwise_pub_item() {
        let source = r#"
            #[doc = "autograder: hide"]
            pub fn internal_grading_helper() -> i32 { 7 }

            pub fn exposed() -> i32 { 1 }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(!stub.contains("internal_grading_helper"));
        assert!(stub.contains("pub fn exposed"));
    }

    #[test]
    fn statement_hide_removes_just_that_statement_from_a_kept_body() {
        let source = r#"
            #[doc = "autograder: keep"]
            pub fn push(items: &mut Vec<i32>, value: i32) {
                #[doc = "autograder: hide"]
                debug_assert!(items.len() < 1000, "internal invariant");

                items.push(value);
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(!stub.contains("debug_assert"));
        assert!(stub.contains("items.push(value)"));
        assert!(!stub.contains("autograder"));
    }

    #[test]
    fn statement_stub_replaces_a_tail_expression_preserving_tail_position() {
        let source = r#"
            #[doc = "autograder: keep"]
            pub fn checksum(data: &[u8]) -> u32 {
                if data.is_empty() {
                    return 0;
                }

                #[doc = "autograder: stub"]
                data.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32))
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(!stub.contains("fold"));
        // The replacement must stay in tail position (no trailing `;`) so
        // it still type-unifies with the fn's `-> u32` return type; a
        // `todo!();` statement there would leave the block's value `()`.
        assert!(!stub.contains("todo!();\n}"));

        let parsed = syn::parse_file(&stub);
        assert!(parsed.is_ok(), "stubbed output must still be valid Rust");
    }

    #[test]
    fn block_grouping_hides_several_statements_at_once() {
        let source = r#"
            #[doc = "autograder: keep"]
            pub fn push(&mut self, value: i32) {
                #[doc = "autograder: hide"]
                {
                    debug_assert!(self.items.len() < self.capacity, "internal invariant");
                    self.metrics.record_push();
                }

                self.items.push(value);
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(!stub.contains("debug_assert"));
        assert!(!stub.contains("record_push"));
        assert!(stub.contains("self.items.push(value)"));
    }

    #[test]
    fn impl_level_keep_includes_an_otherwise_private_impl_block() {
        let source = r#"
            struct Internal {
                value: i32,
            }

            #[doc = "autograder: keep"]
            impl Internal {
                fn new(value: i32) -> Self {
                    Internal { value }
                }
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("impl Internal"));
        assert!(stub.contains("fn new"));
        assert!(stub.contains("Internal { value }"));
    }

    #[test]
    fn trait_default_method_hide_drops_just_that_default() {
        let source = r#"
            pub trait Greeter {
                fn name(&self) -> String;

                #[doc = "autograder: hide"]
                fn greet(&self) -> String {
                    format!("Hello, {}!", self.name())
                }
            }
        "#;

        let stub = strip_to_stub(source).unwrap();
        assert!(stub.contains("fn name(&self) -> String;"));
        assert!(stub.contains("fn greet(&self) -> String;"));
        assert!(!stub.contains("format!"));
    }
}
