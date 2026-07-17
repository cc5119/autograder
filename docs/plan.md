# Autograder — Implementation Plan

Status: M1 and M2 done
Last updated: 2026-07-17
Companion to: [`docs/design.md`](./design.md)

This plan turns the design into an ordered sequence of implementation steps.
It follows the design's phased milestones (§19, M1–M6) but breaks each into
concrete, individually-shippable steps.

## Ground rules

1. **The project compiles after every step.** `cargo build` and `cargo test`
   succeed at each step boundary. New surface area is introduced trait-first:
   define the trait + data types + a stub impl (a stub may `todo!()` / return a
   structured "not implemented" error at *runtime* while still *compiling*), then
   fill the body in a later step. We never leave a half-written type that breaks
   the build.
2. **Behavior lands behind the trait it belongs to** so the pipeline can be wired
   end-to-end early (with stubs) and each real impl swaps in without touching the
   orchestrator.
3. **Verification honesty.** This machine has Rust 1.94.1 + git, but **no
   podman, no docker, no cargo-nextest**. Steps that actually *execute*
   containers or nextest are built so their command construction and output
   parsing are unit-testable in isolation; their live/integration verification is
   explicitly marked **[deferred: needs podman]** / **[deferred: needs nextest]**
   and run later on a provisioned host. Nothing in the build depends on those
   tools being present.
4. **Host toolchain vs. assignment toolchain.** The `[toolchain]` channel in a
   spec (e.g. `1.86.0`) is the toolchain used *inside the sandbox base image* to
   build student code. It is independent of the toolchain used to build the
   `autograder` binary itself (1.94.1 here). Don't conflate them.

## Target module layout

Single crate, split into a library (`src/lib.rs`) + a thin binary (`src/main.rs`)
so logic is unit-testable and the CLI stays a shell.

```
src/
  main.rs         # thin: init tracing, parse CLI, dispatch to lib
  lib.rs          # module declarations + top-level re-exports
  cli.rs          # clap command/argument definitions
  config.rs       # host-wide config (credentials location, storage dir, limits, runtime)
  error.rs        # crate error type(s) (thiserror) + Result alias
  model.rs        # core data types: Submission<F> (generic over a Fetchable
                  #   locator type), JobContext, EvaluationResult, TestResult,
                  #   statuses, ResourceUsage, Grade, Tier, LocalPath, GitRepo
  spec.rs         # assignment spec parsing (autograder.toml / .public.toml)
  source/         # SubmissionsSource<F> trait + impls
    mod.rs        # SubmissionsSource<F> trait + Submissions (open() picks the
                  #   kind from the --submissions path: dir vs file)
    csv.rs        # CsvRoster: SubmissionsSource<GitRepo>
  fetch.rs        # Fetch stage + the Fetchable trait (each locator type knows
                  #   how to fetch itself: LocalPath now, GitRepo is a stub
                  #   until M6's GitHubFetcher fills it in). No separate
                  #   Fetcher/LocalDirFetcher indirection — see step 5 note.
  prepare.rs      # Prepare stage: workspace overlay, offline cargo env, manifest allowlist diff
  sandbox/        # Sandbox trait + SandboxSpec/Outcome
    mod.rs
    local.rs      # LocalSandbox (limits-only, runner-isolated)
    container.rs  # ContainerSandbox (rootless podman shell-out)
  evaluator/      # Evaluator trait + judge protocol
    mod.rs
    linked_library.rs
    binary_harness.rs
  vendor.rs       # offline vendoring / prefetch (cargo vendor + .cargo/config.toml)
  grade.rs        # Grader trait + scoring policies (pure)
  report/         # Reporter trait + impls
    mod.rs
    json.rs
    csv.rs
    ci.rs
  pipeline.rs     # stage orchestration + (later) async worker pool
  scaffold.rs     # starter-template generation
  store.rs        # EvaluationResult persistence ({assignment}/{student}/{run_id})
```

Modules are added as their milestone arrives; the layout above is the
destination, not something created all at once in step 1.

---

## M1 — Skeleton (trusted wiring, no sandbox) — **Done**

Goal: a working `grade` pipeline over the **mock filesystem fetcher** and a
**stub evaluator**, proving SubmissionsSource → Fetch → Prepare → Evaluate →
Grade → Report + result persistence. No untrusted-code isolation yet.

