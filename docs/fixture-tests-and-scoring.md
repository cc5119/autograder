# Fixture-driven tests and value-based scoring

## Goals

- The harness/judge test files are the source of truth for which tests
  exist. `autograder.toml` should never need to re-list a test name just
  for that test to be discovered and run.
- Tests can be generated from data/fixture files (one file per case),
  coexisting with hand-written `#[test]` fns in the same harness.
- Scoring never depends on the literal name a test ends up with at
  runtime (whether hand-written or macro-generated) — a submission's grade
  is computed from what tests *report*, not from matching a fixed test
  name against a pre-declared table.
- A test's source visibility (what ships to students vs. what stays
  hidden) uses the same `keep`/`stub`/`hide` doc-comment convention
  `stub.rs` already applies to ordinary items and statements — extended to
  apply to `#[test]` fns and their bodies as well.

## Test declaration

Two ways to declare a test, both discoverable without touching
`autograder.toml`:

### A. Hand-written `#[test]` fns

An ordinary test function in the harness, annotated with the existing
`autograder:` doc-comment DSL to control what students see:

```rust
/// autograder: keep
#[test]
fn insert_basic() {
    let n = 10; // autograder: stub
    assert_eq!(tree.insert(n), Some(n));
}
```

`keep` / `stub` / `hide` control source visibility exactly as they do
today for non-test items: `keep` ships the fn body as-is, `hide` drops it
entirely from the public package, and individual statements inside the
body can carry their own `stub`/`hide` directive (e.g. blanking out a key
assertion while leaving the rest of the test visible as scaffolding). No
new DSL — this is the same mechanism, now recognized on `#[test]` fns too.

A test's presence in the public package *is* its visibility — there is no
separate visibility flag. A `hide`d test's source never reaches the
public package, so it can never run in a student's own `autograder ci`;
a `keep`/`stub`d test does run there. Nothing else currently renders a
per-test breakdown back to students beyond that.

### B. Fixture-driven tests

One file per test case under a directory convention (e.g.
`tests/fixtures/public/*.json`, `tests/fixtures/private/*.json` — the
`public`/`private` split governs which fixtures the `dir_test` glob in the
*public* harness picks up, same "presence is visibility" rule as above),
generated into real `#[test]` fns via the `dir-test` crate:

```rust
#[dir_test(dir: "$CARGO_MANIFEST_DIR/tests/fixtures/private", glob: "**/*.json", loader: load_case)]
fn judge(fixture: Fixture<Case>) {
    run_case(fixture.content());
}
```

`dir-test` expands this at compile time into one genuine, individually
nameable, individually reported `#[test] fn` per matching file — nextest
sees and runs them exactly like hand-written tests, no custom test
harness required.

`dir-test` is used unmodified, as an ordinary `dev-dependency` of the
harness — autograder does not fork it, wrap it, or special-case it
anywhere. It isn't parsed, invoked, or otherwise touched by any autograder
code; it just needs to work the way it already does for any crate that
depends on it, and its generated `#[test]` fns fall out of the same
`cargo nextest run` invocation the evaluator already performs. The
`case`/`value` reporting convention (below) and the "public/private
directory" convention (above) are conventions the *harness author*
follows inside the function body `#[dir_test]` wraps — not something
`dir-test` itself knows about or needs to support.

Neither declaration mechanism attaches a point value to a test. Points are
not attached to individual tests at all — see Scoring below. This
supersedes earlier point-per-test ideas (a `points` field on doc-comments
or fixture files): once scoring is computed by summing self-reported
values, a per-test point declaration would be redundant with what the
test itself reports at run time.

## Scoring signal: what a test reports

Any test — hand-written or fixture-generated — may report a numeric
contribution by printing a line to stdout during its run:

```
autograder: case=<id> score=<f64>
```

- `score` is an arbitrary number chosen by the instructor's judge code —
  a raw correctness measurement, not necessarily 0.0–1.0 or already
  scaled to points. Interpreting it is the scoring formula's job (below),
  not the test's.
- `case` is a stable, instructor-chosen identifier for the test — for
  fixture-driven tests this is naturally the fixture's own file stem
  (available at runtime via `Fixture::path()`), decoupling it from
  whatever mangled name `dir-test` generated. It is optional and used only
  for report/diagnostic traceability (e.g. "case insert_basic: 0.83") —
  the scoring formulas below don't look anything up by name or case, they
  only aggregate.
- A test that reports **no** `score=` line contributes `1.0` if it passed
  and `0.0` if it failed — the default, backward-compatible behavior for
  plain boolean tests that don't opt into partial credit.
- A test may report more than one `score=` line (e.g. a hand-written test
  looping over several internal checks); all of them are summed.

This requires `cargo nextest`'s JUnit output to capture stdout for
*passing* tests, not just failing ones (`[profile.default.junit]
store-success-output = true` in the harness's `.config/nextest.toml`),
since a test can pass while only earning partial credit.

## Scoring formulas

`[scoring].formula` replaces the old `[scoring].model`. Two variants:

### `sum`

```toml
[scoring]
formula = "sum"
base = 1.0
```

```
score = base + Σ(reported values)
```

No normalization. The instructor is responsible for designing each test's
value range so the sums land in a sensible spot for the target scale.

### `affine`

```toml
[scoring]
formula = "affine"
max-sum = 20.0
scale-min = 1.0
scale-max = 7.0
```

```
score = scale_min + clamp(Σ(reported values) / max_sum, 0, 1) * (scale_max - scale_min)
```

`max_sum` is a single, fixed, instructor-declared constant — what a
perfect submission's values are expected to sum to — obtained by running
the reference solution once and summing what it reports, or computed
analytically. It is never derived from how many tests happened to run at
grading time: a crashing test must not shrink the denominator and
implicitly inflate the score.

### Failure floor

Both formulas floor at their worst-case value when a stage fails before
Evaluate ever runs (build failure, fetch failure, etc.) — the same
`stage_failed` short-circuit `grade.rs` already applies to other models,
just landing on `base` (`sum`) or `scale_min` (`affine`) instead of `0.0`,
since neither of those is a valid "worse than everything else" value on
these scales.

## Non-goals / deferred

- A configurable expression language for custom formulas beyond `sum` and
  `affine` — not needed unless a concrete case shows the two aren't
  enough.
- Per-test-name score overrides in `autograder.toml` — superseded by the
  runtime-reporting model; an instructor who needs to adjust one test's
  weight changes what that test reports, not a TOML entry.
- Migrating the currently-implemented `Weighted`/`PassCount`/`PassFail`
  models to the same floor convention — out of scope here, `sum` and
  `affine` are net-new, additive variants.
