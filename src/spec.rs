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
    Library,
    Binary,
}

/// `id` doubles as the student-facing crate name: the dependency name for
/// `library`, the expected binary target name for `binary`.
#[derive(Debug, Clone, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub name: String,
    pub kind: AssignmentKind,
    pub deadline: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sandbox {
    /// The container image `ContainerSandbox` requires to already exist
    /// locally -- `autograder` never pulls images itself.
    pub image: String,
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
    fn zero() -> Self {
        Duration(std::time::Duration::ZERO)
    }

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
            "h" => value * 60 * 60,
            "d" => value * 60 * 60 * 24,
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

/// A late-submission penalty: a grace period, then a percentage of the
/// score deducted per day late, capped at `max-percent`. Absent, no late
/// penalty is ever applied, even with an `overrides.toml` late entry.
#[derive(Debug, Clone, Deserialize)]
pub struct LatePenalty {
    #[serde(default = "Duration::zero")]
    pub grace: Duration,
    #[serde(rename = "per-day-percent")]
    pub per_day_percent: f64,
    #[serde(rename = "max-percent")]
    pub max_percent: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scoring {
    pub model: ScoringModel,
    #[serde(default, rename = "tests")]
    pub tests: Vec<ScoredTest>,
    #[serde(default, rename = "late-penalty")]
    pub late_penalty: Option<LatePenalty>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    pub assignment: Assignment,
    pub sandbox: Sandbox,
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
        let contents = crate::fs::read_to_string(path)?;
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
kind = "library"
deadline = "2026-02-14T23:59:59-08:00"


[sandbox]
image = "autograder-base:1.86.0"

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
kind = "library"
deadline = "2026-02-14T23:59:59-08:00"


[sandbox]
image = "autograder-base:1.86.0"

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
        assert_eq!(spec.assignment.kind, AssignmentKind::Library);
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

    #[test]
    fn scoring_with_no_late_penalty_table_parses_to_none() {
        let spec: Spec = toml::from_str(PUBLIC_TOML).unwrap();
        assert!(spec.scoring.late_penalty.is_none());
    }

    #[test]
    fn parses_the_image_and_accepts_a_registry_reference() {
        let spec: Spec = toml::from_str(PUBLIC_TOML).unwrap();
        assert_eq!(spec.sandbox.image, "autograder-base:1.86.0");

        let toml = PUBLIC_TOML.replace(
            r#"image = "autograder-base:1.86.0""#,
            r#"image = "ghcr.io/org/autograder-base:1.86.0""#,
        );
        let spec: Spec = toml::from_str(&toml).unwrap();
        assert_eq!(spec.sandbox.image, "ghcr.io/org/autograder-base:1.86.0");
    }

    #[test]
    fn missing_sandbox_table_is_a_clear_parse_error() {
        let toml = PUBLIC_TOML.replace("[sandbox]\nimage = \"autograder-base:1.86.0\"\n", "");
        let result: std::result::Result<Spec, _> = toml::from_str(&toml);
        assert!(result.is_err());
    }

    #[test]
    fn late_penalty_table_parses_grace_hours_and_percentages() {
        let toml = PUBLIC_TOML.replace(
            "[scoring]\nmodel = \"weighted\"\n[[scoring.tests]]",
            "[scoring]\nmodel = \"weighted\"\n\n[scoring.late-penalty]\ngrace = \"24h\"\nper-day-percent = 10\nmax-percent = 50\n\n[[scoring.tests]]",
        );
        let spec: Spec = toml::from_str(&toml).unwrap();
        let penalty = spec.scoring.late_penalty.unwrap();
        assert_eq!(
            penalty.grace,
            Duration(std::time::Duration::from_secs(24 * 3600))
        );
        assert_eq!(penalty.per_day_percent, 10.0);
        assert_eq!(penalty.max_percent, 50.0);
    }

    #[test]
    fn duration_parses_hour_and_day_units() {
        assert_eq!(
            Duration::parse("2d").unwrap(),
            Duration(std::time::Duration::from_secs(2 * 24 * 3600))
        );
        assert_eq!(
            Duration::parse("3h").unwrap(),
            Duration(std::time::Duration::from_secs(3 * 3600))
        );
    }
}
