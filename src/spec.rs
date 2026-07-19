use std::collections::BTreeMap;
use std::path::Path;

use jiff::Zoned;
use serde::{Deserialize, Deserializer};

use crate::error::{Error, Result};
use crate::id::AssignmentId;

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
    pub id: AssignmentId,
    pub name: String,
    pub kind: AssignmentKind,
    pub deadline: Zoned,
    /// The nextest test-binary target to run (the file name under
    /// `harness/tests/` minus `.rs`, e.g. `"judge"` for
    /// `harness/tests/judge.rs`), passed as `cargo nextest run --test
    /// <target>`.
    #[serde(rename = "judge-target")]
    pub judge_target: String,
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

/// How a submission's score is computed from the sum of `score=` lines
/// tests report at run time (see `crate::grade`). No per-test point
/// declarations here -- a test's contribution is whatever it reports, or
/// the 1.0/0.0 pass/fail default when it reports nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum ScoringFormula {
    /// `score = base + sum(reported values)`, unnormalized.
    Sum { base: f64 },
    /// `score = scale_min + clamp(sum / max_sum, 0, 1) * (scale_max - scale_min)`.
    Affine {
        max_sum: f64,
        scale_min: f64,
        scale_max: f64,
    },
}

#[derive(Debug, Clone)]
pub struct Scoring {
    pub formula: ScoringFormula,
    pub late_penalty: Option<LatePenalty>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawScoring {
    formula: String,
    #[serde(default)]
    base: f64,
    #[serde(default, rename = "max-sum")]
    max_sum: Option<f64>,
    #[serde(default, rename = "scale-min")]
    scale_min: Option<f64>,
    #[serde(default, rename = "scale-max")]
    scale_max: Option<f64>,
    #[serde(default, rename = "late-penalty")]
    late_penalty: Option<LatePenalty>,
}

impl<'de> Deserialize<'de> for Scoring {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawScoring::deserialize(deserializer)?;
        let formula = match raw.formula.as_str() {
            "sum" => ScoringFormula::Sum { base: raw.base },
            "affine" => ScoringFormula::Affine {
                max_sum: raw
                    .max_sum
                    .ok_or_else(|| serde::de::Error::missing_field("max-sum"))?,
                scale_min: raw
                    .scale_min
                    .ok_or_else(|| serde::de::Error::missing_field("scale-min"))?,
                scale_max: raw
                    .scale_max
                    .ok_or_else(|| serde::de::Error::missing_field("scale-max"))?,
            },
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown [scoring].formula {other:?}, expected \"sum\" or \"affine\""
                )));
            }
        };
        Ok(Scoring {
            formula,
            late_penalty: raw.late_penalty,
        })
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
judge-target = "judge"


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
formula = "sum"
base = 1.0
"#;

    const AFFINE_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
judge-target = "judge"


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
formula = "affine"
max-sum = 20.0
scale-min = 1.0
scale-max = 7.0
"#;

    #[test]
    fn parses_a_sum_formula_spec() {
        let spec: Spec = toml::from_str(PUBLIC_TOML).unwrap();
        assert_eq!(spec.assignment.id, "hw3");
        assert_eq!(spec.assignment.kind, AssignmentKind::Library);
        assert_eq!(spec.limits.build.memory, ByteSize(2 * 1024 * 1024 * 1024));
        assert_eq!(
            spec.limits.build.wall_clock,
            Duration(std::time::Duration::from_secs(120))
        );
        assert_eq!(spec.limits.run.max_output_bytes, ByteSize(1024 * 1024));
        assert_eq!(spec.scoring.formula, ScoringFormula::Sum { base: 1.0 });
    }

    #[test]
    fn parses_an_affine_formula_spec() {
        let spec: Spec = toml::from_str(AFFINE_TOML).unwrap();
        assert_eq!(
            spec.scoring.formula,
            ScoringFormula::Affine {
                max_sum: 20.0,
                scale_min: 1.0,
                scale_max: 7.0,
            }
        );
    }

    #[test]
    fn affine_formula_missing_a_required_field_is_a_clear_parse_error() {
        let toml = AFFINE_TOML.replace("max-sum = 20.0\n", "");
        let result: std::result::Result<Spec, _> = toml::from_str(&toml);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_formula_is_a_clear_parse_error() {
        let toml = PUBLIC_TOML.replace(
            "formula = \"sum\"\nbase = 1.0",
            "formula = \"geometric-mean\"",
        );
        let result: std::result::Result<Spec, _> = toml::from_str(&toml);
        assert!(result.is_err());
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
    fn missing_judge_target_is_a_clear_parse_error() {
        let toml = PUBLIC_TOML.replace("judge-target = \"judge\"\n", "");
        let result: std::result::Result<Spec, _> = toml::from_str(&toml);
        assert!(result.is_err());
    }

    #[test]
    fn late_penalty_table_parses_grace_hours_and_percentages() {
        let toml = PUBLIC_TOML.replace(
            "[scoring]\nformula = \"sum\"\nbase = 1.0",
            "[scoring]\nformula = \"sum\"\nbase = 1.0\n\n[scoring.late-penalty]\ngrace = \"24h\"\nper-day-percent = 10\nmax-percent = 50",
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
