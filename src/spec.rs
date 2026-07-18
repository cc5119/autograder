use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Deserializer};

use crate::error::{Error, Result};
use crate::model::TestVisibility;

/// The private full spec file; carries points.
pub const PRIVATE_SPEC_FILE: &str = "autograder.toml";
/// The public subset shipped to students; carries no points.
pub const PUBLIC_SPEC_FILE: &str = "autograder.public.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssignmentKind {
    LinkedLibrary,
    BinaryHarness,
}

/// `id` doubles as the student-facing crate name: for `linked-library` it's
/// the dependency name the harness links against, for `binary-harness` it's
/// the expected binary target name. One identifier, no separate
/// `[student]` config section to keep in sync with it.
#[derive(Debug, Clone, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub name: String,
    pub kind: AssignmentKind,
    pub deadline: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Toolchain {
    pub channel: String,
}

/// A size in bytes, parsed from strings like "2GiB", "1MiB", "512B".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl ByteSize {
    fn parse(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim();
        let (digits, unit) = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .map(|i| s.split_at(i))
            .ok_or_else(|| format!("size {s:?} has no unit"))?;
        let value: f64 = digits
            .parse()
            .map_err(|_| format!("invalid size number in {s:?}"))?;
        let multiplier: u64 = match unit.trim() {
            "B" => 1,
            "KiB" => 1024,
            "MiB" => 1024 * 1024,
            "GiB" => 1024 * 1024 * 1024,
            other => return Err(format!("unknown size unit {other:?} in {s:?}")),
        };
        Ok(ByteSize((value * multiplier as f64) as u64))
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ByteSize::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A duration parsed from strings like "120s", "5s".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Duration(pub std::time::Duration);

impl Duration {
    fn parse(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim();
        let digits = s.trim_end_matches(|c: char| c.is_alphabetic());
        let unit = &s[digits.len()..];
        let value: u64 = digits
            .parse()
            .map_err(|_| format!("invalid duration number in {s:?}"))?;
        let secs = match unit {
            "s" => value,
            "m" => value * 60,
            other => return Err(format!("unknown duration unit {other:?} in {s:?}")),
        };
        Ok(Duration(std::time::Duration::from_secs(secs)))
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Duration::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildLimits {
    #[serde(rename = "wall-clock")]
    pub wall_clock: Duration,
    pub cpus: u32,
    pub memory: ByteSize,
    pub pids: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunLimits {
    #[serde(rename = "cpu-time")]
    pub cpu_time: Duration,
    #[serde(rename = "wall-clock")]
    pub wall_clock: Duration,
    pub cpus: u32,
    pub memory: ByteSize,
    pub pids: u32,
    #[serde(rename = "max-output-bytes")]
    pub max_output_bytes: ByteSize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    pub build: BuildLimits,
    pub run: RunLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScoringModel {
    Weighted,
    PassCount,
    PassFail,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScoredTest {
    pub name: String,
    pub visibility: TestVisibility,
    /// Absent in the public spec: the shipped spec never reveals weighting,
    /// and the autograder computes no score for a pointsless test.
    #[serde(default)]
    pub points: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scoring {
    pub model: ScoringModel,
    #[serde(default, rename = "tests")]
    pub tests: Vec<ScoredTest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    pub assignment: Assignment,
    pub toolchain: Toolchain,
    #[serde(default, rename = "allowed-crates")]
    pub allowed_crates: BTreeMap<String, String>,
    pub limits: Limits,
    pub scoring: Scoring,
}

impl Spec {
    /// Reads `autograder.toml` (private/full) if present, else
    /// `autograder.public.toml` (public subset) from `dir`.
    pub fn load(dir: &Path) -> Result<Spec> {
        let private = dir.join(PRIVATE_SPEC_FILE);
        let public = dir.join(PUBLIC_SPEC_FILE);
        let path = if private.exists() { private } else { public };
        Self::load_file(&path)
    }

    pub fn load_file(path: &Path) -> Result<Spec> {
        let contents = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&contents).map_err(|source| Error::Toml {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }

    /// True if no scored test in this spec carries points (i.e. this is a
    /// public spec, whether named `autograder.public.toml` or not).
    pub fn exposes_no_points(&self) -> bool {
        self.scoring.tests.iter().all(|t| t.points.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "linked-library"
deadline = "2026-02-14T23:59:59-08:00"


[toolchain]
channel = "1.86.0"

[allowed-crates]
serde = "1"
rand  = "0.8"

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
visibility = "public"
"#;

    const PRIVATE_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "linked-library"
deadline = "2026-02-14T23:59:59-08:00"


[toolchain]
channel = "1.86.0"

[allowed-crates]
serde = "1"
rand  = "0.8"

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
    fn parses_public_spec_with_no_points() {
        let spec: Spec = toml::from_str(PUBLIC_TOML).unwrap();
        assert_eq!(spec.assignment.id, "hw3");
        assert_eq!(spec.assignment.kind, AssignmentKind::LinkedLibrary);
        assert_eq!(spec.limits.build.memory, ByteSize(2 * 1024 * 1024 * 1024));
        assert_eq!(
            spec.limits.build.wall_clock,
            Duration(std::time::Duration::from_secs(120))
        );
        assert_eq!(spec.limits.run.max_output_bytes, ByteSize(1024 * 1024));
        assert!(spec.exposes_no_points());
    }

    #[test]
    fn parses_private_spec_with_points() {
        let spec: Spec = toml::from_str(PRIVATE_TOML).unwrap();
        assert_eq!(spec.scoring.tests.len(), 2);
        assert_eq!(spec.scoring.tests[0].points, Some(10.0));
        assert_eq!(spec.scoring.tests[1].visibility, TestVisibility::Private);
        assert!(!spec.exposes_no_points());
    }
}
