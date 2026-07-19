//! Publishes the starter/template repo for distribution to students
//! from the **private instructor package** in one pass: copy everything
//! real, then strip the sensitive parts in place. No hand-maintained
//! `public/` sibling repo, and no separately-assembled `Cargo.toml`/`src/`
//! built up from spec fields in Rust code.
//!
//! Two independent, mechanical transforms feed the copy pass below — no
//! name resolution, no judgment calls about what's "safe," just
//! structural filtering against data the private spec already carries:
//!
//! - [`derive_public_spec_toml`]: strips `points` and any non-`public`
//!   `[[scoring.tests]]` entries out of the private spec's raw TOML via a
//!   generic `toml::Value` edit — no need to round-trip through `Spec`'s
//!   typed fields, since points/visibility are already there in the text.
//! - [`keep_only_named_tests`]: parses a harness test file with `syn` and
//!   drops any `#[test]` fn not named in the public test list returned by
//!   the spec transform above — the same "only what's explicitly
//!   known-safe survives" shape as [`crate::stub`], applied to test
//!   functions instead of solution code.
//!
//! `harness/Cargo.toml` itself needs no transform: it's already a plain
//! path dependency on the sibling directory named after `[assignment].id`
//! (see `evaluator::library`'s module doc comment), which means the same
//! thing whether that sibling is the private reference solution or the
//! published starter's copy of the student's own crate — `publish` copies
//! it verbatim.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ignore::overrides::OverrideBuilder;
use syn::Item;

use crate::error::{Error, Result};
use crate::spec::{self, AssignmentKind, Spec};

/// Placeholder release coordinates for the downloaded `autograder` binary
/// (design §11.2). The real pinned version + sha256 come from the release
/// pipeline (M3 step 19, `.github/workflows/release.yml` in this repo); an
/// instructor stands up their own fork/release and edits these before
/// distributing a starter repo.
const RELEASE_REPO: &str = "your-org/autograder";
const RELEASE_VERSION: &str = "v0.1.0";
const RELEASE_SHA256_PLACEHOLDER: &str = "<pinned-sha256>";

#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub out_dir: PathBuf,
}

struct PublishCtx {
    package_dir: PathBuf,
    id: String,
    public_test_names: HashSet<String>,
}

struct MatchedFile {
    rel_path: PathBuf,
    content: String,
}

type FileHook = fn(path: &str, file: MatchedFile, ctx: &PublishCtx) -> Result<MatchedFile>;

type GlobHook =
    fn(pattern: &str, matches: Vec<MatchedFile>, ctx: &PublishCtx) -> Result<Vec<MatchedFile>>;

