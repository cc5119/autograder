use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub struct Id<Tag> {
    value: &'static str,
    _tag: PhantomData<fn() -> Tag>,
}

impl<Tag> Id<Tag> {
    /// Leaks `value`'s storage. Call once, where a fresh id string is
    /// first produced -- never in a hot loop.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: Box::leak(value.into().into_boxed_str()),
            _tag: PhantomData,
        }
    }

    pub fn as_str(&self) -> &'static str {
        self.value
    }
}

impl<Tag> Copy for Id<Tag> {}

impl<Tag> Clone for Id<Tag> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Tag> PartialEq for Id<Tag> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<Tag> Eq for Id<Tag> {}

impl<Tag> PartialEq<&str> for Id<Tag> {
    fn eq(&self, other: &&str) -> bool {
        self.value == *other
    }
}

impl<Tag> PartialOrd for Id<Tag> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<Tag> Ord for Id<Tag> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(other.value)
    }
}

impl<Tag> std::hash::Hash for Id<Tag> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<Tag> fmt::Debug for Id<Tag> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.value, f)
    }
}

impl<Tag> fmt::Display for Id<Tag> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.value, f)
    }
}

impl<Tag> From<&str> for Id<Tag> {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl<Tag> From<String> for Id<Tag> {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl<Tag> FromStr for Id<Tag> {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl<Tag> Serialize for Id<Tag> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.value)
    }
}

impl<'de, Tag> Deserialize<'de> for Id<Tag> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::new(String::deserialize(deserializer)?))
    }
}

pub enum StudentTag {}
pub enum AssignmentTag {}
pub enum RunTag {}

/// A student's roster id, and also what identifies one submission
/// directory under a submissions dir: `fetch` names each checkout after
/// the student it fetched, and evaluate/grade reuse that exact directory
/// name (see `pipeline::evaluate_batch`'s module doc comment). There's no
/// separate submission identity -- a submission's id *is* the student id
/// it was checked out under.
pub type StudentId = Id<StudentTag>;
pub type AssignmentId = Id<AssignmentTag>;
pub type RunId = Id<RunTag>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_value_is_equal_and_copy() {
        let a = StudentId::new("alice");
        let b = StudentId::new("alice");
        let c = a; // Copy, not a move
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.as_str(), "alice");
        assert_eq!(a.to_string(), "alice");
    }

    #[test]
    fn roundtrips_through_json() {
        let id = StudentId::new("alice");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"alice\"");
        let parsed: StudentId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }
}
