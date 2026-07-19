//! Grade-stage overrides (design §14, §18.2 -- M5 step 24): manual
//! per-student score overrides and a late-penalty policy, both applied
//! *after* `grade::grade` produces a normal `Grade` from a persisted
//! `EvaluationResult` -- never by mutating the raw `EvaluationResult`
//! itself, so raw judge output stays an untouched audit trail (design §14)
//! no matter how many times a course re-grades or an appeal changes a
//! student's score. Both `grade` and `regrade` apply the same
//! `overrides.toml` through `apply`, so re-grading with a changed
//! `[scoring]` policy or an edited override file both take effect the same
//! way: recomputed fresh from the persisted eval, never patched in place.
//!
//! `crate::fetch`'s `Fetchable for GitRepo` resolves *which commit* gets
//! graded from the deadline (design §7.1), but doesn't surface that
//! commit's own push/commit timestamp anywhere `apply` can read -- so
//! `submitted_at` here is still operator-supplied, the same way a manual
//! score override already is: an instructor/TA fills in `overrides.toml`
//! after checking the real submission time. Threading a real timestamp
//! through automatically (from `FetchRecord` or the resolved commit's own
//! metadata) would remove the need for this file entry, but the
//! late-penalty *calculation* itself (grace period, then a percentage per
//! day late, capped) wouldn't change.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, FixedOffset};
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::Grade;
use crate::spec::LatePenalty;

/// The file name looked up under an assignment package dir, alongside
/// `autograder.toml` and `harness/`.
pub const OVERRIDES_FILE: &str = "overrides.toml";