enum Rule {
    File(&'static str, Option<FileHook>),
    Glob(&'static str, Option<GlobHook>),
}

fn rules(kind: AssignmentKind) -> Vec<Rule> {
    match kind {
        AssignmentKind::Library => vec![
            Rule::File("Cargo.toml", None),
            Rule::File("{id}/Cargo.toml", Some(validate_manifest)),
            Rule::Glob("{id}/src/**", Some(strip_stub)),
            Rule::File("harness/Cargo.toml", None),
            Rule::Glob("harness/src/**", None),
            Rule::Glob("harness/tests/**", Some(keep_only_public_tests)),
        ],
        AssignmentKind::Binary => vec![
            Rule::File("Cargo.toml", None),
            Rule::File("{id}/Cargo.toml", Some(validate_manifest)),
            Rule::Glob("{id}/src/**", Some(strip_stub)),
            Rule::Glob("{id}/tests/**", Some(keep_only_public_tests)),
        ],
    }
}

pub fn publish(package_dir: &Path, out_dir: &Path) -> Result<PublishOutcome> {
    let private_spec_path = package_dir.join(spec::PRIVATE_SPEC_FILE);
    if !private_spec_path.is_file() {
        return Err(Error::InvalidSpec(format!(
            "publish requires {} in {} (the private instructor package)",
            spec::PRIVATE_SPEC_FILE,
            package_dir.display()
        )));
    }
    let spec = Spec::load_file(&private_spec_path)?;
    let id = spec.assignment.id.clone();

    // Derived purely from the private TOML text -- doesn't need anything
    // copied yet, so it can run before the walk instead of being
    // interleaved with it.
    let private_toml = std::fs::read_to_string(&private_spec_path).map_err(|source| Error::Io {
        path: private_spec_path.clone(),
        source,
    })?;
    let (public_spec_toml, public_test_names) = derive_public_spec_toml(&private_toml)?;

    let ctx = PublishCtx {
        package_dir: package_dir.to_path_buf(),
        id: id.clone(),
        public_test_names,
    };

    copy_matching(&ctx, out_dir, &rules(spec.assignment.kind))?;

    let student_dir = out_dir.join(&id);
    run_cargo_fix(&student_dir)?;

    let public_spec_path = out_dir.join(spec::PUBLIC_SPEC_FILE);
    std::fs::write(&public_spec_path, public_spec_toml).map_err(|source| Error::Io {
        path: public_spec_path,
        source,
    })?;

    let workflow_dir = out_dir.join(".github/workflows");
    std::fs::create_dir_all(&workflow_dir).map_err(|source| Error::Io {
        path: workflow_dir.clone(),
        source,
    })?;
    let workflow_path = workflow_dir.join("autograde.yml");
    let workflow_yaml = autograde_workflow_yaml(&spec.sandbox.image);
    std::fs::write(&workflow_path, workflow_yaml).map_err(|source| Error::Io {
        path: workflow_path.clone(),
        source,
    })?;

    Ok(PublishOutcome {
        out_dir: out_dir.to_path_buf(),
    })
}

fn copy_matching(ctx: &PublishCtx, out_dir: &Path, rules: &[Rule]) -> Result<()> {
    let mut all_files = Vec::new();
    walk_files(&ctx.package_dir, &ctx.package_dir, &mut all_files)?;

    for rule in rules {
        let output_files = match rule {
            Rule::File(path, hook) => {
                let rel_path = PathBuf::from(path.replace("{id}", &ctx.id));
                if !ctx.package_dir.join(&rel_path).is_file() {
                    return Err(Error::InvalidSpec(format!(
                        "publish requires {} under {} (the private instructor package) -- \
                         there is nothing to copy the starter's `{}` from",
                        rel_path.display(),
                        ctx.package_dir.display(),
                        rel_path.display()
                    )));
                }
                let file = read_file(&ctx.package_dir, rel_path)?;
                match hook {
                    Some(hook) => vec![hook(path, file, ctx)?],
                    None => vec![file],
                }
            }
            Rule::Glob(pattern, hook) => {
                let pattern = pattern.replace("{id}", &ctx.id);
                let override_ = OverrideBuilder::new(&ctx.package_dir)
                    .add(&pattern)
                    .unwrap()
                    .build()
                    .unwrap();

                let mut matches = Vec::new();
                for rel_path in &all_files {
                    if override_.matched(rel_path, false).is_whitelist() {
                        matches.push(read_file(&ctx.package_dir, rel_path.clone())?);
                    }
                }

                match hook {
                    Some(hook) => hook(&pattern, matches, ctx)?,
                    None => matches,
                }
            }
        };

        for file in output_files {
            write_file(out_dir, file)?;
        }
    }

    Ok(())
}

fn read_file(package_dir: &Path, rel_path: PathBuf) -> Result<MatchedFile> {
    let full_path = package_dir.join(&rel_path);
    let content = std::fs::read_to_string(&full_path).map_err(|source| Error::Io {
        path: full_path,
        source,
    })?;
    Ok(MatchedFile { rel_path, content })
}

fn write_file(out_dir: &Path, file: MatchedFile) -> Result<()> {
    let dst = out_dir.join(&file.rel_path);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&dst, file.content).map_err(|source| Error::Io { path: dst, source })
}

/// Collects every regular file under `dir`, recursively, as paths relative
/// to `root`.
fn walk_files(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).map_err(|source| Error::Io {
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            walk_files(&path, root, out)?;
        } else if file_type.is_file() {
            out.push(
                path.strip_prefix(root)
                    .expect("path is under root")
                    .to_path_buf(),
            );
        }
    }
    Ok(())
}

fn validate_manifest(path: &str, file: MatchedFile, ctx: &PublishCtx) -> Result<MatchedFile> {
    let manifest_path = ctx.package_dir.join(path);
    let value: toml::Value = toml::from_str(&file.content).map_err(|source| Error::Toml {
        path: manifest_path.clone(),
        source: Box::new(source),
    })?;
    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    if package_name != Some(ctx.id.as_str()) {
        return Err(Error::InvalidSpec(format!(
            "{} has [package].name = {:?}, expected {:?} to match [assignment].id -- \
             rename the package, or the solution directory/id, so they agree",
            manifest_path.display(),
            package_name.unwrap_or("<missing>"),
            ctx.id
        )));
    }
    Ok(file)
}

