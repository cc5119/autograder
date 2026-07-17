//! A correct sample solution for the `linked-library` example assignment
//! (see `examples/linked-library-stack/README.md`): a plain LIFO stack of
//! `i64`s.

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
