# Autograder — Design Document

Status: Draft
Last updated: 2026-07-17
Author: Nico Lehmann

## 1. Overview

The autograder clones students' private Rust repositories, builds and runs an
instructor-provided test suite against each submission inside a resource-limited
sandbox, and produces per-student grades. It is designed to run **untrusted
student code on a single Linux machine**, safely.

It also provides a **student-facing CI tier**: a limited, advisory run that
executes only *public* tests inside the student's own CI, giving fast feedback
before the authoritative instructor-run grading.

Primary use case: private repos hosted on GitHub. The design keeps the
submission *source* behind an abstraction so other sources can be added later.

### The two tiers

| | **Authoritative tier** (instructor) | **CI tier** (student-facing) |
| --- | --- | --- |
| Where it runs | Instructor's single Linux machine | Student's own CI runner |
| Tests | Public **+ hidden** | **Public only** |
| Trust | Source of truth | **Advisory only** — never trusted, re-computed authoritatively |
| Isolation | Full sandbox (containers, offline, limits) | Relies on the CI runner for host isolation; enforces the same resource limits for parity (not the offline dependency allowlist) |
| Entry point | `autograder grade …` | `autograder ci` (platform-agnostic) |
| Purpose | Grading | Fast student feedback |

### Design goals

- **Safe execution of untrusted code** — hard limits on CPU time, wall-clock
  time, and memory; no network access during grading; filesystem isolation.
- **Multiple assignment types** — starting with (a) a library with a predefined
  public API linked against an instructor test harness, and (b) a binary driven
  by an instructor test harness. The set of types must be extensible.
- **Declarative assignments** — an assignment is a self-contained, versioned
  package (its spec + harness + tests) that lives in a private instructor repo.
- **Restricted dependencies** — an assignment declares exactly which crates
  students may use; anything else fails to compile.
- **Reproducibility** — grading is deterministic and records the exact commit
  graded; re-grading does not require re-running student code.
- **Fast student feedback** — the same grader, in a reduced `ci` mode, gives
  students advisory pass/fail feedback on public tests in their own CI.
- **Extensibility** — submission sources, assignment types, CI platforms, and
  (later) a long-running service mode can be added without reworking the core.

### Non-goals (initial version)

- Not a long-running web service (but the core is structured so one can be added
  — see §15). The v1 deliverable is a CLI batch tool.
- Not a plagiarism detector.
- Not a general CI system; it is specialized to Rust assignments.
- Not multi-machine/distributed. Single machine only for v1.
- The CI tier is **not** a grade of record; it is advisory feedback only.

## 2. Threat model & security assumptions

Students are assumed to be **potentially adversarial**. Every artifact from a
student repo — source, `Cargo.toml`, `Cargo.lock`, `build.rs`, proc-macros,
committed binaries — is untrusted.

Key risks and mitigations (authoritative tier):

| Risk | Mitigation |
| --- | --- |
| Arbitrary code at **build time** (`build.rs`, proc-macros run during `cargo build`) | The build runs *inside the same sandbox* as the tests, with the same resource limits. Build is never run on the host. |
| Resource exhaustion (fork bombs, infinite loops, OOM, disk fill) | cgroup limits: `--memory`, `--cpus`, `--pids-limit`; a scored CPU-time bound plus a wall-clock safety-net timeout for build and run (§10); bounded output capture; size-quota'd disk-backed writable volume. |
| Network exfiltration / calling home / dependency confusion | Grading containers run with `--network=none`. All dependencies are vendored offline (§8). |
| Reading/exfiltrating the hidden test suite | Grading is offline, so even though tests are present on the container FS during evaluation, results can only leave via the controlled, captured output channel. The **CI tier never receives hidden tests at all** (§11). |
| Escaping the sandbox to the host | Containers run unprivileged (rootless Podman recommended), read-only root FS, dropped capabilities, `no-new-privileges`, seccomp default profile. |
| Tampering with the grader / instructor tests | Instructor harness + tests come from a trusted private repo, materialized fresh per job and kept out of the (untrusted) student checkout entirely for `library` (§7.2, §9.1); student-provided test files are ignored. |
| **Forging the verdict at runtime** — in an in-process test run, student code shares an address space with the assertions and can `exit(0)` (from a `Drop`, thread, or ctor), swallow assertion panics, or print fake result JSON | The pass/fail verdict is **always computed by a trusted judge process that contains no student code** (§9). Student code is driven across a process boundary and graded only on its observable outputs; the judge defaults every test to *fail* and records a pass only on a positive, judge-observed signal. Enforced in **both tiers**. |