/// Strips every `.rs` file among the matches to a stub via
/// [`crate::stub::strip_to_stub`]: pub signatures survive, bodies become
/// `todo!()`, private items and `#[cfg(test)]` modules are dropped.
/// Non-`.rs` files (rare, but a solution crate could have one under
/// `src/`) pass through untouched.
fn strip_stub(
    _pattern: &str,
    matches: Vec<MatchedFile>,
    _ctx: &PublishCtx,
) -> Result<Vec<MatchedFile>> {
    matches
        .into_iter()
        .map(|file| {
            if file.rel_path.extension().is_some_and(|ext| ext == "rs") {
                let stripped = crate::stub::strip_to_stub(&file.content)?;
                Ok(MatchedFile {
                    content: stripped,
                    ..file
                })
            } else {
                Ok(file)
            }
        })
        .collect()
}

/// Filters every `.rs` file among the matches down to just the `#[test]`
/// fns named in `ctx.public_test_names`, via [`keep_only_named_tests`].
/// Used for both `library`'s `harness/tests/**` and `binary`'s
/// `{id}/tests/**` -- the latter is the reference solution's own
/// (private) judge, but this hook only ever runs against the `tests/**`
/// rule, never `src/**`, so it can't collide with [`strip_stub`] mangling
/// real `#[test]` assertions into `todo!()` stubs.
fn keep_only_public_tests(
    _pattern: &str,
    matches: Vec<MatchedFile>,
    ctx: &PublishCtx,
) -> Result<Vec<MatchedFile>> {
    matches
        .into_iter()
        .map(|file| {
            if file.rel_path.extension().is_some_and(|ext| ext == "rs") {
                let filtered = keep_only_named_tests(&file.content, &ctx.public_test_names)?;
                Ok(MatchedFile {
                    content: filtered,
                    ..file
                })
            } else {
                Ok(file)
            }
        })
        .collect()
}

/// The thin GitHub Actions wrapper (design §11.2): downloads a
/// version-pinned prebuilt `autograder` binary, verifies its checksum, and
/// runs `autograder ci` from the repo root (`ci` takes no `--harness` flag
/// -- it always runs from the checkout root where `autograder.public.toml`/
/// `harness/` live, finding the student's own crate at the sibling
/// directory named after `[assignment].id`). No compile step, so student CI
/// never needs a Rust toolchain of its own for the grader itself.
///
/// `ci` now runs inside the same `ContainerSandbox` grading uses (design
/// §10/§11 unified), so before that step the workflow also has to: make
/// sure Podman is on the runner (`ubuntu-latest` doesn't ship it by
/// default), vendor dependencies offline via `autograder prefetch` (reusing
/// `vendor::prefetch` unchanged -- runs on the bare runner with network
/// access, *before* the sandboxed, network-disabled `ci` step, from the
/// public spec's `[allowed-crates]`), and `podman pull` the same base
/// image (`base_image`, from `[sandbox].image`) grading uses -- the
/// instructor is responsible for having pushed that image somewhere this
/// runner can pull it from.
fn autograde_workflow_yaml(base_image: &str) -> String {
    format!(
        r#"name: autograde
on:
  push:
    branches: [main]        # default branch only
jobs:
  public-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          curl -fsSL https://github.com/{RELEASE_REPO}/releases/download/{RELEASE_VERSION}/autograder-x86_64-linux -o autograder
          echo "{RELEASE_SHA256_PLACEHOLDER}  autograder" | sha256sum -c -   # verify before exec
          chmod +x autograder
      - run: command -v podman || (sudo apt-get update && sudo apt-get install -y podman)
      - run: ./autograder prefetch .
      - run: podman pull {base_image}
      - run: ./autograder ci
"#
    )
}

