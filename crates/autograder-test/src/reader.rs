//! On-demand parsing of a whitespace-separated stream -- a command's
//! output, or the driver's own standard input.
//!
//! The mirror image of [`crate::Args`], with one deliberate difference in
//! tone: a bad *argument* is a bug in the harness, so [`crate::Args`] can
//! afford a terse panic, but bad *output* is the very thing being graded.
//! Every panic here therefore carries the position and the surrounding
//! text, because that panic text is what the student ends up reading.

use std::fmt::Debug;
use std::str::FromStr;

/// Whether a bulk read may cross a line boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Stop at the newline: the read never leaves the current line.
    Line,
    /// Newlines are ordinary whitespace: the read runs to the end of the
    /// output.
    All,
}

/// A cursor over one stream, tokenized on whitespace and parsed on
/// demand:
///
/// ```ignore
/// let out = autograder_test::cmd!("solver").arg(n).run().assert_success();
/// let mut r = out.stdout_reader();
///
/// let n: usize = r.read();
/// let [w, h]: [u32; 2] = r.take_arr(Scope::Line);
/// let row: Vec<i32> = r.rest(Scope::Line);
/// r.next_line();
/// r.end();
/// ```
///
/// [`Reader::stdin`] gives the same cursor over the driver's own standard
/// input.
pub struct Reader {
    text: String,
    pos: usize,
}

impl Reader {
    pub fn new(text: impl Into<String>) -> Self {
        Reader {
            text: text.into(),
            pos: 0,
        }
    }

    /// A cursor over all of standard input, read to EOF -- the stdin
    /// mirror of [`crate::Args`], for a driver fed on stdin rather than
    /// argv. Reading to EOF up front means this cannot drive an
    /// interactive exchange, only a batch of input.
    pub fn stdin() -> Self {
        let text = std::io::read_to_string(std::io::stdin()).expect("could not read stdin");
        Reader::new(text)
    }

    /// The next whitespace-separated token, crossing newlines freely.
    /// Panics if the stream is exhausted or the token does not parse.
    pub fn read<T: FromStr<Err: Debug>>(&mut self) -> T {
        match self.try_read() {
            Some(v) => v,
            None => panic!(
                "expected {}, but reached the end{}",
                std::any::type_name::<T>(),
                self.context()
            ),
        }
    }

    /// The next whitespace-separated token, or `None` once nothing is
    /// left -- for a stream whose length is not known in advance. Only
    /// the end is soft: a token that is there but does not parse still
    /// panics, since that is a real mistake rather than a stopping point.
    pub fn try_read<T: FromStr<Err: Debug>>(&mut self) -> Option<T> {
        let tok = self.next_token(Scope::All)?;
        Some(self.parse(tok))
    }

    /// Exactly `n` tokens. Running out -- of output, or of line when
    /// `scope` is [`Scope::Line`] -- panics rather than returning fewer.
    pub fn take<T: FromStr<Err: Debug>>(&mut self, n: usize, scope: Scope) -> Vec<T> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            match self.next_token(scope) {
                Some(tok) => out.push(self.parse(tok)),
                None => panic!(
                    "expected {n} values{}, but found only {i}{}",
                    match scope {
                        Scope::Line => " on this line",
                        Scope::All => "",
                    },
                    self.context()
                ),
            }
        }
        out
    }

    /// Exactly `N` tokens, with the same all-or-panic contract as
    /// [`Reader::take`].
    pub fn take_arr<const N: usize, T: FromStr<Err: Debug>>(&mut self, scope: Scope) -> [T; N] {
        // `from_fn` calls in order of increasing index, so the tokens land
        // in the array in the order they were printed.
        std::array::from_fn(|_| match self.next_token(scope) {
            Some(tok) => self.parse(tok),
            None => panic!(
                "expected {N} values{}, but reached the end{}",
                match scope {
                    Scope::Line => " on this line",
                    Scope::All => "",
                },
                self.context()
            ),
        })
    }

    /// Every remaining token -- on the current line for [`Scope::Line`], in
    /// the whole output for [`Scope::All`]. Empty is not an error.
    pub fn rest<T: FromStr<Err: Debug>>(&mut self, scope: Scope) -> Vec<T> {
        let mut out = Vec::new();
        while let Some(tok) = self.next_token(scope) {
            out.push(self.parse(tok));
        }
        out
    }

    /// Advances past the current line, returning whatever was left on it,
    /// newline excluded. A [`Scope::Line`] read stops dead at the newline,
    /// so this is how a line-at-a-time loop moves forward.
    pub fn next_line(&mut self) -> String {
        let start = self.pos;
        let end = match self.text[start..].find('\n') {
            Some(i) => {
                self.pos = start + i + 1;
                start + i
            }
            None => {
                self.pos = self.text.len();
                self.pos
            }
        };
        self.text[start..end].to_string()
    }

    /// True when nothing but whitespace remains.
    pub fn is_empty(&self) -> bool {
        self.text[self.pos..].trim().is_empty()
    }

    /// Asserts everything has been consumed -- catches stray debug prints
    /// after the values the judge asked for.
    pub fn end(&self) {
        let left = self.text[self.pos..].trim();
        assert!(
            left.is_empty(),
            "expected nothing more, but found {:?}{}",
            truncate(left),
            self.context()
        );
    }

    /// The next token under `scope`, advancing past it. Leading whitespace
    /// is skipped, but under [`Scope::Line`] the skip stops at the newline
    /// without consuming it, so the cursor stays on the current line.
    fn next_token(&mut self, scope: Scope) -> Option<(usize, usize)> {
        for (i, c) in self.text[self.pos..].char_indices() {
            if c == '\n' && scope == Scope::Line {
                self.pos += i;
                return None;
            }
            if !c.is_whitespace() {
                let start = self.pos + i;
                let end = self.text[start..]
                    .find(char::is_whitespace)
                    .map(|j| start + j)
                    .unwrap_or(self.text.len());
                self.pos = end;
                return Some((start, end));
            }
        }
        self.pos = self.text.len();
        None
    }

    fn parse<T: FromStr<Err: Debug>>(&self, (start, end): (usize, usize)) -> T {
        let tok = &self.text[start..end];
        match tok.parse() {
            Ok(v) => v,
            Err(e) => panic!(
                "could not parse {tok:?} as {}: {e:?}{}",
                std::any::type_name::<T>(),
                self.context_at(start)
            ),
        }
    }

    fn context(&self) -> String {
        self.context_at(self.pos)
    }

    /// ` (at line 3, column 5)` plus the offending line, so the panic
    /// points at a place in the output rather than just a byte offset.
    fn context_at(&self, at: usize) -> String {
        let at = at.min(self.text.len());
        let line_no = self.text[..at].matches('\n').count() + 1;
        let line_start = self.text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = self.text[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.text.len());
        let column = self.text[line_start..at].chars().count() + 1;
        format!(
            " (at line {line_no}, column {column})\nline {line_no}: {:?}",
            truncate(&self.text[line_start..line_end])
        )
    }
}