### Step 1 — lib/bin split, dependencies, CLI skeleton
- Convert to `lib.rs` + thin `main.rs`. Add `error.rs` (thiserror error enum +
  `Result` alias) and `config.rs` (deserializable host config, defaults).
- Add deps: `clap` (derive), `serde` (derive), `serde_json`, `thiserror`,
  `anyhow`, `tracing`, `tracing-subscriber`.
- Define all v1 subcommands in `cli.rs` per design §16: `prefetch`, `grade`,
  `ci`, `regrade`, `report`, `scaffold` (with their flags: `--submissions`,
  `--jobs`, `--as-of`, `--harness`, `--format`, `--out`). Each dispatches to a
  handler that returns a "not implemented" error for now.
  - `grade` takes a single `--submissions <path>` rather than separate
    `--roster`/`--submissions` flags (revised post-M1): the kind is inferred
    from the path shape (directory vs file) instead of a second flag — see
    step 4/5.
- **Compiles because:** handlers are real functions returning `Err(...)`.
- **Verify:** `cargo run -- --help`, `cargo run -- grade --help`.

### Step 2 — Core model types
- `model.rs`: `Submission`, `JobContext`, `Tier` (`Authoritative`/`Ci`),
  `StageStatus`, `TestVisibility` (`Public`/`Private`), `TestStatus`
  (`Pass`/`Fail`/`Timeout`/`Oom`/`Error`/…), `TestResult`, `StageReport`,
  `ResourceUsage`, `Diagnostics`, `EvaluationResult`, `Grade`. All `serde`
  (de)serializable, matching the JSON in design §12.
  - Revised in step 5: `Submission` became `Submission<F>`, generic over a
    `Fetchable` locator type (`LocalPath`, `GitRepo`), so a submission can
    only be fetched through the `Fetchable` impl matching how it was
    produced — see step 5 note.
- **Compiles because:** pure data + derives.
- **Verify:** a round-trip unit test serializing the §12 example `EvaluationResult`
  to JSON and back.

### Step 3 — Assignment spec parsing
- Add `toml` and `chrono` (serde feature) deps.
- `spec.rs`: serde structs for `[assignment]`, `[student]`, `[toolchain]`,
  `[allowed-crates]` (map), `[limits.build]` / `[limits.run]` (durations parsed
  from strings like `"120s"`, `"5s"`; sizes like `"2GiB"`, `"1MiB"`), `[scoring]`
  + `[[scoring.tests]]` (name, visibility, **optional** `points`). `deadline` as
  an RFC3339 `chrono::DateTime`.
- A `Spec::load(dir)` that reads `autograder.toml` (private/full) or
  `autograder.public.toml` (public subset). Points-less tests compute no score.
- **Verify:** unit tests parsing both example specs from design §5.3; assert the
  public spec exposes no points.

### Step 4 — SubmissionsSource trait + CsvRoster
- Add `csv` dep.
- `source/mod.rs`: `SubmissionsSource<F>` trait
  (`fn submissions(&self) -> Result<Vec<Submission<F>>>`), generic over the
  locator type `F` its submissions carry.
  `source/csv.rs`: `CsvRoster` (`SubmissionsSource<GitRepo>`) reading the
  design §6 columns (`student_id,repo_url,ref,email,section`), carrying extra
  columns into `metadata`. `repo_url`/`ref` become the `GitRepo` locator.
- **Verify:** unit test parsing the sample roster; extra columns land in metadata.

### Step 5 — Fetch stage: mock filesystem fetcher, and a type-safe fetch seam
- `fetch.rs`: instead of a separate swappable `Fetcher` object, each locator
  type implements a `Fetchable` trait (`fn fetch(&self, dest: &Path) ->
  Result<FetchOutcome>`) directly — revised from the original "`Fetcher`
  seam" plan once it became clear each locator has exactly one way to
  resolve itself, so a separate strategy object per locator was pure
  indirection. `impl Fetchable for LocalPath`: given a local directory path,
  materialize a clean checkout by copying that directory into a per-job
  workspace. Records a synthetic `graded_commit` (e.g. a content hash of the
  tree) and a `FetchOutcome`. `impl Fetchable for GitRepo` is a
  `NotImplemented` stub until M6's `GitHubFetcher` fills it in.
  `Submission<F>::fetch(&self, dest)` is sugar for `self.fetchable.fetch(dest)`.
