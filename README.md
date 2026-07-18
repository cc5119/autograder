# autograder

A sandboxed autograder for Rust programming assignments. See
[`docs/design.md`](docs/design.md) and [`docs/plan.md`](docs/plan.md) for
design and implementation status.

```sh
cargo build
```

## Try it

[`examples/linked-library-stack/`](examples/linked-library-stack/) is a
working `linked-library` assignment (a tiny `Stack<i64>`): just
`instructor/` — private full spec + `harness/` (the driver crate + judge,
public + hidden tests) + a reference solution kept alongside it. There's no
separate hand-maintained public repo to keep in sync: `scaffold` derives
everything a student needs (a spec with no points/hidden tests, a harness
with only the public test, and a starter `src/` with the solution's API
shape but `todo!()` bodies) straight from `instructor/` in one pass — see
`src/publish.rs` and `src/stub.rs`.

There's no `[student]` spec section either: `[assignment].id` doubles as
the student-facing crate name everywhere (design §5) — it's what
`harness/Cargo.toml` depends on, and what the reference solution's own
directory must be named (`instructor/<id>/`, validated by `scaffold`).
The starter's `Cargo.toml` is a straight copy of that solution's own
manifest, not separately generated. One identifier, nothing to keep in
sync by hand.

`harness/Cargo.toml` depends on the assignment's crate via `<id> = "*"`
plus a checked-in `[patch.crates-io]` pointing at `../<id>` by default — so
`cd examples/linked-library-stack/instructor/harness && cargo nextest run`
grades the reference solution directly, no autograder involvement.
Grading/`ci` always override that default via a `--config` CLI flag
pointing at whatever checkout is actually being evaluated (a config-sourced
`[patch]` takes precedence over one declared in the manifest).

Needs `cargo-nextest` (`cargo install cargo-nextest --locked`) for the run
stage, and rootless Podman for `grade`'s authoritative sandbox — `grade`
checks Podman up front and fails immediately with one clear error if it
isn't usable, rather than grading anyone. `ci` and `scaffold` don't need
Podman. No working Podman on this machine (or in a container/sandbox that
can't run nested containers)? Pass `grade --local-sandbox` to grade over a
plain host process instead — **dev/testing only**, it drops the container
isolation the authoritative tier relies on for untrusted code, so never use
it to grade real submissions.

```sh
cargo build
BIN=$PWD/target/debug/autograder

# sanity-check the harness against the reference solution -- no autograder,
# needs cargo-nextest installed
(cd examples/linked-library-stack/instructor/harness && cargo nextest run)

# vendor the (empty) dependency allowlist
$BIN prefetch examples/linked-library-stack/instructor

# authoritative grading, needs Podman (add --local-sandbox if you don't have it)
# -- grade wants a --submissions dir with one subdirectory per student, so
# stage the reference solution as a scratch "alice" submission
rm -rf /tmp/ll-stack-submissions
mkdir -p /tmp/ll-stack-submissions/alice
cp -r examples/linked-library-stack/instructor/linked-library-stack-example/. /tmp/ll-stack-submissions/alice/
$BIN grade examples/linked-library-stack/instructor --submissions /tmp/ll-stack-submissions
$BIN report linked-library-stack-example --format csv

# generate the starter repo a student would clone: derives the public spec
# (no points/hidden tests) + public harness (only the public test, no
# [patch]) + a starter src/lib.rs (solution's API shape, todo!() bodies --
# picked up automatically from instructor/<id>/, no --solution flag) --
# all from the private instructor/ package in one command
$BIN scaffold examples/linked-library-stack/instructor --out /tmp/starter-stack

# student-facing CI check, no Podman needed -- runs against the harness
# scaffold just derived, from within a student's own checkout (here, the
# reference solution again)
(cd examples/linked-library-stack/instructor/linked-library-stack-example && \
  $BIN ci --harness /tmp/starter-stack/.autograder/public)
```

Use the already-built `$BIN` rather than `cargo run` once you `cd` into
`instructor/linked-library-stack-example/`: `prefetch` writes
`instructor/.cargo/config.toml` (pointing at `instructor/vendor/`), and
Cargo's config discovery walks *up* from the current directory — so a
`cargo run` invoked from inside that directory would pick that config up
too and try to resolve `autograder`'s own crates.io dependencies against
the assignment's (empty) vendored set instead of the real registry.

Generated artifacts (`vendor/`, `.cargo/`, `.autograder-store/`) are
gitignored. `ci` never writes into the checkout it's run from — the driver
it builds lives in a separate scratch directory under the system temp dir.