#[derive(Debug, Clone, Deserialize)]
pub struct ManualOverride {
    pub score: f64,
    #[serde(default)]
    pub status: Option<String>,
    /// Required: a manual override always needs a recorded reason (design
    /// §14 -- overrides must be auditable, not silent).
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LateSubmission {
    pub submitted_at: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Overrides {
    #[serde(default)]
    pub manual: BTreeMap<String, ManualOverride>,
    #[serde(default)]
    pub late: BTreeMap<String, LateSubmission>,
}

impl Overrides {
    /// Absent file -> empty overrides (the common case: most runs have
    /// none), rather than an error.
    pub fn load(path: &Path) -> Result<Overrides> {
        if !path.is_file() {
            return Ok(Overrides::default());
        }
        let contents = crate::fs::read_to_string(path)?;
        toml::from_str(&contents).map_err(|source| Error::Toml {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }

    pub fn load_from_package(package_dir: &Path) -> Result<Overrides> {
        Self::load(&package_dir.join(OVERRIDES_FILE))
    }
}

/// Applies whichever override applies to `grade.student_id`, in priority
/// order: a manual override always wins outright (the instructor's final
/// word); otherwise a late-penalty entry is applied if both the student has
/// one in `overrides.toml` *and* the spec defines `[scoring.late-penalty]`
/// (an assignment with no late-penalty policy never docks a score, even if
/// an operator recorded a late timestamp). Neither present -> `grade`
/// unchanged.
pub fn apply(
    grade: Grade,
    overrides: &Overrides,
    deadline: DateTime<FixedOffset>,
    late_policy: Option<&LatePenalty>,
) -> Grade {
    if let Some(manual) = overrides.manual.get(&grade.student_id) {
        return apply_manual_override(grade, manual);
    }
    if let (Some(late), Some(policy)) = (overrides.late.get(&grade.student_id), late_policy) {
        return apply_late_penalty(grade, late.submitted_at, deadline, policy);
    }
    grade
}

fn apply_manual_override(mut grade: Grade, over: &ManualOverride) -> Grade {
    grade.score = over.score;
    grade.status = over
        .status
        .clone()
        .unwrap_or_else(|| "override".to_string());
    grade.override_reason = Some(over.reason.clone());
    grade
}

/// `submitted_at` at or before `deadline + grace` -> unchanged (on time).
/// Otherwise, `per_day_percent` is deducted for every day (rounded up) past
/// the grace period, capped at `max_percent`.
fn apply_late_penalty(
    mut grade: Grade,
    submitted_at: DateTime<FixedOffset>,
    deadline: DateTime<FixedOffset>,
    policy: &LatePenalty,
) -> Grade {
    let grace = chrono::Duration::from_std(policy.grace.0).unwrap_or(chrono::Duration::zero());
    let late_by = submitted_at.signed_duration_since(deadline) - grace;
    if late_by <= chrono::Duration::zero() {
        return grade;
    }

    let days_late = (late_by.num_seconds() as f64 / 86_400.0).ceil().max(1.0);
    let penalty_percent = (policy.per_day_percent * days_late).min(policy.max_percent);
    let factor = (1.0 - penalty_percent / 100.0).max(0.0);

    grade.score = (grade.score * factor).max(0.0);
    grade.late_penalty_percent = Some(penalty_percent);
    grade
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grade(score: f64) -> Grade {
        Grade {
            student_id: "alice".into(),
            score,
            max: 100.0,
            status: "pass".into(),
            failing_tests: Vec::new(),
            override_reason: None,
            late_penalty_percent: None,
        }
    }

    fn deadline() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-02-14T23:59:59-08:00").unwrap()
    }

    fn policy() -> LatePenalty {
        LatePenalty {
            grace: crate::spec::Duration(std::time::Duration::from_secs(0)),
            per_day_percent: 10.0,
            max_percent: 50.0,
        }
    }

    #[test]
    fn load_missing_file_returns_empty_overrides() {
        let overrides = Overrides::load(Path::new("/nonexistent/overrides.toml")).unwrap();
        assert!(overrides.manual.is_empty());
        assert!(overrides.late.is_empty());
    }

    #[test]
    fn parses_manual_and_late_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overrides.toml");
        std::fs::write(
            &path,
            r#"
[manual.alice]
score = 45.0
reason = "Regrade after appeal, see email 2026-07-10"

[late.bob]
submitted_at = "2026-02-16T10:00:00-08:00"
"#,
        )
        .unwrap();

        let overrides = Overrides::load(&path).unwrap();
        assert_eq!(overrides.manual["alice"].score, 45.0);
        assert_eq!(
            overrides.late["bob"].submitted_at,
            DateTime::parse_from_rfc3339("2026-02-16T10:00:00-08:00").unwrap()
        );
    }

    #[test]
    fn manual_override_replaces_score_and_records_the_reason() {
        let over = ManualOverride {
            score: 45.0,
            status: Some("manual-pass".into()),
            reason: "Regrade after appeal".into(),
        };
        let overrides = Overrides {
            manual: BTreeMap::from([("alice".to_string(), over)]),
            late: BTreeMap::new(),
        };

        let result = apply(grade(10.0), &overrides, deadline(), None);

        assert_eq!(result.score, 45.0);
        assert_eq!(result.status, "manual-pass");
        assert_eq!(
            result.override_reason.as_deref(),
            Some("Regrade after appeal")
        );
    }

    #[test]
    fn on_time_submission_is_unaffected_even_with_a_late_entry_recorded() {
        let overrides = Overrides {
            manual: BTreeMap::new(),
            late: BTreeMap::from([(
                "alice".to_string(),
                LateSubmission {
                    submitted_at: deadline() - chrono::Duration::hours(1),
                },
            )]),
        };

        let result = apply(grade(80.0), &overrides, deadline(), Some(&policy()));

        assert_eq!(result.score, 80.0);
        assert!(result.late_penalty_percent.is_none());
    }

    #[test]
    fn late_submission_deducts_per_day_percent_capped_at_max() {
        let overrides = Overrides {
            manual: BTreeMap::new(),
            late: BTreeMap::from([(
                "alice".to_string(),
                LateSubmission {
                    // just over 2 days late -> 3 days rounded up -> 30%,
                    // under the 50% cap.
                    submitted_at: deadline() + chrono::Duration::hours(49),
                },
            )]),
        };

        let result = apply(grade(100.0), &overrides, deadline(), Some(&policy()));

        assert_eq!(result.late_penalty_percent, Some(30.0));
        assert_eq!(result.score, 70.0);
    }

    #[test]
    fn late_penalty_never_exceeds_the_configured_cap() {
        let overrides = Overrides {
            manual: BTreeMap::new(),
            late: BTreeMap::from([(
                "alice".to_string(),
                LateSubmission {
                    submitted_at: deadline() + chrono::Duration::days(30),
                },
            )]),
        };

        let result = apply(grade(100.0), &overrides, deadline(), Some(&policy()));

        assert_eq!(result.late_penalty_percent, Some(50.0));
        assert_eq!(result.score, 50.0);
    }

    #[test]
    fn late_entry_without_a_configured_policy_is_a_no_op() {
        let overrides = Overrides {
            manual: BTreeMap::new(),
            late: BTreeMap::from([(
                "alice".to_string(),
                LateSubmission {
                    submitted_at: deadline() + chrono::Duration::days(5),
                },
            )]),
        };

        let result = apply(grade(100.0), &overrides, deadline(), None);

        assert_eq!(result.score, 100.0);
        assert!(result.late_penalty_percent.is_none());
    }

    #[test]
    fn manual_override_wins_over_a_recorded_late_entry() {
        let overrides = Overrides {
            manual: BTreeMap::from([(
                "alice".to_string(),
                ManualOverride {
                    score: 90.0,
                    status: None,
                    reason: "Extension granted".into(),
                },
            )]),
            late: BTreeMap::from([(
                "alice".to_string(),
                LateSubmission {
                    submitted_at: deadline() + chrono::Duration::days(5),
                },
            )]),
        };

        let result = apply(grade(100.0), &overrides, deadline(), Some(&policy()));

        assert_eq!(result.score, 90.0);
        assert!(result.late_penalty_percent.is_none());
        assert_eq!(result.status, "override");
    }
}