- **Type safety:** `Submission<F>`/`SubmissionsSource<F>`/`Fetchable` are
  generic over the same `F`, so pairing e.g. a `CsvRoster`'s (`GitRepo`)
  submissions with code that only knows how to fetch a `LocalPath` is a
  *compile* error, not a silent misinterpretation of a stringly-typed
  locator. This closed a real gap: `Submission` originally carried a single
  `repo_url: String` field whose meaning (local path vs. git URL) depended
  entirely on which fetcher happened to consume it.
- Deadline handling: `--as-of` is threaded through but `LocalPath::fetch`
  does not enforce push-time (there's no server) — it just checks out the
  directory as-is. The real GitHub clone + server-side push-time selection
  (design §7.1) is a documented later step (M6, Step 25) behind
  `impl Fetchable for GitRepo`.
- Optional convenience: a `DirectorySource` impl of `SubmissionsSource<LocalPath>`
  that treats each subdirectory of a root as one student (`student_id` = dir
  name), so a full run needs only a folder of sample submissions — no CSV,
  no network.
- `source::Submissions::open(path)` resolves the single `--submissions <path>`
  CLI flag into the right kind at runtime (directory -> `DirectorySource`,
  file -> `CsvRoster`) via an enum, so `grade` needs no separate flag to pick
  the source kind; the `Csv` arm currently returns `NotImplemented` at the
  `grade` command level pending step 25's `GitHubFetcher`.
- Missing/empty directories become a structured non-fatal `fetch_failed` outcome;
  the batch continues.
- **Verify:** point `LocalPath::fetch` at a temp dir tree; assert a workspace
  is produced and a missing dir yields `fetch_failed` (not a panic).

### Step 6 — Prepare stage (overlay only, no offline env yet)
- `prepare.rs`: assemble the workspace — student checkout + instructor
  `harness/` + `fixtures/` **overlaid on top** (instructor files win; matching
  student paths removed; student test files ignored), per design §7.2. Wire
  student code to harness per `kind` at a structural level (create the driver
  crate scaffold for `linked-library`; identify the bin target for
  `binary-harness`). Offline cargo env + manifest allowlist diff come in M2.
- **Verify:** unit test that overlay precedence is correct and a student file at
  an instructor path is replaced.

### Step 7 — Grade stage (pure) + scoring policies
- `grade.rs`: `Grader` trait (`grade(&EvaluationResult, &ScoringPolicy) -> Grade`)
  + `ScoringPolicy` (from spec `[scoring]`). Implement `weighted`, `pass-count`,
  `pass-fail`. Terminal failure states (`build_failed`, `harness_error`,
  `timeout`, `oom`, …) map to **zero**, never better than a normal `fail`
  (design §7.4). Carries `student_id`.
- **Verify:** unit tests incl. the adversarial case — a `harness_error` scores no
  higher than a plain `fail`.

### Step 8 — Reporters: JSON + CSV
- `report/mod.rs`: `Reporter` trait (`report(&[Grade]) -> Result<()>`).
  `report/json.rs`: machine-readable per-student JSON (scores + which tests
  failed, compiler errors, timeouts). `report/csv.rs`: gradebook
  `student_id,score,max,status`.
- **Verify:** unit tests asserting output shape for a small `Grade` set.

### Step 9 — Result persistence + pipeline wiring + stub evaluator
- `store.rs`: persist/load `EvaluationResult` as JSON keyed by
  `{assignment_id}/{student_id}/{run_id}` under the config storage dir.
- `evaluator/mod.rs`: `Evaluator` trait (design §4). A `StubEvaluator` that emits
  a well-formed `EvaluationResult` (build `ok`, tests from a fixture) so the whole
  chain runs without executing student code.
- `pipeline.rs`: sequential orchestration Fetch → Prepare → Evaluate → persist →
  Grade → Report. `grade` command runs it end to end; `report` and `regrade`
  commands read persisted results.
- **Verify:** end-to-end `cargo run -- grade` against a fixture assignment dir +
  a folder of sample submissions produces a JSON report, a gradebook CSV, and
  persisted `EvaluationResult`s. **M1 done.**

**Status:** done. `grade` runs Fetch → Prepare → Evaluate (stub) → persist →
Grade → Report end to end over a `DirectorySource`; `report`/`regrade` read
persisted results. 25 unit/integration tests pass; `Submissions::Csv` still
returns `NotImplemented` pending M6's `GitHubFetcher`.

---

## M2 — Sandbox, offline vendoring, linked-library evaluator — **Done**

Goal: replace the stub evaluator with a real sandboxed `linked-library` run and
enforce the dependency allowlist offline.

**Status:** done. `grade` now builds a `LinkedLibrary` evaluator over a
`ContainerSandbox` for `linked-library` assignments (`binary-harness` still
`NotImplemented`, M4). Confirmed end-to-end short of the actual container
run: fetch → prepare (offline env + manifest diagnostics) → evaluator
correctly shells out to `podman run <exact flags>` and fails there with "No
such file or directory" since this host has neither podman nor nextest —
exactly the deferred boundary the ground rules describe. 55 unit/integration
tests pass (up from 25 at M1 close); one deviation from the design doc worth
noting: verdicts come from parsing `cargo nextest`'s JUnit report rather than
a custom judge process writing `EvaluationResult` JSON to `/out` — this
follows the plan's own step 14 wording and keeps `linked_library.rs` agnostic
to any particular assignment's op-sequence protocol (that protocol is fully
instructor-authored test code overlaid from `harness/`).

