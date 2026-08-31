use std::path::Path;

use jiff::Zoned;
use serde::{Deserialize, Deserializer};

use crate::error::{Error, Result};
use crate::id::AssignmentId;

pub const SPEC_FILE: &str = "autograder.toml";

#[derive(Debug, Clone, Deserialize)]
pub struct Assignment {
    /// The assignment `id` is used as the name for the package students must complete/submit
    pub id: AssignmentId,
    pub deadline: Zoned,
    /// The name of the `harness` package
    pub harness: String,
    /// Further workspace packages the instructor owns, beyond `{harness}`:
    /// support libraries `{id}` and/or the harness link against (a fake
    /// server, a driver protocol). They sit at the same trust level as the
    /// harness -- always overlaid from the instructor tree, so a student's
    /// edits to them are discarded -- and are the only path dependencies
    /// `{id}`'s manifest may declare (see `crate::pipeline::manifest_check`).
    #[serde(rename = "extra-packages", default)]
    pub extra_packages: Vec<String>,
    /// SHA-256 of the `Cargo.lock` blessed for this assignment
    #[serde(rename = "cargo-lock-sha256")]
    pub cargo_lock_sha256: String,
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
    #[serde(rename = "max-output-bytes")]
    pub max_output_bytes: ByteSize,
}

/// How a submission's score is computed from the sum of `score=` lines
/// tests report at run time (see `crate::pipeline::grade`).
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
        Ok(Scoring { formula })
    }
}

/// How a late submission's score is docked, applied to the final `score`
/// (after `spec.scoring`'s formula) rather than to raw points, so it
/// composes with either `sum` or `affine` scoring without knowing the
/// point scale.
#[derive(Debug, Clone, PartialEq)]
pub enum LatePenaltyFormula {
    /// `fraction_off = clamp(rate * days_late_after_grace, 0, max_penalty)`.
    Linear {
        rate: f64,
        grace: jiff::SignedDuration,
        max_penalty: f64,
    },
}

#[derive(Debug, Clone)]
pub struct LatePenalty {
    pub formula: LatePenaltyFormula,
}

impl LatePenalty {
    /// The fraction of `score` to deduct for a submission that was
    /// `late_by` past the deadline. `late_by` is expected non-negative --
    /// callers only reach here when there's lateness to penalize at all.
    pub fn fraction_off(&self, late_by: jiff::SignedDuration) -> f64 {
        match self.formula {
            LatePenaltyFormula::Linear {
                rate,
                grace,
                max_penalty,
            } => {
                let over_grace = (late_by.as_secs_f64() - grace.as_secs_f64()).max(0.0);
                let days_late = over_grace / 86_400.0;
                (rate * days_late).clamp(0.0, max_penalty)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawLatePenalty {
    formula: String,
    rate: f64,
    #[serde(default)]
    grace: Option<Duration>,
    #[serde(default, rename = "max-penalty")]
    max_penalty: Option<f64>,
}

impl<'de> Deserialize<'de> for LatePenalty {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawLatePenalty::deserialize(deserializer)?;
        let formula = match raw.formula.as_str() {
            "linear" => LatePenaltyFormula::Linear {
                rate: raw.rate,
                grace: raw
                    .grace
                    .map(|d| jiff::SignedDuration::try_from(d.0).map_err(serde::de::Error::custom))
                    .transpose()?
                    .unwrap_or(jiff::SignedDuration::ZERO),
                max_penalty: raw.max_penalty.unwrap_or(1.0),
            },
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown [late-penalty].formula {other:?}, expected \"linear\""
                )));
            }
        };
        Ok(LatePenalty { formula })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    pub assignment: Assignment,
    pub sandbox: Sandbox,
    #[serde(rename = "build-limits")]
    pub build_limits: BuildLimits,
    pub scoring: Scoring,
    /// Absent means no penalty for lateness -- the deadline still gates
    /// which commit `fetch` picks, but a late score isn't docked for it.
    #[serde(rename = "late-penalty", default)]
    pub late_penalty: Option<LatePenalty>,
}

impl Spec {
    pub fn load(dir: &Path) -> Result<Spec> {
        Self::load_file(&dir.join(SPEC_FILE))
    }

