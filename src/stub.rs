//! Strips a private reference solution down to a "starter stub". Default
//! policy is visibility-based: every unrestricted `pub` item survives with
//! its signature intact and a `todo!()` body; everything else (private
//! helpers, `#[cfg(test)] mod`s, etc.) is dropped entirely.
//!
//! Only unrestricted `pub` items are kept by default -- that's exactly the
//! surface an external crate (the harness/driver) can already see, so
//! anything narrower (`pub(crate)`, private) is implementation detail
//! invisible to it and safe to drop.
//!
//! For finer control than the visibility default gives you, mark an item
//! (a fn, struct, enum, union, type alias, const, static, trait, impl
//! block, impl method, or trait default method) with
//! `#[doc = "autograder: keep"]`, `#[doc = "autograder: stub"]`, or
//! `#[doc = "autograder: hide"]`. Each is a no-op in some cases (see
//! below) and does something new in others:
//!  - `keep` -- ship this exactly as-is, real body untouched. A no-op on
//!    anything already kept in full by default (a `pub` struct/enum/
//!    const/...); does something new on a `pub` fn (skips the automatic
//!    `todo!()` stubbing -- e.g. boilerplate like `pub fn new()` that
//!    isn't the graded exercise) or on a private item (rescues it from
//!    being dropped, with real code -- e.g. a private helper students need
//!    but shouldn't have to write).
//!  - `stub` -- ship this with a `todo!()` body, i.e. make it a required
//!    fill-in-the-blank. A no-op on a `pub` fn (already the default) and
//!    on anything with no body to blank (struct/enum/const/...); does
//!    something new on a **private** fn/method: forces it to appear in
//!    the starter as its own exercise instead of silently vanishing (e.g.
//!    breaking a big problem into named sub-parts: `partition` as its own
//!    stub, `quicksort` calling it).
//!  - `hide` -- never include this, even if `pub`. A no-op on anything
//!    already dropped by default (private, unmarked); does something new
//!    on a `pub` item that exists only for the harness to call and isn't
//!    meant for students to see.
//!
//! The marker attribute itself is always stripped from the output. Plain
//! `#[doc = "..."]` was chosen over `///`-sugar so a marker reads
//! unambiguously as a directive rather than prose -- both desugar to the
//! same attribute, so `///` still works if preferred, but this crate's
//! own examples all use the explicit form.
//!
//! ## Statement-level markers
//!
//! `keep` on a function/method additionally unlocks statement-level
//! control *inside* that body: any individual `let` binding, expression
//! statement, or macro-invocation statement can itself carry `hide` (drop
//! just that one statement) or `stub` (replace just that one statement
//! with `todo!()`), independently of its neighbors. `stub`/`hide` on a
//! statement only do anything inside a `keep`-marked body: a body with no
//! item-level `keep` is either already fully `todo!()`-ed (nothing left
//! to mark) or already fully dropped, so a nested marker has nowhere to
//! attach in the emitted stub either way.
//!
//! Hiding an instructor-only invariant check while keeping the rest of a
//! method real:
//!
//! ```ignore
//! #[doc = "autograder: keep"]
//! pub fn push(&mut self, value: T) {
//!     #[doc = "autograder: hide"]
//!     debug_assert!(self.items.len() < self.capacity, "internal invariant");
//!
//!     self.items.push_back(value);
//! }
//! ```
//!
//! becomes, in the starter:
//!
//! ```ignore
//! pub fn push(&mut self, value: T) {
//!     self.items.push_back(value);
//! }
//! ```
//!
//! Stubbing out just the core computation, by marking its tail expression
//! (no trailing `;`, so the replacement stays in tail position too and
//! `todo!()`'s bottom type still unifies with the return type):
//!
//! ```ignore
//! #[doc = "autograder: keep"]
//! pub fn checksum(data: &[u8]) -> u32 {
//!     if data.is_empty() {
//!         return 0;
//!     }
//!
//!     #[doc = "autograder: stub"]
//!     data.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32))
//! }
//! ```
//!
//! becomes:
//!
//! ```ignore
//! pub fn checksum(data: &[u8]) -> u32 {
//!     if data.is_empty() {
//!         return 0;
//!     }
//!     todo!()
//! }
//! ```
//!
//! A marker only removes/replaces the one statement it's attached to; it
//! doesn't truncate the rest of the block. To hide/stub several
//! *contiguous* statements at once, wrap them in a `{ ... }` block
//! expression and mark that block -- it's a single statement carrying a
//! single marker, same as marking a whole `if`/`while`/`match` already
//! covers everything nested inside it:
//!
//! ```ignore
//! #[doc = "autograder: hide"]
//! {
//!     debug_assert!(self.items.len() < self.capacity, "internal invariant");
//!     self.metrics.record_push();
//! }
//! ```
//!
//! It's the instructor's job to pick statements/items whose removal still
//! leaves valid, sensible Rust (don't stub a `let` whose binding a later
//! statement still references, and remember `keep` isn't transitive -- a
//! kept item calling an unmarked private helper still loses that helper).
//!
//! A marker compiles fine in the reference solution itself (`#[doc =
//! ...]` is always valid, inert Rust -- no dependency, no dedicated
//! attribute macro needed), but rustc's `unused_doc_comments` lint fires
//! on a statement-level one, since docs aren't rendered there. Add
//! `#![allow(unused_doc_comments)]` at the solution crate's root if the
//! warning noise bothers you.
//!
//! `use` statements are left untouched, deliberately: filtering them down
//! to only the ones the retained signatures still need would mean
//! reimplementing name resolution. Instead the caller builds the candidate
//! in a real crate and runs `cargo fix` on it (see
//! `publish::run_cargo_fix`) -- the compiler is the actual authority
//! on which imports are unused, so it prunes them, not us.
//!
//! The file's shebang and crate-level attributes (`#!...`, doc comments
//! included) are carried over verbatim -- if the instructor put something
//! there, it's their deliberate choice to include it in the starter, not
//! ours to second-guess. An instructor who doesn't want the solution's
//! crate-level doc comment shown to students should simply not write one,
//! or write a student-appropriate one instead.

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