### Step 10 — Sandbox trait + LocalSandbox
- `sandbox/mod.rs`: `Sandbox` trait (`run(&SandboxSpec) -> Result<SandboxOutcome>`)
  + `SandboxSpec` (command/argv, env, mounts w/ ro|rw, network flag, resource
  limits) + `SandboxOutcome` (exit status, captured+byte-capped stdout/stderr,
  timed-out?, oom?, resource usage).
- `sandbox/local.rs`: `LocalSandbox` — runs the command as a host child process
  with a wall-clock timeout and output cap (best-effort limits; runner-isolated).
  Usable *without* podman, so it's verifiable here and is what the CI tier uses.
- **Verify:** unit/integration test running `true`/`sleep` under LocalSandbox;
  assert timeout + output-cap behavior.

### Step 11 — ContainerSandbox (rootless podman shell-out)
- `sandbox/container.rs`: build the `podman run` argv from a `SandboxSpec` per
  design §10 (`--network=none`, `--memory`/`--memory-swap`, `--cpus`,
  `--pids-limit`, `--read-only`, `--cap-drop=ALL`, `--security-opt
  no-new-privileges`, `seccomp=<profile>`, `--user`, the `/vendor` `/work/src`
  `/work/target` `/out` mounts). Detect OOM/timeout from exit + cgroup signals;
  parse resource usage.
- **Compiles/tested:** argv construction and outcome parsing are pure functions
  with unit tests. **[deferred: needs podman]** live container execution.
- **Verify (unit):** given a `SandboxSpec`, the produced argv contains the exact
  isolation flags. Live run deferred to a provisioned host.

### Step 12 — Offline vendoring + `prefetch`
- `vendor.rs` + `prefetch` command: from `[allowed-crates]`, generate a synthetic
  manifest, run `cargo vendor` to produce `vendor/` (allowed crates + transitive
  deps at pinned versions), and emit the `.cargo/config.toml` source replacement.
  Base-image pre-warming is noted but built in M4.
- **Verify:** `cargo run -- prefetch <fixture-assignment>` produces a `vendor/`
  dir + config for a tiny allowlist (uses host cargo; no podman needed).

### Step 13 — Prepare: offline env + allowlist diff diagnostics
- Extend `prepare.rs`: mount `vendor/` read-only, install `.cargo/config.toml`,
  set `CARGO_NET_OFFLINE`. Diff the student `Cargo.toml` against the allowlist and
  emit the three precise diagnostics from design §8.3 (disallowed crate / version
  outside vendored set / missing feature). Statically reject `[patch]`, git deps,
  and external path deps (§8.4).
- Parse manifests with `toml` (optionally `cargo_metadata` for resolved graphs).
- **Verify:** unit tests for each diagnostic case against crafted `Cargo.toml`s.

### Step 14 — Judge protocol + `linked-library` evaluator
- `evaluator/linked_library.rs`: assemble the trusted **driver** crate
  (path-depends on the student lib via `[student].package-name`) whose only job is
  read-op → call student API → serialize return value → write back, with **no
  assertions**. The **judge** (no student code) drives op sequences and asserts on
  serialized outputs; defaults every test to *fail*, records pass only on a
  positive judge-observed signal; crash/timeout/early-exit/wrong-output → fail.
