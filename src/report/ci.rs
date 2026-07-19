//! Student-facing CI feedback: per-test pass/fail plus diagnostics, and an
//! overall public pass/fail summary -- **no scores**. Distinct from the
//! `Reporter` trait (which renders `Grade`s): the CI tier never computes a
//! score, only the raw `EvaluationResult` plus any manifest diagnostics
//! found before the evaluator ran.

use std::fmt::Write as _;

use crate::manifest_check::ManifestDiagnostic;
use crate::model::{EvaluationResult, StageStatus, TestStatus};

/// Renders and judges a CI run. `eval` is `None` when a disallowed
/// dependency short-circuited Evaluate entirely (mirrors the authoritative
/// pipeline's short-circuit in `pipeline::grade_batch`).
pub struct CiReport<'a> {
    pub eval: Option<&'a EvaluationResult>,
    pub manifest_diagnostics: &'a [ManifestDiagnostic],
}

impl<'a> CiReport<'a> {
    /// True iff there is nothing for the student to fix: no manifest
    /// diagnostics, the build and run stages succeeded, and every test
    /// passed.
    pub fn passed(&self) -> bool {
        if !self.manifest_diagnostics.is_empty() {
            return false;
        }
        let Some(eval) = self.eval else {
            return false;
        };
        eval.stages.build.status == StageStatus::Ok
            && eval.stages.run.status == StageStatus::Ok
            && eval.tests.iter().all(|t| t.status == TestStatus::Pass)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();

        for diagnostic in self.manifest_diagnostics {
            let _ = writeln!(out, "\u{26a0} {diagnostic}");
        }
        if !self.manifest_diagnostics.is_empty() {
            let _ = writeln!(
                out,
                "autograde: build blocked by {} disallowed-dependency issue(s)",
                self.manifest_diagnostics.len()
            );
            return out;
        }

        let Some(eval) = self.eval else {
            let _ = writeln!(out, "autograde: no evaluation result");
            return out;
        };

        if eval.stages.build.status != StageStatus::Ok {
            if let Some(errors) = &eval.diagnostics.compiler_errors {
                out.push_str(errors);
                if !errors.ends_with('\n') {
                    out.push('\n');
                }
            }
            let _ = writeln!(
                out,
                "autograde: build failed ({})",
                stage_status_label(eval.stages.build.status)
            );
            return out;
        }

        if eval.stages.run.status != StageStatus::Ok {
            if let Some(excerpt) = &eval.diagnostics.stderr_excerpt {
                out.push_str(excerpt);
                if !excerpt.ends_with('\n') {
                    out.push('\n');
                }
            }
            let _ = writeln!(
                out,
                "autograde: run failed ({})",
                stage_status_label(eval.stages.run.status)
            );
            return out;
        }

        let mut failing = 0usize;
        for test in &eval.tests {
            match test.status {
                TestStatus::Pass => {
                    let _ = writeln!(out, "\u{2713} {}", test.name);
                }
                other => {
                    failing += 1;
                    let detail = test
                        .message
                        .clone()
                        .unwrap_or_else(|| test_status_label(other).to_string());
                    let _ = writeln!(out, "\u{2717} {}        {detail}", test.name);
                }
            }
        }

        let _ = writeln!(
            out,
            "autograde: {failing} of {} public tests failing",
            eval.tests.len()
        );
        out
    }
}

fn stage_status_label(status: StageStatus) -> &'static str {
    match status {
        StageStatus::Ok => "ok",
        StageStatus::BuildFailed => "build failed",
        StageStatus::Timeout => "timeout",
        StageStatus::Oom => "out of memory",
        StageStatus::DisallowedDependency => "disallowed dependency",
        StageStatus::FetchFailed => "fetch failed",
        StageStatus::HarnessError => "harness error",
    }
}

fn test_status_label(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Pass => "pass",
        TestStatus::Fail => "failed",
        TestStatus::Timeout => "timeout",
        TestStatus::Oom => "out of memory",
        TestStatus::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Diagnostics;
    use crate::model::{ResourceUsage, StageReport, StageReports, TestResult};

    fn eval_with_tests(tests: Vec<TestResult>) -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            assignment_id: "hw3".into(),
            student_id: "local".into(),
            run_id: "run-1".into(),
            graded_commit: None,
            instructor_commit: None,
            public_harness_commit: None,
            stages: StageReports {
                fetch: StageReport::ok(),
                build: StageReport::ok(),
                run: StageReport::ok(),
            },
            tests,
            resource_usage: ResourceUsage::default(),
            diagnostics: Diagnostics::default(),
        }
    }

    #[test]
    fn all_passing_tests_render_checkmarks_and_pass() {
        let eval = eval_with_tests(vec![TestResult {
            name: "balance_small".into(),
            status: TestStatus::Pass,
            duration_ms: Some(1),
            message: None,
            reported_score: None,
        }]);
        let report = CiReport {
            eval: Some(&eval),
            manifest_diagnostics: &[],
        };

        assert!(report.passed());
        assert!(report.render().contains("\u{2713} balance_small"));
    }

    #[test]
    fn a_failing_test_is_reported_with_its_message_and_fails_overall() {
        let eval = eval_with_tests(vec![TestResult {
            name: "insert_basic".into(),
            status: TestStatus::Fail,
            duration_ms: Some(1),
            message: Some("assertion failed: expected Some(3), got None".into()),
            reported_score: None,
        }]);
        let report = CiReport {
            eval: Some(&eval),
            manifest_diagnostics: &[],
        };

        assert!(!report.passed());
        let rendered = report.render();
        assert!(rendered.contains("\u{2717} insert_basic"));
        assert!(rendered.contains("expected Some(3)"));
        assert!(rendered.contains("autograde: 1 of 1 public tests failing"));
    }

    #[test]
    fn manifest_diagnostics_short_circuit_before_any_eval() {
        let diagnostics = vec![ManifestDiagnostic::DisallowedCrate {
            name: "tokio".into(),
        }];
        let report = CiReport {
            eval: None,
            manifest_diagnostics: &diagnostics,
        };

        assert!(!report.passed());
        let rendered = report.render();
        assert!(rendered.contains("\u{26a0} disallowed crate: tokio"));
        assert!(rendered.contains("autograde: build blocked"));
    }

    #[test]
    fn build_failure_reports_compiler_errors_without_test_list() {
        let mut eval = eval_with_tests(vec![]);
        eval.stages.build = StageReport {
            status: StageStatus::BuildFailed,
            duration_ms: None,
            warnings: None,
        };
        eval.diagnostics.compiler_errors = Some("error[E0433]: failed to resolve".into());
        let report = CiReport {
            eval: Some(&eval),
            manifest_diagnostics: &[],
        };

        assert!(!report.passed());
        let rendered = report.render();
        assert!(rendered.contains("E0433"));
        assert!(rendered.contains("autograde: build failed"));
    }
}
