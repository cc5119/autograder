pub mod evaluator;
pub mod grade;
pub mod hash;
pub mod manifest_check;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use indicatif::{ProgressBar, ProgressStyle};

use crate::deps;
use crate::deps::cargo_lock::CargoLock;
use crate::error::Result;
use crate::exec::fs;
use crate::exec::json::{read_json, write_json};
use crate::exec::overlay::{self, Context, Rule};
use crate::id::{GithubUser, RunId};
use crate::model::{BuildStatus, Diagnostics, EvalStatus, EvaluationResult, JobContext};
use crate::pipeline::evaluator::Evaluator;
use crate::pipeline::manifest_check::ManifestDiagnostic;
use crate::spec::Spec;
use crate::str_map;

static RUN_COUNTER: AtomicU32 = AtomicU32::new(0);

/// What one `evaluate_batch` run produced. `evals` covers every submission
/// found, including the skipped ones -- their existing result is still the
/// current one, so the tally stays a picture of the whole class rather than
/// only of what happened to re-run.
#[derive(Debug)]
pub struct BatchOutcome {
    pub evals: Vec<EvaluationResult>,
    pub skipped: usize,
}

/// Where `evaluate_batch` persists each `EvaluationResult`, alongside (not
/// inside) the submission checkouts under `submissions_dir`.
const EVAL_DIR: &str = ".eval";

/// One entry per submission under `submissions_dir`: every direct
/// subdirectory whose name doesn't start with `.` -- the fetch/eval/grade
/// stages each keep their own dot-prefixed directory alongside the actual
/// submission checkouts, and skipping by that one rule means evaluate never
/// needs to know their names. Evaluate reads nothing else about how a
/// checkout got there -- no fetch record required or consulted -- so a
/// directory dropped in by hand works exactly like one `autograder fetch`
/// produced. Sorted by name.
fn list_submissions(submissions_dir: &Path) -> Result<Vec<GithubUser>> {
    if !submissions_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir_entries(submissions_dir)? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        ids.push(GithubUser::new(name));
    }
    ids.sort();
    Ok(ids)
}

/// Everything copied from the student's own fetched checkout: the untrusted submission.
pub(crate) fn checkout_rules() -> Vec<Rule> {
    vec![Rule::Glob("{id}/**", None)]
}

/// Everything copied from the instructor's private package: the trusted judge.
pub(crate) fn package_rules() -> Vec<Rule> {
    vec![
        Rule::File("Cargo.toml", None),
        Rule::File("Cargo.lock", None),
        Rule::Glob("{harness}/**", None),
    ]
}

/// One instructor-owned support package (`[assignment].extra-packages`),
/// resolved against `{extra}` -- so this one static table serves every
/// package, applied once per name (see [`apply_extra_packages`]).
pub(crate) fn extra_package_rules() -> Vec<Rule> {
    vec![
        Rule::File("{extra}/Cargo.toml", None),
        Rule::Glob("{extra}/src/**", None),
    ]
}

/// Overlays every `[assignment].extra-packages` package from `source` onto
/// `dest`. Always called *after* the checkout overlay, and always reading
/// from the instructor tree: a student who edits a support package in their
/// own repo has those edits overwritten here rather than compiled.
pub(crate) fn apply_extra_packages(source: &Path, dest: &Path, spec: &Spec) -> Result<()> {
    let rules = extra_package_rules();
    for package in &spec.assignment.extra_packages {
        overlay::apply(
            &Context::new(source, str_map! {"extra" => package}),
            dest,
            &rules,
        )?;
    }
    Ok(())
}