- Run **process-per-session under `cargo nextest`**; parse nextest's machine
  output into `TestResult`s. Build first (build failure → `build_failed`), then
  run, both under the sandbox with `[limits.build]` / `[limits.run]`.
- **Compiles/tested:** driver-generation and nextest-output parsing are
  unit-testable. **[deferred: needs nextest + podman]** live judge run.
- **Verify (unit):** parse a captured nextest output sample into `TestResult`s;
  live run deferred.

### Step 15 — Results egress + swap real evaluator into `grade`
- Judge writes `EvaluationResult` to the `/out` mount; student stdout/stderr
  captured separately (byte-capped) for diagnostics only, never parsed for
  verdicts (design §10, §12). Replace `StubEvaluator` in the `grade` pipeline with
  `LinkedLibrary` over `ContainerSandbox`.
- **Verify:** end-to-end on a provisioned host **[deferred: needs podman +
  nextest]**; unit coverage for the egress/diagnostics split lands now. **M2 done.**

---

## M3 — CI tier (public, advisory)

### Step 16 — `autograder ci` entrypoint
- `ci` command: Prepare + Build + Evaluate against the **public harness**
  (`--harness .autograder/public`), **public tests only**, over `LocalSandbox`,
  using the **same out-of-process judge** as the authoritative tier. Emits an
  `EvaluationResult` with `tier: "ci"`. Enforces the offline allowlist + limits
  for parity; memory/pids best-effort (§11.3).
- **Verify:** run `ci` against a fixture public harness with LocalSandbox (no
  podman needed) — real end-to-end here since LocalSandbox works locally.

### Step 17 — CiReporter
- `report/ci.rs`: per-test pass/fail + diagnostics (compiler errors, timeouts,
  disallowed-crate), overall `N of M public tests failing`, **no scores** (the
  public spec carries no points, §5.3, §11.4). Process exit code reflects public
  pass/fail.
- **Verify:** snapshot the §11.4 sample output; assert non-zero exit on failures.

### Step 18 — `scaffold` command + GitHub Actions wrapper
- `scaffold.rs` + `scaffold` command: emit the starter template (design §5.1) —
  `.autograder/public/` (vendored public harness + public spec + fixtures),
  `.github/workflows/autograde.yml` (thin wrapper that downloads a version-pinned
  binary, `sha256sum -c`, runs `autograder ci`), and a student `Cargo.toml`
  constrained to the allowlist.
- **Verify:** `cargo run -- scaffold <fixture> --out starter-hw3/` produces the
  documented tree; the emitted workflow matches design §11.2.

### Step 19 — Release/download pipeline (infra + docs)
- Add a GitHub Actions release workflow (repo infra, not crate code) that
  cross-compiles the pinned `autograder` binary and publishes it with a recorded
  sha256 for the CI download step. Document the version-pinning/upgrade story
  (open question §18.5).
- **Compiles because:** this is YAML/docs; no Rust change. **M3 done.**

---

## M4 — Parallelism, caching, binary-harness

### Step 20 — Async worker pool
- Add `tokio`. Make `pipeline.rs` an async worker pool grading up to *N* students
  concurrently, *N* derived from a host memory + disk budget (Σ per-job memory +
  target-volume quota), not an arbitrary count (design §13). A semaphore bounds
  concurrent sandboxes. Per-student failure isolation — one failure never aborts
  the batch.
- **Verify:** run a batch of many fixture submissions; assert bounded concurrency
  and that an injected per-student failure doesn't abort peers.

### Step 21 — Caching: base image + bare clones + quota'd target volumes
- Build a per-assignment base image (toolchain + pre-warmed vendored deps).
  Cache bare clones on the host (`git fetch` to update — used by the M6 GitHub
  fetcher). Give each job a **fresh, disk-backed, size-quota'd** target volume
  (not tmpfs, not shared) per design §10/§13.
- **Compiles/tested:** volume/quota provisioning + image-build command
  construction unit-tested; **[deferred: needs podman]** live image build.

### Step 22 — `binary-harness` evaluator
- `evaluator/binary_harness.rs`: the trusted judge spawns the built student
  **binary** (target from `[student].bin-name`) as a child — protocol / timing /
  stdin / args — and asserts on stdout/files/observable behavior, each interaction
  → a named `TestResult`. Exit code is only one input to the verdict, never "pass"
  alone. Always under `[limits.run]`.
