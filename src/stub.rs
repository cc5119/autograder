//! Strips a private reference solution down to a "starter stub": every
//! unrestricted `pub` item survives with its signature intact, every
//! function/method body becomes `todo!()`, and everything else (private
//! helpers, `#[cfg(test)] mod`s, etc.) is dropped entirely.
//!
//! Only unrestricted `pub` items are kept -- that's exactly the surface an
//! external crate (the harness/driver) can already see, so anything
//! narrower (`pub(crate)`, private) is implementation detail invisible to
//! it and safe to drop.
//!
//! `use` statements are left untouched, deliberately: filtering them down
//! to only the ones the retained signatures still need would mean
//! reimplementing name resolution. Instead the caller builds the candidate
//! in a real crate and runs `cargo fix` on it (see
//! `scaffold::build_stub_source`) -- the compiler is the actual authority
//! on which imports are unused, so it prunes them, not us.
//!
//! The file's shebang and crate-level attributes (`#!...`, doc comments
//! included) are carried over verbatim -- if the instructor put something
//! there, it's their deliberate choice to include it in the starter, not
//! ours to second-guess. An instructor who doesn't want the solution's
//! crate-level doc comment shown to students should simply not write one,
//! or write a student-appropriate one instead.

use std::collections::HashSet;

use syn::{Item, ImplItem, TraitItem, Visibility};

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

/// Keeps an item iff it (or, for `impl`, its self/trait type) is part of
/// the crate's unrestricted-`pub` surface, stubbing bodies as it goes.
fn strip_item(item: Item, pub_type_names: &HashSet<String>) -> Option<Item> {
    match item {
        Item::Use(_) => Some(item),
        // `fn main` is kept even though it's never `pub` -- for a
        // `binary`-kind assignment it's the crate's mandatory entry point;
        // dropping it (as a plain private fn would be) leaves a starter
        // that doesn't compile at all (`E0601: main function not found`).
        Item::Fn(mut f) if is_pub(&f.vis) || f.sig.ident == "main" => {
            f.block = Box::new(todo_block());
            Some(Item::Fn(f))
        }
        Item::Struct(s) if is_pub(&s.vis) => Some(Item::Struct(s)),
        Item::Enum(e) if is_pub(&e.vis) => Some(Item::Enum(e)),
        Item::Union(u) if is_pub(&u.vis) => Some(Item::Union(u)),
        Item::Type(t) if is_pub(&t.vis) => Some(Item::Type(t)),
        Item::Const(c) if is_pub(&c.vis) => Some(Item::Const(c)),
        Item::Static(s) if is_pub(&s.vis) => Some(Item::Static(s)),
        Item::Trait(mut t) if is_pub(&t.vis) => {
            for trait_item in &mut t.items {
                if let TraitItem::Fn(m) = trait_item {
                    if m.default.is_some() {
                        m.default = Some(todo_block());
                    }
                }
            }
            Some(Item::Trait(t))
        }
        Item::Impl(mut imp) => {
            let self_name = type_name(imp.self_ty.as_ref());
            let trait_name = imp
                .trait_
                .as_ref()
                .and_then(|(_, path, _)| path.segments.last())
                .map(|s| s.ident.to_string());
            let is_trait_impl = imp.trait_.is_some();

            let self_is_pub = self_name.is_some_and(|n| pub_type_names.contains(&n));
            let trait_is_pub = trait_name.is_some_and(|n| pub_type_names.contains(&n));

            if !self_is_pub && !(is_trait_impl && trait_is_pub) {
                return None;
            }

            imp.items = imp
                .items
                .into_iter()
                .filter_map(|impl_item| match impl_item {
                    ImplItem::Fn(mut m) => {
                        if is_trait_impl || is_pub(&m.vis) {
                            m.block = todo_block();
                            Some(ImplItem::Fn(m))
                        } else {
                            None
                        }
                    }
                    other => Some(other),
                })
                .collect();

            Some(Item::Impl(imp))
        }
        _ => None,
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
}
