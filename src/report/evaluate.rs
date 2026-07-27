//! Human-readable summary of an `autograder evaluate` batch: one line per
//! submission plus an aggregate tally, printed straight to stdout
//! regardless of the tracing/log level -- this is the only feedback
//! `evaluate` gives by default, so it must never depend on `RUST_LOG`.
//!
//! Every element of `evals` already reflects a real `EvaluationResult` --
//! `pipeline::evaluate_batch` never skips a submission (see its own doc
//! comment) -- so an empty `evals` here means no submission directory was
//! ever found under `submissions_dir` (e.g. an empty or wrong
//! `--submissions` path), not that every one of them failed silently.

use std::fmt::Write as _;

use crate::model::{EvaluationResult, RunStatus, EvalStatus, TestStatus};

pub fn render_evaluate_summary(evals: &[EvaluationResult]) -> String {
    let mut out = String::new();

    if evals.is_empty() {
        let _ = writeln!(
            out,
            "autograde: 0 submissions evaluated -- no submission directories found under \
             --submissions (run `autograder fetch` first, or check the path)"
        );
        return out;
    }

    let mut ok = 0usize;
    for eval in evals {
        match &eval.status {
            EvalStatus::Ran(RunStatus::Ok) => {
                ok += 1;
                let passed = eval
                    .tests
                    .iter()
                    .filter(|t| t.status == TestStatus::Pass)
                    .count();
                let _ = writeln!(
                    out,
                    "{}: ok ({passed}/{} tests passed)",
                    eval.submission_id,
                    eval.tests.len()
                );
            }
            EvalStatus::BuildFailed(status) => {
                let _ = writeln!(out, "{}: {}", eval.submission_id, status.label());
            }
            EvalStatus::Ran(status) => {
                let _ = writeln!(out, "{}: {}", eval.submission_id, status.label());
            }
        }
    }

    let _ = writeln!(
        out,
        "\n{} submissions evaluated: {ok} ok, {} not ok",
        evals.len(),
        evals.len() - ok
    );
    out
}
