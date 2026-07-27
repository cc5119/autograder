//! Human-readable summary of an `autograder evaluate` batch: one line per
//! student plus an aggregate tally, printed straight to stdout regardless
//! of the tracing/log level -- this is the only feedback `evaluate` gives
//! by default, so it must never depend on `RUST_LOG`.
//!
//! Every element of `evals` already reflects a real `EvaluationResult` --
//! `pipeline::evaluate_batch` never skips a student, even one whose fetch
//! or build failed (see its own doc comment) -- so an empty `evals` here
//! means no student was ever recorded (e.g. `submissions_dir/.meta/` is
//! missing or empty), not that every one of them failed silently.

use std::fmt::Write as _;

use crate::model::{EvaluationResult, StageStatus, TestStatus};

pub fn render_evaluate_summary(evals: &[EvaluationResult]) -> String {
    let mut out = String::new();

    if evals.is_empty() {
        let _ = writeln!(
            out,
            "autograde: 0 students evaluated -- no fetch records found under \
             <submissions>/.meta/ (run `autograder fetch` first, or check --submissions)"
        );
        return out;
    }

    let mut ok = 0usize;
    for eval in evals {
        let stage_status = worst_stage_status(eval);
        if stage_status == StageStatus::Ok {
            ok += 1;
            let passed = eval
                .tests
                .iter()
                .filter(|t| t.status == TestStatus::Pass)
                .count();
            let _ = writeln!(
                out,
                "{}: ok ({passed}/{} tests passed)",
                eval.student_id,
                eval.tests.len()
            );
        } else {
            let _ = writeln!(out, "{}: {}", eval.student_id, stage_status.label());
        }
    }

    let _ = writeln!(
        out,
        "\n{} students evaluated: {ok} ok, {} not ok",
        evals.len(),
        evals.len() - ok
    );
    out
}

/// The first non-`Ok` stage in fetch -> build -> run order, or `Ok` if all
/// three succeeded -- mirrors the order the pipeline itself short-circuits
/// in (`pipeline::evaluate_submission`).
fn worst_stage_status(eval: &EvaluationResult) -> StageStatus {
    [
        eval.stages.fetch.status,
        eval.stages.build.status,
        eval.stages.run.status,
    ]
    .into_iter()
    .find(|status| *status != StageStatus::Ok)
    .unwrap_or(StageStatus::Ok)
}
