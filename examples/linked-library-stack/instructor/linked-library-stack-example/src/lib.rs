//! A correct reference solution for the `linked-library-stack` example
//! assignment (see the top-level README's "Try it" section): a plain LIFO
//! stack of `i64`s. This directory is named after `[assignment].id` in
//! `../autograder.toml` (`linked-library-stack-example`) -- `autograder
//! scaffold` requires that to derive the starter's src/ from it, and
//! `../harness/Cargo.toml` depends on + patches this same name by default,
//! so `cd ../harness && cargo nextest run` works standalone, no autograder
//! involvement.

pub struct Stack {
    items: Vec<i64>,
}

impl Stack {
    pub fn new() -> Self {
        Stack { items: Vec::new() }
    }

    pub fn push(&mut self, value: i64) {
        self.items.push(value);
    }

    pub fn pop(&mut self) -> Option<i64> {
        self.items.pop()
    }

    pub fn peek(&self) -> Option<i64> {
        self.items.last().copied()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

impl Default for Stack {
    fn default() -> Self {
        Self::new()
    }
}