/// Runs `cargo fix` directly in `student_dir` (already a full, disposable
/// copy of the solution crate at this point -- no separate scratch copy
/// needed) to prune whichever `use` lines stub-stripping left unused. The
/// real compiler is the authority on what's unused, not hand-rolled name
/// resolution. Cleans up the `target/` directory `cargo fix` leaves behind
/// afterward, so the shipped starter tree carries no build output.
fn run_cargo_fix(student_dir: &Path) -> Result<()> {
    let output = std::process::Command::new("cargo")
        .args(["fix", "--allow-dirty", "--allow-staged", "--allow-no-vcs"])
        .current_dir(student_dir)
        .output()
        .map_err(|source| Error::Io {
            path: student_dir.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "cargo fix failed while stripping the starter at {}:\n{}",
            student_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let target_dir = student_dir.join("target");
    if target_dir.is_dir() {
        std::fs::remove_dir_all(&target_dir).map_err(|source| Error::Io {
            path: target_dir,
            source,
        })?;
    }

    Ok(())
}

/// Returns the public spec TOML text plus the set of test names it kept
/// (visibility = "public"), so the caller can filter harness test files to
/// match.
pub fn derive_public_spec_toml(private_toml: &str) -> Result<(String, HashSet<String>)> {
    let mut value: toml::Value = toml::from_str(private_toml).map_err(|source| Error::Toml {
        path: PathBuf::from("<private spec>"),
        source: Box::new(source),
    })?;

    let mut public_names = HashSet::new();
    if let Some(tests) = value
        .get_mut("scoring")
        .and_then(|s| s.get_mut("tests"))
        .and_then(|t| t.as_array_mut())
    {
        tests.retain(|t| {
            let is_public = t.get("visibility").and_then(|v| v.as_str()) == Some("public");
            if is_public {
                if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                    public_names.insert(name.to_string());
                }
            }
            is_public
        });
        for t in tests.iter_mut() {
            if let Some(table) = t.as_table_mut() {
                table.remove("points");
            }
        }
    }

    let rendered = toml::to_string_pretty(&value)
        .map_err(|e| Error::Other(format!("failed to render public spec: {e}")))?;
    Ok((rendered, public_names))
}

/// Drops any `#[test]` fn not named in `keep_names`; everything else
/// (helper structs/impls, `use`s, non-test fns) passes through untouched.
pub fn keep_only_named_tests(source: &str, keep_names: &HashSet<String>) -> Result<String> {
    let file = syn::parse_file(source)
        .map_err(|e| Error::Other(format!("failed to parse harness test file: {e}")))?;

    let items: Vec<Item> = file
        .items
        .into_iter()
        .filter(|item| match item {
            Item::Fn(f) => {
                let is_test = f.attrs.iter().any(|a| a.path().is_ident("test"));
                !is_test || keep_names.contains(&f.sig.ident.to_string())
            }
            _ => true,
        })
        .collect();

    let filtered = syn::File {
        shebang: file.shebang,
        attrs: file.attrs,
        items,
    };
    Ok(prettyplease::unparse(&filtered))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn derive_public_spec_toml_drops_points_and_hidden_tests() {
        let (public_toml, names) = derive_public_spec_toml(PRIVATE_TOML).unwrap();
        assert!(!public_toml.contains("points"));
        assert!(!public_toml.contains("balance_adversarial"));
        assert!(public_toml.contains("insert_basic"));
        assert_eq!(names, HashSet::from(["insert_basic".to_string()]));
    }

    #[test]
    fn keep_only_named_tests_drops_unlisted_tests_but_keeps_everything_else() {
        let source = r#"
            struct Session;

            impl Session {
                fn start() -> Self { Session }
            }

            #[test]
            fn insert_basic() {
                assert!(true);
            }

            #[test]
            fn balance_adversarial() {
                assert!(true);
            }
        "#;
        let keep = HashSet::from(["insert_basic".to_string()]);
        let filtered = keep_only_named_tests(source, &keep).unwrap();
        assert!(filtered.contains("fn insert_basic"));
        assert!(!filtered.contains("balance_adversarial"));
        assert!(filtered.contains("struct Session"));
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const PRIVATE_SPEC: &str = r#"
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

    const HARNESS_MANIFEST: &str = "[package]\nname = \"driver\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nhw3 = { path = \"../hw3\" }\n";

    const JUDGE_RS: &str = r#"
        #[test]
        fn insert_basic() {
            assert!(true);
        }

        #[test]
        fn balance_adversarial() {
            assert!(true);
        }
    "#;

    fn write_solution_crate(solution_dir: &Path, package_name: &str) {
        write(
            &solution_dir.join("Cargo.toml"),
            &format!(
                "[package]\nname = \"{package_name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n"
            ),
        );
        write(
            &solution_dir.join("src/lib.rs"),
            r#"
                use std::collections::HashSet;

                pub struct Stack<T> {
                    items: Vec<T>,
                }

                impl<T> Stack<T> {
                    pub fn new() -> Self {
                        Stack { items: Vec::new() }
                    }

                    pub fn push(&mut self, value: T) {
                        self.items.push(value);
                    }

                    fn dedup_hint(&self) -> HashSet<usize> {
                        HashSet::new()
                    }
                }
            "#,
        );
    }

    /// Writes a full instructor package, solution included (named after
    /// `PRIVATE_SPEC`'s `id = "hw3"`) -- a solution is required for
    /// `publish` to produce anything, so every test that isn't
    /// specifically about a missing/mismatched solution needs one.
    /// `harness/src/main.rs` matters here in a way it didn't before this
    /// module started emitting a `[workspace]`: with `harness` now a real
    /// workspace member, a `cargo build`/`cargo test` from the starter
    /// root needs it to actually be a buildable crate, not just a
    /// manifest.
    fn write_instructor_package(package_dir: &Path) {
        write(&package_dir.join(spec::PRIVATE_SPEC_FILE), PRIVATE_SPEC);
        write(
            &package_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"harness\", \"hw3\"]\n",
        );
        write(&package_dir.join("harness/Cargo.toml"), HARNESS_MANIFEST);
        write(&package_dir.join("harness/src/main.rs"), "fn main() {}\n");
        write(&package_dir.join("harness/tests/judge.rs"), JUDGE_RS);
        write_solution_crate(&package_dir.join("hw3"), "hw3");
    }

    #[test]
    fn publish_produces_the_documented_starter_tree() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());

        let out_dir = tempfile::tempdir().unwrap();
        let outcome = publish(package_dir.path(), out_dir.path()).unwrap();

        assert!(outcome.out_dir.join(spec::PUBLIC_SPEC_FILE).is_file());
        assert!(outcome.out_dir.join("harness/tests/judge.rs").is_file());
        assert!(
            outcome
                .out_dir
                .join(".github/workflows/autograde.yml")
                .is_file()
        );
        // The root manifest is now the workspace, not the student's own
        // package -- that lives at `<id>/`.
        assert!(outcome.out_dir.join("Cargo.toml").is_file());
        assert!(outcome.out_dir.join("hw3/Cargo.toml").is_file());
        assert!(outcome.out_dir.join("hw3/src/lib.rs").is_file());
    }

    #[test]
    fn publish_derives_a_public_spec_with_no_points_or_hidden_tests() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let public_spec =
            std::fs::read_to_string(out_dir.path().join(spec::PUBLIC_SPEC_FILE)).unwrap();
        assert!(!public_spec.contains("points"));
        assert!(!public_spec.contains("balance_adversarial"));
        assert!(public_spec.contains("insert_basic"));
    }

    #[test]
    fn publish_derives_a_public_harness_with_only_the_public_test_and_a_path_dependency() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(out_dir.path().join("harness/tests/judge.rs")).unwrap();
        assert!(judge.contains("fn insert_basic"));
        assert!(!judge.contains("balance_adversarial"));

        let manifest = std::fs::read_to_string(out_dir.path().join("harness/Cargo.toml")).unwrap();
        assert!(!manifest.contains("patch"));
        // `../hw3` now means something real: the student's own crate,
        // a sibling of `harness/` at the starter root (see
        // `write_instructor_package`) -- not a dangling reference to a
        // solution directory that doesn't exist in the starter.
        assert!(manifest.contains("path = \"../hw3\""));
    }

    #[test]
    fn publish_errors_clearly_when_the_solution_directory_is_missing() {
        let package_dir = tempfile::tempdir().unwrap();
        write(
            &package_dir.path().join(spec::PRIVATE_SPEC_FILE),
            PRIVATE_SPEC,
        );
        write(
            &package_dir.path().join("harness/Cargo.toml"),
            HARNESS_MANIFEST,
        );
        write(&package_dir.path().join("harness/tests/judge.rs"), JUDGE_RS);
        // deliberately no `hw3/` solution directory
        let out_dir = tempfile::tempdir().unwrap();

        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn emitted_workflow_runs_ci_from_the_repo_root_inside_podman() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let workflow =
            std::fs::read_to_string(out_dir.path().join(".github/workflows/autograde.yml"))
                .unwrap();
        assert!(workflow.contains("on:\n  push:\n    branches: [main]"));
        assert!(workflow.contains("sha256sum -c -"));
        assert!(workflow.contains("command -v podman"));
        assert!(workflow.contains("./autograder prefetch ."));
        assert!(workflow.contains("podman pull autograder-base:1.86.0"));
        assert!(workflow.contains("./autograder ci"));
        assert!(!workflow.contains("--harness"));
    }

    #[test]
    fn emitted_workspace_manifest_lists_the_harness_and_student_crate() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let workspace_manifest =
            std::fs::read_to_string(out_dir.path().join("Cargo.toml")).unwrap();
        assert_eq!(
            workspace_manifest,
            "[workspace]\nmembers = [\"harness\", \"hw3\"]\n"
        );
    }

    #[test]
    fn emitted_student_manifest_matches_the_solutions_own_cargo_toml() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let solution_manifest =
            std::fs::read_to_string(package_dir.path().join("hw3/Cargo.toml")).unwrap();
        let starter_manifest =
            std::fs::read_to_string(out_dir.path().join("hw3/Cargo.toml")).unwrap();
        assert_eq!(starter_manifest, solution_manifest);
    }

    #[test]
    fn publish_rejects_a_package_dir_without_a_private_spec() {
        let package_dir = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();

        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    /// The root `[workspace]` manifest is copied from the private package,
    /// not regenerated -- refuses to guess one if it's missing, same as
    /// `autograder.toml` or the solution directory.
    #[test]
    fn publish_rejects_a_package_dir_without_a_root_workspace_manifest() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        std::fs::remove_file(package_dir.path().join("Cargo.toml")).unwrap();
        let out_dir = tempfile::tempdir().unwrap();

        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    /// End-to-end: given the real solution crate `write_instructor_package`
    /// puts at `package_dir/<id>`, the emitted starter's `<id>/` crate
    /// builds on its own (implying `cargo fix` left it in a compiling
    /// state), exposes the pub API, stubs bodies to `todo!()`, and never
    /// leaks the private helper or its now-unused import.
    #[test]
    fn publish_derives_a_building_stub_from_the_id_named_solution_dir() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());

        let out_dir = tempfile::tempdir().unwrap();
        publish(package_dir.path(), out_dir.path()).unwrap();

        let src = std::fs::read_to_string(out_dir.path().join("hw3/src/lib.rs")).unwrap();
        assert!(src.contains("pub struct Stack"));
        assert!(src.contains("pub fn new"));
        assert!(src.contains("pub fn push"));
        assert!(src.contains("todo!"));
        assert!(!src.contains("dedup_hint"));
        assert!(!src.contains("HashSet"));

        let build = std::process::Command::new("cargo")
            .arg("build")
            .current_dir(out_dir.path().join("hw3"))
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "published starter failed to build: {}",
            String::from_utf8_lossy(&build.stderr)
        );
    }

    /// The whole point of the new layout: a plain `cargo test` run from
    /// the starter's own root -- no `autograder` involvement, no `cd`,
    /// no flags -- builds and runs the public harness's judge tests
    /// (`insert_basic`, since `balance_adversarial` was filtered out as
    /// hidden) against the student's stub, via the `[workspace]` manifest
    /// and the harness's plain path dependency on `hw3`.
    #[test]
    fn cargo_test_at_the_starter_root_runs_the_public_harness() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());

        let out_dir = tempfile::tempdir().unwrap();
        publish(package_dir.path(), out_dir.path()).unwrap();

        let test = std::process::Command::new("cargo")
            .arg("test")
            .current_dir(out_dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&test.stdout);
        assert!(
            test.status.success(),
            "cargo test at the starter root failed: {}{}",
            stdout,
            String::from_utf8_lossy(&test.stderr)
        );
        assert!(stdout.contains("insert_basic"));
    }

    #[test]
    fn publish_rejects_a_solution_dir_whose_package_name_does_not_match_the_id() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        write_solution_crate(&package_dir.path().join("hw3"), "wrong-name");

        let out_dir = tempfile::tempdir().unwrap();
        let err = publish(package_dir.path(), out_dir.path()).unwrap_err();
        assert!(matches!(err, Error::InvalidSpec(_)));
    }

    #[test]
    fn publish_never_copies_a_vendor_directory_dropped_in_the_solution_crate() {
        let package_dir = tempfile::tempdir().unwrap();
        write_instructor_package(package_dir.path());
        write(
            &package_dir.path().join("hw3/vendor/some-crate/src/lib.rs"),
            "// prefetch output, never checked into the starter\n",
        );
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        assert!(!out_dir.path().join("hw3/vendor").exists());
    }

    const BINARY_PRIVATE_SPEC: &str = r#"
