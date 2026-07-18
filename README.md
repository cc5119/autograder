# autograder

A sandboxed autograder for Rust programming assignments.

```sh
cargo build
```

## Try it

Two working example assignments live under [`examples/`](examples/), one
per `[assignment].kind`. Each is a single, self-contained private package —
full spec + harness + a reference solution kept alongside it, no separate
hand-maintained public repo. `scaffold` derives everything a student needs
(a spec with no points/hidden tests, a harness with only the public test,
and a starter `src/` with the solution's API shape but `todo!()` bodies)
straight from the package in one pass — see `src/publish.rs` and
`src/stub.rs`.

There's no `[student]` spec section either: `[assignment].id` doubles as
the student-facing crate/binary name everywhere — it's what the harness
depends on or spawns, and what the reference solution's own directory must
be named (`<id>/`, validated by `scaffold`). The starter's `Cargo.toml` is
a straight copy of that solution's own manifest, not separately generated.
One identifier, nothing to keep in sync by hand.

Needs `cargo-nextest` (`cargo install cargo-nextest --locked`) for the run
stage, and rootless Podman for `grade`'s authoritative sandbox — `grade`
checks Podman up front and fails immediately with one clear error if it
isn't usable, rather than grading anyone. `ci` and `scaffold` don't need
Podman. No working Podman on this machine (or in a container/sandbox that
can't run nested containers)? Pass `grade --local-sandbox` to grade over a
plain host process instead — **dev/testing only**, it drops the container
isolation the authoritative tier relies on for untrusted code, so never use
it to grade real submissions.

### `library`: [`examples/library-stack/`](examples/library-stack/)

A tiny `Stack<i64>`. `harness/Cargo.toml` depends on the assignment's crate
via `<id> = "*"` plus a checked-in `[patch.crates-io]` pointing at `../<id>`
by default — so `cd examples/library-stack/harness && cargo nextest run`
grades the reference solution directly, no autograder involvement.
Grading/`ci` always override that default via a `--config` CLI flag
pointing at whatever checkout is actually being evaluated (a config-sourced
`[patch]` takes precedence over one declared in the manifest).

```sh
cargo build
BIN=$PWD/target/debug/autograder

# sanity-check the harness against the reference solution -- no autograder,
# needs cargo-nextest installed
(cd examples/library-stack/harness && cargo nextest run)

# vendor the (empty) dependency allowlist
$BIN prefetch examples/library-stack

# authoritative grading, needs Podman (add --local-sandbox if you don't have it)
# -- grade wants a --submissions dir with one subdirectory per student, so
# stage the reference solution as a scratch "alice" submission
rm -rf /tmp/ll-stack-submissions
mkdir -p /tmp/ll-stack-submissions/alice
cp -r examples/library-stack/library-stack-example/. /tmp/ll-stack-submissions/alice/
$BIN grade examples/library-stack --submissions /tmp/ll-stack-submissions
$BIN report library-stack-example --format csv

# generate the starter repo a student would clone: derives the public spec
# (no points/hidden tests) + public harness (only the public test, plain
# path dependency instead of [patch]) + a starter src/lib.rs (solution's
# API shape, todo!() bodies -- picked up automatically from <id>/, no
# --solution flag) -- laid out as a structural mirror of the private
# package, so it's genuinely just autograder.public.toml/harness/<id>/
# side by side, no .autograder/ wrapper
$BIN scaffold examples/library-stack --out /tmp/starter-stack

# a bare `cargo test` from the starter root already runs both the
# student's own crate and the public harness's judge test -- the whole
# point of the workspace + path-dependency layout, no autograder at all
(cd /tmp/starter-stack && cargo test)

# student-facing CI check: ci always runs from the starter repo root (no
# --harness flag -- it finds the student's own crate at the sibling
# directory named after [assignment].id automatically). In real CI this
# runs inside the same ContainerSandbox `grade` uses; --local-sandbox here
# skips Podman for this local demo.
(cd /tmp/starter-stack && $BIN ci --local-sandbox)
```

### `binary`: [`examples/binary-fizzbuzz/`](examples/binary-fizzbuzz/)

FizzBuzz as a CLI: the student binary (target name `fizzbuzz`) takes `n` as
its one argument and prints lines `1..=n`. Unlike `library`, there's no
separate driver/harness crate — `harness/tests/judge.rs` is copied directly
onto the student's own checkout (`prepare::Wiring::Binary`), and its judge
spawns the built binary via `env!("CARGO_BIN_EXE_fizzbuzz")` (Cargo sets
that automatically for an integration test in the same package as the bin
target) and asserts purely on its stdout. Same commands as `library`, just
pointed at the other example directory and with no standalone
`cd harness && cargo nextest run` (there's no crate there to run it in):

```sh
$BIN prefetch examples/binary-fizzbuzz

rm -rf /tmp/fizzbuzz-submissions
mkdir -p /tmp/fizzbuzz-submissions/alice
cp -r examples/binary-fizzbuzz/fizzbuzz/. /tmp/fizzbuzz-submissions/alice/
$BIN grade examples/binary-fizzbuzz --submissions /tmp/fizzbuzz-submissions
$BIN report fizzbuzz --format csv

# scaffold also statically copies the public harness tests straight into
# fizzbuzz/tests/ (there's no crate to link in via the workspace the way
# library's harness/ is), so `cargo test` from the starter root works here
# too, no autograder involvement
$BIN scaffold examples/binary-fizzbuzz --out /tmp/starter-fizzbuzz
(cd /tmp/starter-fizzbuzz && cargo test)

(cd /tmp/starter-fizzbuzz && $BIN ci --local-sandbox)
```

Use the already-built `$BIN` rather than `cargo run` once you `cd` into a
reference solution directory: `prefetch` writes `<package>/.cargo/
config.toml` (pointing at `<package>/vendor/`), and Cargo's config discovery
walks *up* from the current directory — so a `cargo run` invoked from
inside that directory would pick that config up too and try to resolve
`autograder`'s own crates.io dependencies against the assignment's (empty)
vendored set instead of the real registry.

Generated artifacts (`vendor/`, `.cargo/`, `.autograder-store/`) are
gitignored. `ci` never writes into the student's own crate directory
(`workspace`) — for `library` it builds `harness/` in place, a sibling
directory in the same starter repo, never inside the crate actually being
graded; for `binary` it builds directly in `workspace` (there's no
separate harness crate to isolate). Real grading (`grade`, no
`--local-sandbox`) is different again: there `harness/` is copied into a
fresh scratch directory under the system temp dir for every job, since the
checkout it needs to link against is a different, arbitrary location each
time (see `prepare::prepare`'s doc comment).

## Setting up Podman

`grade` (without `--local-sandbox`) runs student code in rootless Podman
containers. Three things need to be in place first:

1. **Install rootless Podman** (`sudo dnf install podman` / `sudo apt-get
   install podman`), then confirm `podman info` works without `sudo`. See
   [Podman's rootless
   docs](https://github.com/containers/podman/blob/main/docs/tutorials/rootless_tutorial.md)
   if it doesn't.
2. **Seccomp profile** at `/etc/autograder/seccomp.json` (the fixed default
   in `Config`; not yet configurable via flag/file):
   ```sh
   sudo mkdir -p /etc/autograder
   sudo cp /usr/share/containers/seccomp.json /etc/autograder/seccomp.json
   ```
3. **Base image**, matching the assignment's `[sandbox].image` (a
   required field in `autograder.toml`; see
   `examples/library-stack/autograder.toml`). Build and tag it yourself:
   ```sh
   printf 'FROM docker.io/library/rust:1.86.0\nRUN cargo install cargo-nextest --locked\n' > Containerfile
   podman build -t autograder-base:1.86.0 -f Containerfile .
   ```
   `[sandbox].image` can also name a registry reference (e.g.
   `ghcr.io/org/autograder-base:1.86.0`) that you've already `podman pull`ed
   onto this host — `autograder` never builds or pulls the image itself, it
   only checks the configured reference already exists locally.

`ContainerSandbox::preflight` checks all this up front and fails clearly if
anything's missing, rather than grading anyone. `ci` and `--local-sandbox`
don't need Podman at all.
