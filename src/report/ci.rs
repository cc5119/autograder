//! Student-facing CI feedback: per-test pass/fail plus diagnostics, and an
//! overall public pass/fail summary -- **no scores**. Distinct from the
//! `Reporter` trait (which renders `Grade`s): the CI tier never computes a
//! score, only the raw `EvaluationResult` plus any manifest diagnostics
//! found before the evaluator ran.

use std::fmt::Write as _;

use console::style;

use crate::model::{EvalStatus, EvaluationResult, TestOutcome, TestStatus};
use crate::pipeline::manifest_check::ManifestDiagnostic;

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
        match &eval.status {
            EvalStatus::Ran {
                tests: TestOutcome::Tests(tests),
                ..
            } => tests.iter().all(|t| t.status == TestStatus::Pass),
            _ => false,
        }
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

        let tests = match &eval.status {
            EvalStatus::BuildFailed(status) => {
                if let Some(errors) = &eval.diagnostics.compiler_errors {
                    out.push_str(errors);
                    if !errors.ends_with('\n') {
                        out.push('\n');
                    }
                }
                let _ = writeln!(out, "autograde: build failed ({})", status.label());
                return out;
            }
            EvalStatus::Ran { process, tests } => {
                let _ = writeln!(out, "autograde: process {}", process.describe());
                match tests {
                    TestOutcome::Unavailable(reason) => {
                        if let Some(excerpt) = &eval.diagnostics.stderr_excerpt {
                            out.push_str(excerpt);
                            if !excerpt.ends_with('\n') {
                                out.push('\n');
                            }
                        }
                        let _ = writeln!(out, "autograde: tests unavailable ({reason})");
                        return out;
                    }
                    TestOutcome::Tests(tests) => tests,
                }
            }
        };

        let mut failing = 0usize;
        for test in tests {
            match test.status {
                TestStatus::Pass => {
                    let _ = writeln!(out, "{} {}", style("\u{2713}").green(), test.name);
                }
                other => {
                    failing += 1;
                    let _ = writeln!(out, "{} {}", style("\u{2717}").red(), test.name);
                    let detail = test
                        .message
                        .clone()
                        .unwrap_or_else(|| test_status_label(other).to_string());
                    for line in detail.lines() {
                        let _ = writeln!(out, "    \u{2502} {}", style(line).dim());
                    }
                    let _ = writeln!(out);
                }
            }
        }

        let _ = writeln!(
            out,
            "autograde: {failing} of {} public tests failing",
            tests.len()
        );
        out
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
    use crate::exec::sandbox::ProcessStatus;
    use crate::model::BuildStatus;
    use crate::model::Diagnostics;
    use crate::model::TestResult;

    fn eval_with_tests(tests: Vec<TestResult>) -> EvaluationResult {
        EvaluationResult {
            assignment_id: "hw3".into(),
            github_user: "local".into(),
            run_id: "run-1".into(),
            graded_commit: None,
            instructor_commit: None,
            status: EvalStatus::Ran {
                process: ProcessStatus::Exited(0),
                tests: TestOutcome::Tests(tests),
            },
            wall_clock_ms: None,
            diagnostics: Diagnostics::default(),
        }
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
        eval.status = EvalStatus::BuildFailed(BuildStatus::Failed(ProcessStatus::Exited(1)));
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