[assignment]
id = "wc"
name = "Word count"
kind = "binary"
deadline = "2026-02-14T23:59:59-08:00"

[sandbox]
image = "autograder-base:1.86.0"

[allowed-crates]

[limits.build]
wall-clock = "60s"
cpus = 2
memory = "1GiB"
pids = 128

[limits.run]
cpu-time = "5s"
wall-clock = "10s"
cpus = 1
memory = "256MiB"
pids = 64
max-output-bytes = "64KiB"

[scoring]
model = "weighted"

[[scoring.tests]]
name = "counts_words"
points = 10
visibility = "public"

[[scoring.tests]]
name = "counts_zero_for_empty_input"
points = 20
visibility = "private"
"#;

    const BINARY_JUDGE_RS: &str = r#"
        #[test]
        fn counts_words() {
            assert!(true);
        }

        #[test]
        fn counts_zero_for_empty_input() {
            assert!(true);
        }
    "#;

    /// `binary`-kind has no separate `harness/` crate at all -- the private
    /// judge lives directly inside the reference solution's own `tests/`
    /// (see `evaluator::binary`'s module doc comment).
    fn write_binary_instructor_package(package_dir: &Path) {
        write(
            &package_dir.join(spec::PRIVATE_SPEC_FILE),
            BINARY_PRIVATE_SPEC,
        );
        write(
            &package_dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"wc\"]\n",
        );
        write(
            &package_dir.join("wc/Cargo.toml"),
            "[package]\nname = \"wc\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        );
        write(
            &package_dir.join("wc/src/main.rs"),
            "pub fn count(s: &str) -> usize { s.split_whitespace().count() }\nfn main() {}\n",
        );
        write(&package_dir.join("wc/tests/judge.rs"), BINARY_JUDGE_RS);
    }

    #[test]
    fn publish_derives_a_public_binary_judge_with_no_separate_harness_dir() {
        let package_dir = tempfile::tempdir().unwrap();
        write_binary_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(out_dir.path().join("wc/tests/judge.rs")).unwrap();
        assert!(judge.contains("fn counts_words"));
        assert!(!judge.contains("counts_zero_for_empty_input"));

        assert!(!out_dir.path().join("harness").exists());
        assert_eq!(
            std::fs::read_to_string(out_dir.path().join("Cargo.toml")).unwrap(),
            "[workspace]\nmembers = [\"wc\"]\n"
        );
    }

    /// The reference solution's own private judge tests must never get
    /// mangled by the stub-stripping pass meant for implementation code --
    /// confirms the public test's real assertion survives, not a `todo!()`.
    #[test]
    fn publish_never_stubs_the_binary_judges_test_bodies() {
        let package_dir = tempfile::tempdir().unwrap();
        write_binary_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();

        publish(package_dir.path(), out_dir.path()).unwrap();

        let judge = std::fs::read_to_string(out_dir.path().join("wc/tests/judge.rs")).unwrap();
        assert!(judge.contains("assert!(true)"));
        assert!(!judge.contains("todo!"));
    }

    /// Mirrors `cargo_test_at_the_starter_root_runs_the_public_harness` for
    /// `binary`-kind: no `autograder` involvement, just the workspace
    /// manifest plus the judge tests already sitting in `wc/tests/`.
    #[test]
    fn cargo_test_at_the_binary_starter_root_runs_the_public_judge() {
        let package_dir = tempfile::tempdir().unwrap();
        write_binary_instructor_package(package_dir.path());
        let out_dir = tempfile::tempdir().unwrap();
        publish(package_dir.path(), out_dir.path()).unwrap();

        let test = std::process::Command::new("cargo")
            .arg("test")
            .current_dir(out_dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&test.stdout);
        assert!(
            test.status.success(),
            "cargo test at the binary starter root failed: {}{}",
            stdout,
            String::from_utf8_lossy(&test.stderr)
        );
        assert!(stdout.contains("counts_words"));
    }
}