**CI tier trust:** students fully control their CI environment and could fake
results, so CI output is **advisory only**. The authoritative tier re-runs *all*
tests (public + hidden) from scratch and never consumes student CI output. The
CI tier does not need the host-isolation guarantees above (it runs the student's
own code on the student's own runner); it relies on the CI provider for host
isolation and applies allowlist + resource limits only for parity/feedback.

Residual risk: a container escape via a kernel/runtime 0-day. Accepted for a
single-instructor machine; Firecracker microVMs are the upgrade path if the
threat model tightens (§15).

## 3. High-level architecture

Two inputs feed a per-student pipeline:

1. **Submission source** — for v1, a CSV roster mapping each student to a repo
   URL (+ metadata). Behind a `Source` trait.
2. **Assignment package** — a private instructor repo containing the full spec,
   the harness, hidden tests, and the allowed-crate list, plus a pinned
   reference to the **public harness** shipped to students.

```
 Source (CSV: id, repo_url, ...)          Instructor package (private repo)
        │                                   autograder.toml · harness/ · hidden tests
        ▼                                            │
   ┌─────────┐   ┌──────────┐   ┌────────────────────────┐   ┌──────────┐   ┌────────┐
   │  Fetch  │──▶│ Prepare  │──▶│   Build + Evaluate      │──▶│  Grade   │──▶│ Report │
   │ clone   │   │ overlay  │   │ Docker/Podman, offline, │   │ score    │   │ JSON / │
   │ @commit │   │ harness  │   │ vendored, limited       │   │ from raw │   │  CSV   │
   └─────────┘   └──────────┘   └────────────────────────┘   └──────────┘   └────────┘
        └────────────────── run once per student, in turn ──────────────┘
                                     │                              ▲
                          raw evaluation results (JSON) ───────────┘

 CI tier (in each student repo, advisory):
   push to default branch ─▶ wrapper workflow ─▶ `autograder ci`
      = Prepare(public harness) + Build+Evaluate(public tests, allowlist, limits)
      = same core, public tests only, no hidden tests, results shown in CI logs
```

The **evaluation** and **grading** stages are deliberately decoupled.
Evaluation runs untrusted code and emits a structured, persisted result
artifact. Grading is a pure function from that artifact (plus the spec's scoring
policy) to a score — so weights, late penalties, and manual overrides can change
and be re-applied without re-running any student code. The **CI tier reuses the
Prepare + Build + Evaluate stages** of the same core, restricted to the public
harness.

## 4. Core abstractions

The core is a set of traits so pieces can be swapped/extended.

```rust
/// Where submissions come from. v1 impl: CsvRoster. Future: GitHubClassroom, etc.
trait Source {
    fn submissions(&self) -> Result<Vec<Submission>>;
}

struct Submission {
    student_id: String,
    repo_url: String,          // or other locator
    metadata: BTreeMap<String, String>,
    r#ref: Option<String>,     // optional pinned ref/branch override
}

/// Turns a prepared workspace into a raw evaluation result. It launches a
/// **trusted judge process that contains no student code** and drives the
/// untrusted student code across a process boundary (§9), grading only the
/// student's observable outputs. v1 impls: Library, Binary. Same
/// impls serve both tiers; the tier only changes which tests/harness are present.
trait Evaluator {
    fn evaluate(&self, ctx: &JobContext, sandbox: &dyn Sandbox) -> Result<EvaluationResult>;
}

/// Runs a command under resource limits and isolation. v1 impl: ContainerSandbox
/// (Docker/Podman). CI tier uses a LocalSandbox (limits only, runner-isolated).
trait Sandbox {
    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome>;
}

/// Pure scoring: EvaluationResult + policy -> per-student Grade (carries the
/// student_id). No untrusted code runs here.
trait Grader {
    fn grade(&self, eval: &EvaluationResult, policy: &ScoringPolicy) -> Grade;
}

/// Emits reports from grades. v1 impls: JsonReporter, CsvReporter, CiReporter.
trait Reporter {
    fn report(&self, grades: &[Grade]) -> Result<()>;
}
```

`Fetch` and `Prepare` are ordinary pipeline stages, not traits: only the parts
that vary — submission sources, assignment types, sandboxes, scoring, and
report outputs — are abstracted behind the traits above.

## 5. Assignment package format

There is only **one** assignment repo: the **private instructor package**. It
is self-contained: it defines its **own complete authoritative harness** (the
public-equivalent tests *and* the hidden tests) and its own full spec.

There is no separate, hand-maintained "public repo" to keep in sync. Instead,
`autograder scaffold` copies everything real out of the private package and
strips the sensitive parts in place (`src/publish.rs`, `src/stub.rs`), rather
than reassembling each output from spec fields in code: the starter's whole
`Cargo.toml`/`src/` is a straight recursive copy of the reference solution
directory, with every `.rs` file then stripped in place (pub signatures
survive, bodies become `todo!()`, private items and `#[cfg(test)]` modules
dropped) and `cargo fix` run once directly in the output to prune imports the
stripping left unused. The public spec (`points`/non-public
`[[scoring.tests]]` entries stripped) and public harness (only the
public-visibility tests, matched by name against the spec) are produced the
same way: copy, then rewrite the couple of sensitive spots in place. Because
these are derived fresh from the private package on every `scaffold` run
rather than hand-copied once, points/hidden-tests/API drift between what the
instructor authored and what ships to students is structurally impossible
for the parts `scaffold` derives (open question §18.6 revisited).

There is no `[student]` spec section either: `[assignment].id` is the single
identifier for everything student-facing — the crate name the harness
depends on (§9.1), the binary target name for `binary`-kind assignments,
and the directory name the reference solution must live in
(`<instructor-repo>/<id>/`). That solution directory is **required** —
`scaffold` has nothing to copy the starter's `Cargo.toml`/`src/` from without
it, so a missing directory is a clear error, not a placeholder. `scaffold`
also validates that the directory it finds is actually the right one: if its
own `Cargo.toml` `[package].name` doesn't match `id`, that's treated as a
misconfigured assignment and `scaffold` refuses to guess.

The one remaining thing this derivation depends on by convention rather than
validation: a judge `#[test]` fn's name must match its `[[scoring.tests]]`
entry's `name` in `autograder.toml` for `keep_only_named_tests` to keep it in
the public harness.

### 5.1 Private instructor package

```
hw3-instructor/                 (the only repo — self-contained)
  autograder.toml               # full standalone spec: all tests + points; id = "hw3"
  harness/                      # complete authoritative harness (public-equivalent + hidden)
  hw3/                          # reference solution, named after [assignment].id (required --
                                 # scaffold copies this directory to produce the starter)
  fixtures/                     # all inputs / expected outputs
```

`scaffold <hw3-instructor> --out starter-hw3/` produces the starter/template
repo (what a student clones/forks):

```
starter-hw3/                    (what the student works in)
  src/                           # copy of hw3/'s src/, stripped: solution's API shape, todo!() bodies
  .github/workflows/autograde.yml   # thin wrapper -> `autograder ci`
  .autograder/public/            # derived public spec + harness + fixtures
  Cargo.toml                     # copy of hw3/'s own Cargo.toml
```

### 5.2 Spec schema

`autograder.public.toml` (shipped) holds only what students may see — public
test *names* and visibility, but **no point values**. `autograder.toml` (private)
is a standalone full spec that defines every test and its points. The autograder
reads whatever spec it is given and computes no score for a test that carries no
`points`, so the shipped spec never reveals weighting.

`id` also doubles as the student-facing crate name — the expected Cargo
package name (`library`) or binary target name (`binary`).
No separate `[student]` section: one identifier, nothing to keep in sync
with it by hand.

```toml
# --- autograder.public.toml (public) ---
[assignment]
id = "hw3"                       # also the expected package/binary name
name = "Binary search tree"
kind = "library"          # or "binary"; extensible
deadline = "2026-02-14T23:59:59-08:00"

[toolchain]
channel = "1.86.0"

# Dependency allowlist — enforced in BOTH tiers via offline vendoring.
[allowed-crates]
serde = "1"
rand  = "0.8"

[limits.build]
wall-clock = "120s"
cpus = 2
memory = "2GiB"
pids = 256

[limits.run]
cpu-time = "5s"                  # scored bound: load-independent pass/fail
wall-clock = "10s"              # safety net only: kills deadlock/spin
cpus = 1
memory = "512MiB"
pids = 128
max-output-bytes = "1MiB"

[scoring]
model = "weighted"               # "weighted" | "pass-count" | "pass-fail"
[[scoring.tests]]
name = "insert_basic"
visibility = "public"            # public tests run in the CI tier too
# no `points`: shipped spec never reveals weighting; CI computes no score

# --- autograder.toml (private) — standalone full spec, no `extends` ---
[[scoring.tests]]
name = "insert_basic"            # same public test, points assigned privately
points = 10
visibility = "public"
[[scoring.tests]]
name = "balance_adversarial"
points = 20
visibility = "private"           # hidden; only in the authoritative tier
```

Rationale: one atomic, versioned unit per repo. `git` history is the change log;
a graded run records the student SHA and the instructor-package SHA for full
reproducibility (plus, for the CI tier, the public-harness SHA shipped to
students).

## 6. Submission source — CSV roster (v1)

The `Source` trait is agnostic; v1 ships a CSV implementation.

```csv
student_id,repo_url,ref,email,section
alice,https://github.com/alice/cse130-hw3.git,,alice@x.edu,A
bob,https://github.com/bob/cse130-hw3.git,main,bob@x.edu,B
```

- `repo_url` — clone URL (private; auth via a token/SSH configured on the host,
  outside the sandbox — cloning happens on the host, not in the container).
- `ref` — optional branch/ref override; otherwise the default branch is used and
  the commit is chosen by deadline policy (§7).
- Extra columns are carried as `metadata` and available to reporters.

Future sources (GitHub Classroom, GitHub App org tokens, a web portal) implement
the same trait.

## 7. Pipeline stages (authoritative tier)

### 7.1 Fetch

- Clone (or update a cached clone of) the student repo **on the host** using the
  instructor's configured GitHub credentials.
- **Commit selection policy: latest commit pushed before the deadline.** Git
  commit metadata (committer/author date) is set by the student and is trivially
  forgeable (`GIT_COMMITTER_DATE`), so it **must not** be trusted for deadline
  enforcement. Instead resolve the graded commit from GitHub's **server-side push
  time**: query the repo's push/branch events via the GitHub API and choose the
  newest commit on the target branch whose *push* to GitHub happened `<= deadline`.
  This adds a GitHub API dependency (see credentials, §18). Configurable per
  assignment; overridable per student via CSV `ref` (a `ref` pinning a specific
  SHA is still subject to the push-time check).
- Record the resolved **commit SHA** and its **push time** in the job record for
  reproducibility.
- Failures (repo missing, no access, empty repo, no commit before deadline) are
  captured as a structured non-fatal outcome for that student — the batch
  continues.

### 7.2 Prepare

- Materialize a clean student checkout at the chosen SHA. For `library`,
  the instructor harness/judge is copied into a **separate, fresh, per-job
  directory** rather than onto the student's own checkout (§9.1) — nothing an
  evaluator builds ever lands inside the student's tree. `binary`
  (unimplemented) copies the harness directly onto the student's checkout,
  since it needs to wire against the student's own build output there.
- Assemble the offline Cargo environment: mount the prevendored crate directory
  and write the `.cargo/config.toml` source replacement (§8) — `library`
  additionally passes the equivalent override as `--config` flags directly, see
  §9.1.
- Wire the student code to the harness according to `kind` (§9).
- Nothing untrusted is executed in this stage.

### 7.3 Build + Evaluate (sandboxed)

- Run inside a container (§10) with `--network=none` and the `[limits.build]` /
  `[limits.run]` limits.
- Build first (build failure ⇒ recorded as such, contributes zero points but is
  not a crash). Then run the harness/tests, capturing structured results.
- Emit an `EvaluationResult` (§12), persisted to disk keyed by
  `{assignment_id}/{student_id}/{run_id}`.

### 7.4 Grade

- Pure, offline, re-runnable. Reads the persisted `EvaluationResult` + the spec's
  `[scoring]` policy → `Grade`. Supports later re-grading, manual overrides, and
  late penalties without re-running student code.
- Terminal failure states map to **zero**, never to skipped: `build_failed` and
  `harness_error` score no worse and **no better** than an ordinary `fail`. This
  matters because student code can deliberately induce a `harness_error` (e.g. by
  crashing the driver) to dodge a failing test, so it must never be scored more
  leniently than the failure it is masking.

### 7.5 Report

- `Reporter` impls turn grades into outputs. v1: a machine-readable JSON report
  and a gradebook CSV (`student_id,score,max,status`). Per-student feedback
  (which tests failed, compiler errors, timeouts) included in JSON.

## 8. Dependency restriction & offline vendoring

This is both the **library allowlist** enforcement and the offline mechanism,
used in **both tiers**.

1. **Prefetch (online, trusted, one-time per assignment):** from
   `[allowed-crates]`, generate a synthetic manifest and run `cargo vendor` to
   produce a `vendor/` directory containing exactly those crates plus their
   transitive dependencies, at pinned versions. Runs on the host / in the public
   harness build, never on student code.
2. **Offline grading:** mount `vendor/` read-only and install a
   `.cargo/config.toml` that replaces the crates.io source with the vendored
   directory. With `--network=none` + `CARGO_NET_OFFLINE=true`, cargo can resolve
   **only** the vendored crates.
3. **Enforcement:** if a student's `Cargo.toml` requires anything not in the
   vendored set, the offline build fails. A raw offline-build failure is opaque,
   so during Prepare we diff the student manifest against the allowlist and emit a
   precise diagnostic distinguishing the cases:
   - `⚠ disallowed crate: tokio` — the crate is not in the allowlist at all.
   - `⚠ crate foo requires version =1.2.3, but only 1.4.0 is vendored` — an
     allowed crate pinned to a version outside the vendored/resolved set.
   - `⚠ crate serde needs feature "derive", not enabled in the vendored build` —
     an allowed crate/version, but a feature the prevendor step didn't include.
4. **Optional defense-in-depth:** statically reject `[patch]`, git deps, and
   external path deps in the student `Cargo.toml` during Prepare for clearer
   errors.

The CI tier does **not** carry `vendor/`/`.cargo/` into the starter template —
offline vendoring is grading-only infrastructure. A student's own `autograder
ci` run resolves dependencies normally (real network access on their CI
runner), so it isn't offline or allowlist-enforced the way the authoritative
tier is; it's advisory feedback on the tests, not a dependency-restriction
check (§11.3).

## 9. Assignment types (v1)

Both implement `Evaluator`. The set is extensible via new impls. The same impls
serve **both tiers** — only the set of tests/harness differs.

**Core invariant (both types, both tiers): the verdict is never computed in, and
never travels through, a process that contains student code.** A trusted *judge*
process — instructor code only — drives the untrusted student code across a
process boundary and decides pass/fail purely from the student's **observable
outputs**. This is what makes forging impossible: to make a judge-chosen input
produce the correct output, the student has to actually be correct; and the
in-process tricks (`exit(0)`, panic-swallowing, forged result JSON) can only
corrupt the student's own process, which the judge scores as a *fail*. The judge
**defaults every test to fail** and records a pass only on a positive,
judge-observed signal (timeout, crash, early exit, wrong/no output → fail).

### 9.1 `library`

- The student repo is a **library crate** exposing a predefined public API.
- Instructor ships a **thin, trusted driver binary**, checked in permanently at
  `harness/` (no `driver/` subdirectory — the harness *is* the driver crate).
  Its `Cargo.toml` declares a wildcard dependency on the student package
  (`<id> = "*"`, `[assignment].id` doubling as the package name) plus a
  checked-in `[patch.crates-io]` pointing at a reference solution kept
  alongside it in a directory named after that same `id` (`../<id>`), so
  the harness is independently testable — `cd harness && cargo nextest
  run`, no autograder involvement. At grade/CI time, the
  autograder overrides that default via a per-invocation `--config
  'patch.crates-io.<pkg>.path="<checkout>"'` flag pointing at whichever
  checkout is actually being evaluated; a config-sourced `[patch]` entry
  takes precedence over one declared in the manifest, so this always wins.
  `"*"` as the version requirement means the patch applies regardless of
  whatever version the student's own `Cargo.toml` happens to declare —
  nothing student-controlled can affect whether it takes effect. The driver's
  only job: read an operation from the judge, call the corresponding student
  API, serialize the *return value*, write it back. It contains **no
  assertions**.
- Each job builds against a *fresh, per-job* copy of `harness/`, overlaid
  during Prepare into a directory that is a **sibling** of the student's own
  freshly-fetched workspace, never nested inside it and never built in
  place: this way nothing an evaluator writes (build output, `Cargo.lock`)
  ever lands inside the student's own checkout tree. Freshness is still
  required despite the two being separate: Cargo rewrites `Cargo.lock`
  whenever the patched-in package's version differs from what's currently
  locked, so sharing one `harness/` directory across jobs would race
  different students' builds against the same `Cargo.lock`/`target/`. Since
  the driver's build directory and the student's checkout no longer share
  an ancestor directory, the offline vendored-source override (§8.2) is
  passed to `cargo`/`cargo nextest` as `--config` flags directly rather than
  relying on Cargo's directory-based config-file discovery.
- A separate **judge process** (no student code) sends the operation sequence —
  including hidden adversarial sequences — and asserts on the serialized results.
  Even though the driver is linked with student code, corrupting it can only yield
  wrong/garbage output → fail; it cannot manufacture correct answers.
- Runs **process-per-session** under `cargo nextest` (each session its own
  process); the judge tracks which ops completed and treats a crash as a fail for
  the in-flight op, with a supervised restart for the next. `cargo nextest` is the
  structured, isolated per-test runner (see §17 — libtest's own JSON output is
  nightly-only/unstable, so this is a hard dependency, not optional).

### 9.2 `binary`

- The student repo builds a **binary** (target named `[assignment].id`).
- The trusted judge/harness spawns the built binary as a child — possibly
  multiple times, with a protocol, timing, stdin/args — and asserts on
  **stdout / files / behavior it defines**. Each interaction maps to a named
  result. The child's exit code is only ever *one input* to the verdict, never
  "pass" on its own.
- The student binary always runs under the `[limits.run]` sandbox.

Both funnel into the same `EvaluationResult` shape so grading is uniform.

**What this constrains:** hidden tests must assert on the *externally observable
behavior* of the public API, not on internals the student's own code reports.
`assert!(tree.height() <= 2*log2(n))` with a student-provided `height()` is
worthless — express such invariants as observable consequences instead (outputs
on adversarial sequences, or a judge-side operation/allocation budget the student
cannot influence).

## 10. Sandboxing (authoritative tier — Docker / Podman)

Per job, one container:

```
podman run --rm \
  --network=none \
  --memory=<limit> --memory-swap=<limit> \
  --cpus=<n> \
  --pids-limit=<n> \
  --read-only \
  --cap-drop=ALL \
  --security-opt no-new-privileges \
  --security-opt seccomp=<profile> \
  --user <nonroot> \
  -v <vendor>:/vendor:ro \
  -v <workspace>:/work/src:ro \
  -v <job-workdir>:/work/target:rw \
  -v <results-dir>:/out:rw \
  <base-image> /judge.sh
```

- **Writable workspace** (`/work/target`, the cargo target dir) is a **per-job,
  disk-backed volume with a size quota** — *not* a container `tmpfs`. tmpfs dies
  with the container (it can't carry build artifacts from the build invocation to
  the run invocation, which are separate — see below) and is RAM-backed (a Rust
  `target/` is easily 1–2 GiB). The volume is per-job and never shared across
  students (no cross-submission cache poisoning).
- **Results egress** is a dedicated small quota'd `rw` mount (`/out`). The trusted
  judge writes the `EvaluationResult` there. Student stdout/stderr is captured
  separately (byte-capped, `max-output-bytes`) for **diagnostics only** and is
  never parsed for verdicts — so a student cannot forge results or truncate them
  out of the report.
- Build and run are **separate invocations** with their own `[limits.build]` /
  `[limits.run]` profiles, sharing the same `<job-workdir>` volume so artifacts
  persist between them.
- **CPU-time is the scored bound.** Pass/fail uses a CPU-time limit so results are
  independent of host load; the **wall-clock timeout is only a runaway safety
  net** (deadlock/spin), enforced by the grader killing the container on top of
  the cgroup limits.
- **Rootless Podman recommended** over Docker; the `Sandbox` trait keeps this
  swappable.
- **Base image** carries the pinned Rust toolchain and (optionally) a
  pre-warmed build of the vendored dependencies (§13).

## 11. Student CI tier (public, advisory)

A limited run in the student's own CI that executes **only public tests**, using
the same grader core.

### 11.1 Platform-agnostic entrypoint

- A single `autograder ci` subcommand does Prepare + Build + Evaluate against the
  **public harness** vendored in the repo, then prints per-test feedback. Evaluate
  uses the **same out-of-process judge** as the authoritative tier (§9), just with
  the public tests only — so pass/fail semantics match exactly.
- Thin, per-platform **wrappers** invoke it. Reference wrapper: GitHub Actions;
  the same entrypoint works from GitLab CI etc.

### 11.2 Delivery

- The public harness + spec + fixtures + workflow are **vendored into the
  assignment's starter/template repo** students clone (`.autograder/public/` and
  `.github/workflows/autograde.yml`).
- The workflow **downloads a version-pinned prebuilt `autograder` binary** from
  releases (no compile step) and runs `autograder ci`.

```yaml
# .github/workflows/autograde.yml (thin wrapper)
name: autograde
on:
  push:
    branches: [main]        # default branch only
jobs:
  public-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          curl -fsSL https://github.com/<org>/autograder/releases/download/v1.2.0/autograder-x86_64-linux -o autograder
          echo "<pinned-sha256>  autograder" | sha256sum -c -   # verify before exec
          chmod +x autograder
      - run: ./autograder ci --harness .autograder/public
```

### 11.3 Parity & isolation

- **Parity:** enforces the same resource limits (§10, `[limits]`) and runs the
  same tests/judge as the authoritative tier (minus hidden tests), so students
  see the same pass/fail semantics early. **Not** parity on dependency
  restriction: the CI tier doesn't carry the vendored allowlist (§8) into the
  starter, so a disallowed-crate problem surfaces only at real grading time,
  not in the student's own CI run.
- **Isolation:** relies on the CI runner for host isolation (it is the student's
  own code in their own runner). Resource limits are applied by `autograder ci`
  itself: wall-clock timeouts and output caps are always enforced; memory/pids
  limits are **best-effort** depending on runner cgroup privileges.
- **Trust:** advisory only. Results are printed for the student; the
  authoritative tier ignores them and re-runs everything.

### 11.4 Feedback

Per the `CiReporter`: **per-test pass/fail plus diagnostics** (compiler errors,
timeouts, disallowed-crate errors), and the job's overall pass/fail check —
**no point scores**. This isn't just a display choice: the shipped public spec
carries no `points` (§5.2), so the CI tier structurally cannot compute scores and
weighting is never revealed.

```
✓ balance_small
✗ insert_basic        assertion failed: expected Some(3), got None
⚠ disallowed crate: tokio (not in the allowed dependency list)
✗ delete_edge         timeout after 10s
autograde: 2 of 4 public tests failing
```

### 11.5 Trigger

On **push to the default branch** by default (`on: push: branches:[main]`).
Students can adjust in their own repo; it does not affect grading.

## 12. Evaluation result (eval ↔ grade interface)

Persisted JSON; the sole contract between the untrusted-execution side and the
scoring side. **Written by the trusted judge to the `/out` results mount (§10),
never via student-controlled stdout.** The CI tier emits the same shape (public
tests only).

```jsonc
{
  "schema_version": 1,
  "tier": "authoritative",          // or "ci"
  "assignment_id": "hw3",
  "student_id": "alice",
  "run_id": "2026-07-17T18-03-00Z-ab12",
  "graded_commit": "a1b2c3d…",
  "instructor_commit": "f9e8d7…",
  "public_harness_commit": "c0ffee…",
  "stages": {
    "fetch":   { "status": "ok" },
    "build":   { "status": "ok", "duration_ms": 8123, "warnings": 3 },
    "run":     { "status": "ok", "duration_ms": 420 }
  },
  "tests": [
    { "name": "insert_basic",  "visibility": "public",  "status": "pass", "duration_ms": 5 },
    { "name": "balance_adv",   "visibility": "private", "status": "fail", "duration_ms": 9,
      "message": "assertion failed: height <= 2*log2(n)" },
    { "name": "delete_edge",   "visibility": "public",  "status": "timeout" }
  ],
  "resource_usage": { "peak_memory_bytes": 41231872, "cpu_ms": 380 },
  "diagnostics": { "compiler_errors": null, "stderr_excerpt": "…" }
}
```

Terminal states: `ok`, `build_failed`, `timeout`, `oom`, `disallowed_dependency`,
`fetch_failed`, `harness_error`. Grading maps these to scores per policy;
`build_failed` and `harness_error` score zero, never better than a normal fail
(§7.4).

## 13. Caching & performance

- **Execution:** students are graded one at a time, in turn — a straightforward
  sequential loop over the batch, not a worker pool.
- **Failure isolation:** each student's pipeline is independent; one failure
  never aborts the batch.
- **Caching (simple for a small course, designed in):**
  - A **base image per assignment** with toolchain + pre-warmed vendored deps.
  - Cached bare clones on the host, updated via `git fetch`.
  - Per-job **fresh** writable target dir on a **disk-backed volume with a size
    quota** (§10) — not RAM-backed tmpfs, and not shared across students, to
    avoid host OOM and cross-submission cache poisoning.

## 14. Reproducibility & re-grading

- Every run records: graded student SHA and its **server-side push time** (§7.1),
  instructor-package SHA, public-harness SHA, toolchain version, vendored
  `Cargo.lock`, and the limit profile used (including the scored CPU-time bound).
- Raw `EvaluationResult`s are persisted, so grading/regrading is a fast offline
  re-computation. Manual overrides and late-penalty policies are applied at the
  Grade stage and recorded, never by mutating raw results.

## 15. Extensibility & future work

- **New sources:** implement `Source` (GitHub Classroom, GitHub App org tokens,
  a web portal).
- **New assignment types:** implement `Evaluator` (I/O black-box binaries,
  property/fuzz testing, benchmark-scored assignments).
- **New CI platforms:** add a thin wrapper around the existing `autograder ci`
  entrypoint (GitLab CI, etc.).
- **Stronger isolation:** implement `Sandbox` with Firecracker microVMs if the
  threat model tightens.
- **Service mode:** the core (queue, workers, storage, `EvaluationResult`
  persistence) is already the shape of a daemon; an HTTP API / webhook trigger
  ("grade on push", "grade at deadline") wraps the same core.

## 16. CLI (v1)

```
autograder prefetch <assignment-repo>      # build the offline vendor dir + base image
autograder grade   <assignment-repo> --roster roster.csv [--as-of <ts>]
autograder ci      --harness <public-dir>  # student-facing: public tests, advisory
autograder regrade <assignment-id>         # re-run Grade stage from persisted results
autograder report  <assignment-id> --format {json,csv}
autograder scaffold <assignment-repo> --out starter-<id>/   # emit starter template
```

- `grade` runs Fetch→…→Report end to end.
- `ci` runs Prepare+Build+Evaluate on the public harness and prints feedback;
  exit code reflects public pass/fail.
- `scaffold` derives the starter/template repo (public spec + harness +
  starter `src/` + workflow) straight from `<assignment-repo>`, the private
  instructor package, for distribution to students (§5).
- `--as-of` overrides the deadline used for commit selection (dry runs).
- Config file for host-wide settings (credentials location, storage dir,
  default limits, container runtime choice).

## 17. Proposed crate dependencies

- `clap` (CLI), `serde` + `toml` + `serde_json` (spec/results), `tokio` (async
  workers), `anyhow`/`thiserror` (errors), `tracing` (logging).
- Git: `git2` or shell out to `git` (host-side clone/fetch).
- Containers: `bollard` (Docker API) or shell out to `podman`.
- Tests: `cargo nextest` for structured, isolated, process-per-test results.
  **Required, not optional** — libtest's own JSON output is nightly-only/unstable,
  so nextest is the structured-output dependency for the `library` judge
  (§9.1).

## 18. Open questions / decisions to revisit

1. **Scoring model default** — proposed `weighted`; confirm vs pass-count/pass-fail.
2. **Late submissions** — resolved for v1 (M5 step 24): "latest commit before
   deadline" is still the default fetch-time selection, plus an optional
   Grade-stage penalty (`[scoring.late-penalty]`: a grace period, then a
   percentage deducted per day late, capped) applied via a per-student
   `submitted_at` recorded in `overrides.toml` — see `overrides.rs`. Until
   M6's GitHub push-time API lands, that timestamp is operator-supplied
   rather than automatically resolved.
3. **Per-test limits** — the scored bound is now CPU-time with wall-clock as a
   safety net (§10); still open is per-test CPU-time budgets vs a single run-wide
   one (nextest supports per-test).
4. **CI memory/pids limits** — best-effort on hosted runners; is enforcing only
   wall-clock + allowlist in CI acceptable, or do we require self-hosted runners
   for full parity?
5. **`autograder` release artifacts** — which targets to cross-compile and pin
   for the CI download step; version-pinning/upgrade story across assignments.
   Resolved for v1 (M3 step 19): `.github/workflows/release.yml` cross-compiles
   `x86_64-unknown-linux-gnu` on a `v*` tag push and publishes
   `autograder-x86_64-linux` + its `.sha256` as GitHub release assets, named
   after the tag. `scaffold::autograde_workflow_yaml` embeds the
   `RELEASE_REPO`/`RELEASE_VERSION`/`RELEASE_SHA256_PLACEHOLDER` constants an
   instructor must edit to point at their own fork's release before
   distributing a starter template — there is no automatic propagation from a
   new release to already-scaffolded starter repos, so upgrading the pinned
   `autograder` version for an assignment already handed out means students
   re-pull the workflow file (or the instructor re-scaffolds). Additional
   targets (macOS, Windows) are a matrix-row addition to the same workflow
   when needed, not a design change.
6. **Public/private spec drift** — resolved for the parts `scaffold` mechanically
   derives (§5): the shipped `autograder.public.toml` and public harness are
   generated fresh from `autograder.toml`/`harness/` on every run, so points,
   hidden-test visibility, and API shape (via `src/stub.rs`) can't drift out of
   sync by hand-editing mistake. What's still unvalidated: a judge `#[test]` fn
   whose name doesn't match any `[[scoring.tests]]` entry silently vanishes from
   the public harness rather than erroring — worth a lint pass at some point.
7. **Instructor credentials** — token vs SSH deploy key vs GitHub App; now also
   load-bearing for the **GitHub push-time API** used in deadline enforcement
   (§7.1), not just host-side cloning.
8. **Container runtime** — commit to rootless Podman, or keep Docker first-class
   behind the `Sandbox` trait?

## 19. Suggested phased plan

1. **M1 — Skeleton:** CSV `Source`, host-side clone with deadline-based commit
   selection, spec parsing, JSON/CSV reporters. No sandbox yet (trusted wiring).
2. **M2 — Sandbox:** `ContainerSandbox` (Podman) with limits, offline vendoring,
   `library` evaluator, `EvaluationResult` persistence.
3. **M3 — CI tier:** `autograder ci` entrypoint + `LocalSandbox` + `CiReporter`,
   public-harness format, `scaffold` command, GitHub Actions wrapper, release
   pipeline for the pinned binary.
4. **M4 — Robustness:** failure isolation, base-image caching, `binary`
   evaluator.
5. **M5 — Grading/regrading:** decoupled Grade stage, scoring policies, manual
   overrides, late penalties.
6. **M6 — Extensibility polish:** additional sources / assignment types / CI
   platforms; groundwork for service mode.
