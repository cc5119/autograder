pub mod ci;
pub mod evaluate;
pub mod fetch;
pub mod grade;
pub mod init;
pub mod lock;
pub mod publish;
pub mod push;
pub mod register;
pub mod show;

use std::path::Path;

use crate::cli::Command;
use crate::error::Result;
use crate::exec::sandbox::{ContainerSandbox, LocalSandbox, Sandbox};
use crate::pipeline::evaluator::Evaluator;
use crate::pipeline::evaluator::nextest::Nextest;
use crate::spec::Spec;

pub fn dispatch(command: Command, verbose: bool) -> Result<()> {
    match command {
        Command::Init { dir, id } => init::run(&dir, &id),
        Command::Lock { assignment } => lock::run(&assignment),
        Command::Fetch {
            assignment,
            repo,
            roster,
            out,
            as_of,
            detach,
            jobs,
        } => fetch::run(
            &assignment,
            &repo,
            &roster,
            &out,
            as_of,
            detach.choice(),
            jobs,
        ),
        Command::Evaluate {
            assignment,
            submissions,
            local_sandbox,
        } => evaluate::run(&assignment, &submissions, local_sandbox),
        Command::Ci { local_sandbox } => ci::run(local_sandbox),
        Command::Grade {
            assignment,
            submissions,
        } => grade::run(&assignment, &submissions),
        Command::Show { submission } => show::run(&submission, verbose),
        Command::Publish {
            assignment,
            out,
            mode,
        } => publish::run(&assignment, &out, mode.map(Into::into)),
        Command::Push { assignment } => push::run(&assignment),
        Command::Register {
            roster,
            org,
            team,
            yes,
        } => register::run(&roster, &org, &team, yes),
    }
}

pub(crate) fn build_evaluator(
    spec: &Spec,
    assignment_dir: &Path,
    vendor_dir: &Path,
    local_sandbox: bool,
) -> Result<Box<dyn Evaluator>> {
    if local_sandbox {
        tracing::warn!(
            "grading with --local-sandbox: skipping Podman entirely, running student code as a \
             host process with no container isolation -- for local development/ \
             testing only, never for grading real submissions"
        );
        build_evaluator_for(spec, assignment_dir, vendor_dir, LocalSandbox)
    } else {
        let sandbox = ContainerSandbox::new(spec.sandbox.image.clone());
        sandbox.preflight()?;
        build_evaluator_for(spec, assignment_dir, vendor_dir, sandbox)
    }
}