    pub fn load_file(path: &Path) -> Result<Spec> {
        let contents = crate::exec::fs::read_to_string(path)?;
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
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "0000000000000000000000000000000000000000000000000000000000000000"


[sandbox]
image = "autograder-base:1.86.0"

[build-limits]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256
max-output-bytes = "1MiB"

[scoring]
formula = "sum"
base = 1.0
"#;

    const AFFINE_TOML: &str = r#"
[assignment]
id = "hw3"
name = "Binary search tree"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "0000000000000000000000000000000000000000000000000000000000000000"


[sandbox]
image = "autograder-base:1.86.0"

[build-limits]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256
max-output-bytes = "1MiB"

[scoring]
formula = "affine"
max-sum = 20.0
scale-min = 1.0
scale-max = 7.0
"#;

    /// The two-package workspace is the common case, so an assignment that
    /// says nothing about support packages must keep parsing unchanged.
    #[test]
    fn extra_packages_defaults_to_empty() {
        let spec: Spec = toml::from_str(PUBLIC_TOML).unwrap();
        assert!(spec.assignment.extra_packages.is_empty());
    }

    #[test]
    fn parses_extra_packages_in_order() {
        let toml = PUBLIC_TOML.replace(
            "harness = \"harness\"",
            "harness = \"harness\"\nextra-packages = [\"messaging\", \"protocol\"]",
        );
        let spec: Spec = toml::from_str(&toml).unwrap();
        assert_eq!(spec.assignment.extra_packages, ["messaging", "protocol"]);
    }

    #[test]
    fn parses_a_sum_formula_spec() {
        let spec: Spec = toml::from_str(PUBLIC_TOML).unwrap();
        assert_eq!(spec.assignment.id, "hw3");
        assert_eq!(spec.build_limits.memory, ByteSize(2 * 1024 * 1024 * 1024));
        assert_eq!(
            spec.build_limits.wall_clock,
            Duration(std::time::Duration::from_secs(120))
        );
        assert_eq!(spec.build_limits.max_output_bytes, ByteSize(1024 * 1024));
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
    fn late_penalty_is_absent_by_default() {
        let spec: Spec = toml::from_str(PUBLIC_TOML).unwrap();
        assert!(spec.late_penalty.is_none());
    }

    #[test]
    fn parses_a_linear_late_penalty_with_defaulted_grace_and_cap() {
        let toml = format!("{PUBLIC_TOML}\n[late-penalty]\nformula = \"linear\"\nrate = 0.1\n");
        let spec: Spec = toml::from_str(&toml).unwrap();
        assert_eq!(
            spec.late_penalty.unwrap().formula,
            LatePenaltyFormula::Linear {
                rate: 0.1,
                grace: jiff::SignedDuration::ZERO,
                max_penalty: 1.0,
            }
        );
    }

    #[test]
    fn parses_a_linear_late_penalty_with_an_explicit_grace_and_cap() {
        let toml = format!(
            "{PUBLIC_TOML}\n[late-penalty]\nformula = \"linear\"\nrate = 0.1\ngrace = \"1h\"\nmax-penalty = 0.5\n"
        );
        let spec: Spec = toml::from_str(&toml).unwrap();
        assert_eq!(
            spec.late_penalty.unwrap().formula,
            LatePenaltyFormula::Linear {
                rate: 0.1,
                grace: jiff::SignedDuration::from_hours(1),
                max_penalty: 0.5,
            }
        );
    }

    #[test]
    fn unknown_late_penalty_formula_is_a_clear_parse_error() {
        let toml =
            format!("{PUBLIC_TOML}\n[late-penalty]\nformula = \"exponential\"\nrate = 0.1\n");
        let result: std::result::Result<Spec, _> = toml::from_str(&toml);
        assert!(result.is_err());
    }

    #[test]
    fn linear_penalty_is_zero_within_the_grace_window() {
        let penalty = LatePenalty {
            formula: LatePenaltyFormula::Linear {
                rate: 0.1,
                grace: jiff::SignedDuration::from_hours(1),
                max_penalty: 1.0,
            },
        };
        assert_eq!(
            penalty.fraction_off(jiff::SignedDuration::from_mins(30)),
            0.0
        );
    }

    #[test]
    fn linear_penalty_accrues_per_day_late_after_grace_and_clamps_at_the_cap() {
        let penalty = LatePenalty {
            formula: LatePenaltyFormula::Linear {
                rate: 0.1,
                grace: jiff::SignedDuration::ZERO,
                max_penalty: 0.5,
            },
        };
        assert_eq!(
            penalty.fraction_off(jiff::SignedDuration::from_hours(24)),
            0.1
        );
        assert_eq!(
            penalty.fraction_off(jiff::SignedDuration::from_hours(24 * 10)),
            0.5
        );
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