/// Builds an `EvaluationResult` for a build-stage failure that happened
/// before Evaluate ever ran (missing crate dir, disallowed dependency), so
/// a non-fatal per-submission problem still produces a well-formed,
/// gradeable result instead of aborting the batch.
fn terminal_eval(
    ctx: &JobContext,
    status: BuildStatus,
    message: Option<String>,
) -> EvaluationResult {
    EvaluationResult {
        assignment_id: ctx.assignment_id,
        github_user: ctx.github_user,
        run_id: ctx.run_id,
        input_hash: ctx.input_hash,
        status: EvalStatus::BuildFailed(status),
        wall_clock_ms: None,
        diagnostics: Diagnostics {
            compiler_errors: None,
            stderr_excerpt: message,
        },
    }
}

pub(crate) fn generate_run_id() -> RunId {
    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    RunId::new(format!(
        "{}-{:04x}",
        jiff::Timestamp::now().strftime("%Y-%m-%dT%H-%M-%SZ"),
        n
    ))
}

/// Evaluates a single submission checkout on disk, into `ctx.workspace`
/// (see `JobContext`'s doc comment for the layout). Early-returns a
/// `terminal_eval` when the checkout has no package to evaluate.
pub(crate) fn evaluate_submission(
    ctx: &JobContext,
    checkout_dir: &Path,
    assignment_dir: &Path,
    spec: &Spec,
    evaluator: &dyn Evaluator,
) -> Result<EvaluationResult> {
    let submitted_package = checkout_dir.join(spec.assignment.id.as_str());
    if !submitted_package.is_dir() {
        return Ok(terminal_eval(
            ctx,
            BuildStatus::NoPackage(spec.assignment.id),
            Some(format!(
                "submission has no {:?} directory -- expected the student's own package \
                 there, matching [assignment].id",
                spec.assignment.id
            )),
        ));
    }

    let subs = str_map! {"id" => spec.assignment.id, "harness" => spec.assignment.harness};
    overlay::apply(
        &Context::new(checkout_dir, subs.clone()),
        &ctx.workspace,
        &checkout_rules(),
    )?;
    overlay::apply(
        &Context::new(assignment_dir, subs),
        &ctx.workspace,
        &package_rules(),
    )?;
    apply_extra_packages(assignment_dir, &ctx.workspace, spec)?;

    // The sandboxed container process runs as an unprivileged,
    // rootless-podman-remapped uid, so we must grant "other" write
    // access to `ctx.workspace` so it can create new entries (Cargo.lock, target/).
    let mut perms = crate::exec::fs::metadata(&ctx.workspace)?.permissions();
    perms.set_mode(perms.mode() | 0o002);
    crate::exec::fs::set_permissions(&ctx.workspace, perms)?;

    evaluator.evaluate(ctx)
}

fn manifest_diagnostics(
    checkout_dir: &Path,
    assignment_dir: &Path,
    vendor_dir: &Path,
    spec: &Spec,
) -> Result<Vec<ManifestDiagnostic>> {
    let manifest_path = checkout_dir
        .join(spec.assignment.id.as_str())
        .join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(&manifest_path)?;

    let lock_contents = fs::read_to_string(&assignment_dir.join("Cargo.lock"))?;
    let lock = CargoLock::parse(&lock_contents)?;
    let allowed_crates = lock.direct_dependencies(spec.assignment.id.as_str());

    manifest_check::check_manifest(
        &contents,
        &allowed_crates,
        &spec.assignment.extra_packages,
        vendor_dir.is_dir().then_some(vendor_dir),
    )
}

/// The most recently persisted result for one submission, by `run_id` sort
/// order. `None` if it has never been evaluated.
pub fn latest_eval(
    submissions_dir: &Path,
    github_user: &GithubUser,
) -> Result<Option<EvaluationResult>> {
    let dir = submissions_dir.join(EVAL_DIR).join(github_user.as_str());
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut runs: Vec<_> = fs::read_dir_entries(&dir)?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().ends_with(".eval.json"))
        .collect();
    runs.sort();
    match runs.last() {
        Some(latest) => Ok(Some(read_json(latest)?)),
        None => Ok(None),
    }
}