fn build_evaluator_for(
    spec: &Spec,
    assignment_dir: &Path,
    vendor_dir: &Path,
    sandbox: impl Sandbox + 'static,
) -> Result<Box<dyn Evaluator>> {
    Ok(Box::new(Nextest::new(
        spec,
        assignment_dir,
        vendor_dir,
        sandbox,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::exec::sandbox::LocalSandbox;
    use crate::model::{self, JobContext};
    use crate::report::ci::CiReport;

    fn write(path: &std::path::Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// Written to the workspace root alongside `Cargo.toml` in every test
    /// that reaches `prepare` -- its hash is what `public_spec` embeds.
    const LOCK_TOML: &str = "version = 4\n\n[[package]]\nname = \"harness\"\nversion = \"0.0.0\"\ndependencies = [\n \"hw3\",\n]\n\n[[package]]\nname = \"hw3\"\nversion = \"0.1.0\"\n";

    fn public_spec() -> String {
        format!(
            r#"
[assignment]
id = "hw3"
name = "Binary search tree"
kind = "library"
deadline = "2026-02-14T23:59:59-08:00[America/Los_Angeles]"
harness = "harness"
cargo-lock-sha256 = "{}"

[sandbox]
image = "autograder-base:1.86.0"

[build-limits]
wall-clock = "30s"
cpus = 1
memory = "512MiB"
pids = 64
max-output-bytes = "64KiB"

[scoring]
formula = "sum"
base = 0.0
"#,
            crate::deps::cargo_lock::sha256_hex(LOCK_TOML)
        )
    }

    /// This host has no `cargo-nextest`. Only stage 3 needs it, so the two
    /// build stages succeed, but `cargo nextest` itself exits with
    /// "no such command" (neither `0` nor `100`) -- reported as `Crashed`,
    /// not a silent pass, before `junit.xml` is even consulted.
    #[test]
    fn ci_pipeline_runs_prepare_and_evaluate_end_to_end_short_of_nextest() {
        let harness_dir = tempfile::tempdir().unwrap();
        write(
            &harness_dir.path().join(crate::spec::SPEC_FILE),
            &public_spec(),
        );
        write(
            &harness_dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"harness\", \"hw3\"]\n",
        );
        write(&harness_dir.path().join("Cargo.lock"), LOCK_TOML);
        write(
            &harness_dir.path().join("harness/Cargo.toml"),
            "[package]\nname = \"harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [[bin]]\nname = \"driver\"\npath = \"src/bin/driver.rs\"\n\n\
             [dependencies]\nhw3 = { path = \"../hw3\" }\n",
        );
        write(
            &harness_dir.path().join("harness/src/bin/driver.rs"),
            "fn main() {}\n",
        );

        let submission_dir = harness_dir.path().join("hw3");
        write(
            &submission_dir.join("Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&submission_dir.join("src/lib.rs"), "pub fn noop() {}\n");

        let spec = Spec::load(harness_dir.path()).unwrap();
        let ctx = JobContext {
            assignment_id: spec.assignment.id,
            github_user: "local".into(),
            run_id: "run-1".into(),
            workspace: harness_dir.path().to_path_buf(),
        };
        let evaluator = build_evaluator_for(
            &spec,
            harness_dir.path(),
            &harness_dir.path().join(".vendor"),
            LocalSandbox,
        )
        .unwrap();
        let eval = evaluator.evaluate(&ctx).unwrap();

        assert!(matches!(
            eval.status,
            model::EvalStatus::Ran {
                tests: model::TestOutcome::Unavailable(_),
                ..
            }
        ));

        let report = CiReport {
            eval: Some(&eval),
            lockfile_mismatch: None,
        };
        assert!(!report.passed());
    }

    /// Evaluator construction is kind-agnostic now (see
    /// `pipeline::evaluator::nextest`'s module doc comment) -- it only
    /// needs the harness manifest to exist, regardless of `kind`.
    #[test]
    fn ci_evaluator_selection_succeeds_when_the_harness_dir_exists() {
        let spec: Spec = toml::from_str(&public_spec()).unwrap();

        let harness_dir = tempfile::tempdir().unwrap();
        write(
            &harness_dir.path().join("harness/Cargo.toml"),
            "[package]\nname = \"harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        let result = build_evaluator_for(
            &spec,
            harness_dir.path(),
            &harness_dir.path().join(".vendor"),
            LocalSandbox,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn ci_evaluator_selection_errors_clearly_when_the_harness_dir_is_missing() {
        let spec: Spec = toml::from_str(&public_spec()).unwrap();

        let harness_dir = tempfile::tempdir().unwrap();
        let result = build_evaluator_for(
            &spec,
            harness_dir.path(),
            &harness_dir.path().join(".vendor"),
            LocalSandbox,
        );
        assert!(matches!(result, Err(Error::InvalidSpec(_))));
    }

    /// `evaluate --local-sandbox` must never touch `ContainerSandbox` or
    /// require Podman.
    #[test]
    fn evaluate_with_local_sandbox_never_requires_podman() {
        let assignment_dir = tempfile::tempdir().unwrap();
        write(
            &assignment_dir.path().join(crate::spec::SPEC_FILE),
            &public_spec(),
        );
        write(
            &assignment_dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"harness\", \"hw3\"]\n",
        );
        write(&assignment_dir.path().join("Cargo.lock"), LOCK_TOML);
        write(
            &assignment_dir.path().join("harness/Cargo.toml"),
            "[package]\nname = \"harness\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [[bin]]\nname = \"driver\"\npath = \"src/bin/driver.rs\"\n\n\
             [dependencies]\nhw3 = { path = \"../hw3\" }\n",
        );
        write(
            &assignment_dir.path().join("harness/src/bin/driver.rs"),
            "fn main() {}\n",
        );
        // `evaluate` vendors the whole workspace up front, so every member
        // `Cargo.toml` the root lists has to be real and resolve to exactly
        // the checked-in `LOCK_TOML` -- `cargo vendor` runs `--locked`.
        write(
            &assignment_dir.path().join("hw3/Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&assignment_dir.path().join("hw3/src/lib.rs"), "");

        // Just a submission checkout -- evaluate never looks for a fetch
        // record at all.
        let submissions_dir = tempfile::tempdir().unwrap();
        write(
            &submissions_dir.path().join("alice/hw3/Cargo.toml"),
            "[package]\nname = \"hw3\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(
            &submissions_dir.path().join("alice/hw3/src/lib.rs"),
            "pub fn noop() {}\n",
        );

        evaluate::run(assignment_dir.path(), submissions_dir.path(), true).unwrap();
        grade::run(assignment_dir.path(), submissions_dir.path()).unwrap();

        let gradebook =
            std::fs::read_to_string(submissions_dir.path().join(".grades/grades.csv")).unwrap();
        // This host has no `cargo-nextest`, which only stage 3 (run) needs
        // -- see `ci_pipeline_runs_prepare_and_evaluate_end_to_end_short_of_nextest`.
        // No readable test results means no trustworthy score, so the
        // gradebook row's score is blank.
        assert!(gradebook.contains("alice,"));
    }
}