/// An `#[doc = "autograder: <word>"]` marker, overriding the default
/// visibility-based keep/drop decision for the item or statement it's
/// attached to. See this module's doc comment for the full semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Directive {
    Keep,
    Stub,
    Hide,
}

/// Scans `attrs` for a recognized marker, removing and returning it if
/// found -- the marker never survives into the emitted stub, on items or
/// statements alike.
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

/// Keeps an item iff it (or, for `impl`, its self/trait type) is part of
/// the crate's unrestricted-`pub` surface, stubbing bodies as it goes --
/// unless a `Directive` on the item overrides that decision outright.
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
                        // No marker of its own: included either because
                        // it's naturally visible (pub method of a pub
                        // impl) or because the *impl block* forced
                        // inclusion -- either way its body defaults to
                        // stubbed, unless the impl-level directive was
                        // specifically `keep`, in which case an unmarked
                        // method inherits "keep" too (an impl marked
                        // `keep` means "this whole impl is real code",
                        // not "real except for whichever methods I forgot
                        // to mark").
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

/// `keep`/`stub` both mean "include, as-is" for an item with no body of
/// its own to blank (struct/enum/union/type/const/static); `hide` always
/// excludes; absent a directive, falls back to the visibility default.
fn keep_or_drop(directive: Option<Directive>, is_pub: bool, item: Item) -> Option<Item> {
    match directive {
        Some(Directive::Hide) => None,
        Some(Directive::Keep) | Some(Directive::Stub) => Some(item),
        None if is_pub => Some(item),
        None => None,
    }
}

/// The attrs list of whichever `Item` variant carries a directive marker,
/// so `strip_item` can extract it once up front regardless of item kind.
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

/// Applies statement-level `stub`/`hide` markers within a `keep`-marked
/// function/method body (see this module's doc comment). Statements
/// without a marker (the common case) pass through untouched.
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

/// The attrs list of whichever `Stmt` variant carries a directive marker.
/// `Stmt::Item` (a nested item declaration) is out of scope -- markers on
/// nested items aren't supported, only on the fn-body statements around
/// them.
fn stmt_attrs_mut(stmt: &mut Stmt) -> Option<&mut Vec<Attribute>> {
    match stmt {
        Stmt::Local(Local { attrs, .. }) => Some(attrs),
        Stmt::Macro(StmtMacro { attrs, .. }) => Some(attrs),
        Stmt::Expr(expr, _) => expr_attrs_mut(expr),
        Stmt::Item(_) => None,
    }
}

/// `syn::Expr`'s ~40 variants each carry their own `attrs: Vec<Attribute>`
/// field (no blanket accessor exists in syn's public API); this covers
/// every statement-position expression a solution realistically uses --
/// notably `Expr::Block`, which is what makes the "wrap several
/// statements in `{ }` and mark the block" grouping trick work, since a
/// block is just another markable expression like any other. `_ => None`
/// (required: `Expr` is `#[non_exhaustive]`) just means that expression
/// kind can't carry a marker -- harmless, it's simply never recognized as
/// one.
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

/// Replaces a statement with `todo!()`, preserving whether it was in tail
/// position (no trailing `;`, its value *is* the block's value -- so the
/// replacement must stay a bare tail expression too, letting `todo!()`'s
/// bottom type unify with whatever the block was supposed to produce) or
/// ordinary statement position (trailing `;`, replaced with `todo!();`).
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
        // A bare `todo!()` with no trailing `;` doesn't parse as a
        // standalone `Stmt` -- outside a block, the parser can't tell
        // whether it's in tail position (no `;` needed) without more
        // context. Parsing it as a one-statement `Block` instead gives it
        // that context, then unwrapping recovers the `Stmt`.
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