- Funnels into the same `EvaluationResult` shape as `linked-library`.
- **Verify (unit):** protocol driver + verdict logic against a fake child;
  live sandboxed run **[deferred: needs podman]**. **M4 done.**

---

## M5 — Grading / regrading

### Step 23 — Decoupled `regrade` from persisted results
- `regrade` command: re-run **only** the Grade stage from persisted
  `EvaluationResult`s (no student code), applying the current `[scoring]` policy.
  Confirm the three scoring models end-to-end over persisted fixtures.
- **Verify:** persist a set of results, change weights, `regrade`, assert scores
  update without re-evaluation.

### Step 24 — Manual overrides + late penalties
- Grade-stage override file (per-student manual score/status overrides) and
  late-penalty policy (open question §18.2), applied at grading time and
  **recorded**, never by mutating raw `EvaluationResult`s (design §14).
- **Verify:** unit tests that an override and a late penalty change the `Grade`
  but leave the persisted raw result untouched. **M5 done.**

---

## M6 — Real GitHub fetch + extensibility polish

### Step 25 — GitHubFetcher + server-side push-time deadline
- Fill in the `impl Fetchable for GitRepo` stub (in `fetch.rs` since step 5):
  clone/`git fetch` (shell out to `git`, or `git2`) using host-side instructor
  credentials, then resolve the graded commit via **GitHub's server-side push
  time** — newest commit on the target branch whose *push* was `<= deadline`
  (design §7.1) — via the GitHub API (`reqwest`/`octocrab`, or `gh`). Never
  trust committer/author dates. Record resolved SHA + push time. `--as-of`
  overrides the deadline for dry runs; a CSV `ref` pinning a SHA is still
  push-time checked.
- `LocalPath::fetch` stays as the offline/dev path; selection is already
  automatic via `source::Submissions::open` (directory vs file), so no new
  flag or config is needed — the `grade` command's `Submissions::Csv` arm
  (currently `NotImplemented`) starts working once this lands.
- **Verify:** integration test against a scratch repo **[deferred: needs GitHub
  credentials + network]**; push-time selection logic unit-tested with mocked API
  responses.

### Step 26 — Extensibility seams + polish
- Add a stub `SubmissionsSource` (e.g. `GitHubClassroom`) and document the `Sandbox`
  Firecracker upgrade path and additional CI-platform wrappers (GitLab) around the
  existing `autograder ci` entrypoint. Note the service-mode groundwork (queue /
  workers / storage / `EvaluationResult` persistence already daemon-shaped, §15).
  Address remaining open questions (§18) as tracked follow-ups.
- **Verify:** traits compile with the new stub impls; docs updated. **M6 done.**

---

## Cross-cutting, carried throughout

- **Errors:** one crate error type (`thiserror`) with `anyhow` at the CLI edge;
  stage failures become structured non-fatal outcomes, never batch-aborting panics.
- **Tracing:** `tracing` spans per student/stage from step 1; `--jobs` and batch
  progress observable.
- **Config:** `config.rs` (credentials location, storage dir, default limits,
  container-runtime choice) grows as milestones need it; runtime choice stays
  behind the `Sandbox` trait (open question §18.8).
- **Reproducibility:** from step 9 every persisted run records graded SHA (+ push
  time once M6 lands), instructor-package SHA, public-harness SHA, toolchain, the
  vendored `Cargo.lock`, and the limit profile (design §14).

## Open questions that gate specific steps

These are the design's §18 items, mapped to where a decision is needed:
- Scoring-model default (§18.1) → Step 7 (default to `weighted`, others available).
- Late-submission policy (§18.2) → Step 24.
- Per-test vs run-wide CPU budgets (§18.3) → Steps 11/14.
- CI memory/pids enforcement expectations (§18.4) → Step 16.
- Release targets + version pinning (§18.5) → Step 19.
- Public/private spec drift validation (§18.6) → optional validator, fits after
  Step 3 or as an M6 polish item.
- Instructor credentials (token/SSH/App), also load-bearing for the push-time API
  (§18.7) → Step 25.
- Container runtime commitment (§18.8) → Step 11 (Podman first, trait-swappable).