fn save_eval(submissions_dir: &Path, eval: &EvaluationResult) -> Result<()> {
    let path = submissions_dir
        .join(EVAL_DIR)
        .join(eval.github_user.as_str())
        .join(format!("{}.eval.json", eval.run_id));
    write_json(&path, eval)
}

/// Stage orchestration for `autograder evaluate`: Prepare -> Evaluate ->
/// persist, one submission at a time. Evaluate never touches the Fetch
/// stage's own records at all -- it just enumerates the submission
/// checkouts already sitting under `submissions_dir` (see
/// `list_submissions`) -- so a submission never aborts the batch, and
/// which student (if any) a submission maps to is entirely `autograder
/// fetch`/`autograder grade`'s concern, not evaluate's. Each result is
/// persisted at `submissions_dir/.eval/<github_user>/<run_id>.eval.json`.
/// No scoring happens here -- run `autograder grade` afterwards for that.
///
/// Vendors the assignment's dependency set once up front, into
/// `deps::vendor::batch_vendor_dir`'s path under `submissions_dir`: every
/// sandboxed build runs `--offline` against it, and it's the same
/// (read-only) dir for every job. A stale `Cargo.lock` or an unresolvable
/// workspace fails the whole batch here, before any submission is touched.
pub fn evaluate_batch(
    submissions_dir: &Path,
    evaluator: &dyn Evaluator,
    assignment_dir: &Path,
    spec: &Spec,
    force: bool,
) -> Result<BatchOutcome> {
    let vendor_dir = deps::vendor::batch_vendor_dir(submissions_dir);
    deps::vendor::vendor(assignment_dir, &vendor_dir, spec)?;

    let github_users = list_submissions(submissions_dir)?;
    let mut evals = Vec::new();
    let mut skipped = 0;

    // A single reused spinner, not one `ProgressBar` per submission: each
    // iteration just changes its message/finishes it, so the terminal shows
    // one live "evaluating <id>..." line that's overwritten in place by the
    // result, rather than a scrolling log of everything printed so far.
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{spinner} {msg}").expect("static template is valid"),
    );
    progress.enable_steady_tick(std::time::Duration::from_millis(100));

    for github_user in github_users {
        progress.set_message(format!("evaluating {github_user}..."));

        let run_id = generate_run_id();
        let checkout_dir = submissions_dir.join(github_user.as_str());
        let input_hash = hash::input_hash(&checkout_dir, assignment_dir, spec)?;

        // Re-running the evaluator over unchanged inputs can only produce
        // the result already on disk, so the existing one stands.
        if !force
            && let Some(previous) = latest_eval(submissions_dir, &github_user)?
            && previous.input_hash == input_hash
        {
            skipped += 1;
            progress.suspend(|| println!("{}: skipped (unchanged)", github_user));
            evals.push(previous);
            continue;
        }

        // A fresh OS temp dir per submission, becoming `ctx.workspace`
        // (see `JobContext`'s doc comment for the layout) -- dropped and
        // cleaned up automatically at the end of this iteration.
        let build_scratch = fs::temp_dir()?;
        let ctx = JobContext {
            assignment_id: spec.assignment.id,
            github_user,
            run_id,
            workspace: build_scratch.path().to_path_buf(),
            vendor_dir: vendor_dir.clone(),
            input_hash,
        };

        let diagnostics = manifest_diagnostics(&checkout_dir, assignment_dir, &vendor_dir, spec)?;
        let eval = if diagnostics.is_empty() {
            evaluate_submission(&ctx, &checkout_dir, assignment_dir, spec, evaluator)?
        } else {
            let message = diagnostics
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            terminal_eval(&ctx, BuildStatus::DisallowedDependency, Some(message))
        };

        save_eval(submissions_dir, &eval)?;
        progress.suspend(|| println!("{}", eval.describe()));
        evals.push(eval);
    }
    progress.finish_and_clear();

    Ok(BatchOutcome { evals, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Spec;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn spec_with_extra_packages(dir: &Path, packages: &[&str]) -> Spec {
        let extra = packages
            .iter()
            .map(|p| format!("{p:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        write(
            &dir.join(crate::spec::SPEC_FILE),
            &format!(
                r#"
[assignment]
id = "hw3"
deadline = "2026-02-14T23:59:59[UTC]"
harness = "harness"
extra-packages = [{extra}]
cargo-lock-sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[sandbox]
image = "example/image:latest"

[build-limits]
wall-clock = "30s"
cpus = 1
memory = "512MiB"
pids = 64
max-output-bytes = "64KiB"

[scoring]
formula = "sum"
base = 0.0
"#
            ),
        );
        Spec::load(dir).unwrap()
    }

    /// A support package is instructor-owned, so a student who edits their
    /// own copy must not be compiled against it. `checkout_rules` never
    /// copies anything outside `{id}/`, and `apply_extra_packages` reads
    /// from the instructor tree afterwards -- between them, the bytes that
    /// land in the workspace are always the instructor's.
    #[test]
    fn an_extra_package_is_overlaid_from_the_instructor_tree_not_the_checkout() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let spec = spec_with_extra_packages(assignment_dir.path(), &["messaging"]);
        write(
            &assignment_dir.path().join("messaging/Cargo.toml"),
            "[package]\nname = \"messaging\"\nversion = \"0.1.0\"\n",
        );
        write(
            &assignment_dir.path().join("messaging/src/lib.rs"),
            "pub fn valid() -> bool { real() }",
        );

        // The student's checkout carries a tampered copy of the same package.
        let checkout_dir = tempfile::tempdir().unwrap();
        write(&checkout_dir.path().join("hw3/src/lib.rs"), "pub fn f() {}");
        write(
            &checkout_dir.path().join("messaging/src/lib.rs"),
            "pub fn valid() -> bool { true }",
        );

        let workspace = tempfile::tempdir().unwrap();
        let subs = str_map! {"id" => spec.assignment.id, "harness" => spec.assignment.harness};
        overlay::apply(
            &Context::new(checkout_dir.path(), subs),
            workspace.path(),
            &checkout_rules(),
        )
        .unwrap();
        apply_extra_packages(assignment_dir.path(), workspace.path(), &spec).unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.path().join("messaging/src/lib.rs")).unwrap(),
            "pub fn valid() -> bool { real() }"
        );
    }

    /// Every name in `extra-packages` gets its own `apply` pass, so a
    /// workspace can carry more than one support package.
    #[test]
    fn every_declared_extra_package_is_overlaid() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let spec = spec_with_extra_packages(assignment_dir.path(), &["messaging", "protocol"]);
        for package in ["messaging", "protocol"] {
            write(
                &assignment_dir.path().join(package).join("Cargo.toml"),
                &format!("[package]\nname = {package:?}\nversion = \"0.1.0\"\n"),
            );
            write(
                &assignment_dir.path().join(package).join("src/lib.rs"),
                &format!("pub fn {package}() {{}}"),
            );
        }

        let workspace = tempfile::tempdir().unwrap();
        apply_extra_packages(assignment_dir.path(), workspace.path(), &spec).unwrap();

        for package in ["messaging", "protocol"] {
            assert!(workspace.path().join(package).join("Cargo.toml").is_file());
            assert!(workspace.path().join(package).join("src/lib.rs").is_file());
        }
    }

    /// An assignment that declares no support packages is the common case
    /// and must be entirely unaffected.
    #[test]
    fn no_extra_packages_copies_nothing() {
        let assignment_dir = tempfile::tempdir().unwrap();
        let spec = spec_with_extra_packages(assignment_dir.path(), &[]);
        let workspace = tempfile::tempdir().unwrap();

        apply_extra_packages(assignment_dir.path(), workspace.path(), &spec).unwrap();

        assert_eq!(
            fs::walk_regular_files(workspace.path()).unwrap(),
            Vec::<std::path::PathBuf>::new()
        );
    }
}