fn truncate(s: &str) -> String {
    const MAX: usize = 80;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}...")
}

/// Compares `actual` against `expected` line by line, ignoring trailing
/// whitespace on each line and any trailing blank lines -- the newline at
/// the very end of a program's output is the one difference that is
/// almost never what a task actually cares about. Panics naming the first
/// line that differs.
pub(crate) fn assert_matches(stream: &str, actual: &str, expected: &str) {
    fn strip(s: &str) -> Vec<&str> {
        let mut lines: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines
    }
    let (a, e) = (strip(actual), strip(expected));
    if a == e {
        return;
    }

    let at = (0..a.len().max(e.len()))
        .find(|&i| a.get(i) != e.get(i))
        .unwrap();
    let show = |lines: &[&str], i: usize| match lines.get(i) {
        Some(l) => format!("{:?}", truncate(l)),
        None => "<no more output>".to_string(),
    };
    panic!(
        "{stream} did not match at line {}:\n  expected: {}\n    actual: {}\n\
         (expected {} lines, got {})\n\nfull {stream}:\n{}",
        at + 1,
        show(&e, at),
        show(&a, at),
        e.len(),
        a.len(),
        actual,
    );
}

/// Exact byte comparison, for a task that really does grade formatting.
pub(crate) fn assert_matches_exactly(stream: &str, actual: &str, expected: &str) {
    assert!(
        actual == expected,
        "{stream} did not match exactly:\n  expected: {expected:?}\n    actual: {actual:?}",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_crosses_lines() {
        let mut r = Reader::new("1 2\n3\n");
        let v: Vec<i32> = (0..3).map(|_| r.read()).collect();
        assert_eq!(v, [1, 2, 3]);
        assert!(r.is_empty());
        r.end();
    }

    #[test]
    fn line_scope_stops_at_newline() {
        let mut r = Reader::new("1 2\n3 4\n");
        assert_eq!(r.rest::<i32>(Scope::Line), [1, 2]);
        // Stuck on the exhausted line until asked to move on.
        assert!(r.rest::<i32>(Scope::Line).is_empty());
        r.next_line();
        assert_eq!(r.rest::<i32>(Scope::Line), [3, 4]);
    }

    #[test]
    fn all_scope_takes_everything() {
        let mut r = Reader::new("1 2\n3 4");
        assert_eq!(r.rest::<i32>(Scope::All), [1, 2, 3, 4]);
    }

    #[test]
    fn take_arr_reads_exactly_n() {
        let mut r = Reader::new("10 20 30\n");
        let [a, b]: [u32; 2] = r.take_arr(Scope::Line);
        assert_eq!((a, b), (10, 20));
        assert_eq!(r.rest::<u32>(Scope::Line), [30]);
    }

    #[test]
    #[should_panic(expected = "found only 2")]
    fn take_past_end_of_line_panics() {
        Reader::new("1 2\n3 4\n").take::<i32>(3, Scope::Line);
    }

    #[test]
    #[should_panic(expected = "could not parse \"oops\" as i32")]
    fn bad_token_names_itself() {
        Reader::new("1\noops\n").take::<i32>(2, Scope::All);
    }

    #[test]
    #[should_panic(expected = "line 2")]
    fn panic_points_at_the_line() {
        Reader::new("1\noops\n").take::<i32>(2, Scope::All);
    }

    #[test]
    #[should_panic(expected = "expected nothing more")]
    fn end_catches_extra_output() {
        let mut r = Reader::new("1\ndebug: hi\n");
        let _: i32 = r.read();
        r.end();
    }

    #[test]
    fn end_tolerates_trailing_whitespace() {
        let mut r = Reader::new("1\n\n  \n");
        let _: i32 = r.read();
        r.end();
    }

    #[test]
    fn matches_ignores_trailing_whitespace() {
        assert_matches("stdout", "a\nb  \n", "a\nb");
        assert_matches("stdout", "a\nb", "a\nb\n\n");
    }

    #[test]
    #[should_panic(expected = "did not match at line 2")]
    fn matches_reports_first_bad_line() {
        assert_matches("stdout", "a\nX\nc\n", "a\nb\nc\n");
    }

    #[test]
    #[should_panic(expected = "<no more output>")]
    fn matches_reports_missing_lines() {
        assert_matches("stdout", "a\n", "a\nb\n");
    }
}
