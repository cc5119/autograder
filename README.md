# autograder

A sandboxed autograder for Rust programming assignments. See
[`docs/design.md`](docs/design.md) and [`docs/plan.md`](docs/plan.md) for
design and implementation status.

```sh
cargo build
```

## Try it

[`examples/linked-library-stack/`](examples/linked-library-stack/) is a
working `linked-library` assignment (a tiny `Stack<i64>`): `instructor/`
(private full spec + `harness/driver/` + judge, public + hidden tests, and a
reference `solution/` kept alongside the driver so the harness can be graded
against it directly), `public/` (the independent public-only copy students
receive).

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

# vendor the (empty) dependency allowlist
$BIN prefetch examples/linked-library-stack/instructor
$BIN prefetch examples/linked-library-stack/public

# authoritative grading, needs Podman (add --local-sandbox if you don't have it)
# -- grade wants a --submissions dir with one subdirectory per student, so
# stage the reference solution as a scratch "alice" submission
rm -rf /tmp/ll-stack-submissions
mkdir -p /tmp/ll-stack-submissions/alice
cp -r examples/linked-library-stack/instructor/solution/. /tmp/ll-stack-submissions/alice/
$BIN grade examples/linked-library-stack/instructor --submissions /tmp/ll-stack-submissions
$BIN report linked-library-stack-example --format csv

# student-facing CI check, no Podman needed -- runs in the student's own
# checkout, so cd into it first (here, the reference solution)
cd examples/linked-library-stack/instructor/solution
$BIN ci --harness ../../public
cd -

# generate the starter repo a student would clone
$BIN scaffold examples/linked-library-stack/public --out /tmp/starter-stack
```

Use the already-built `$BIN` rather than `cargo run` once you `cd` into
`instructor/solution/`: `prefetch` writes `instructor/.cargo/config.toml`
(pointing at `instructor/vendor/`), and Cargo's config discovery walks *up*
from the current directory — so a `cargo run` invoked from inside
`instructor/solution/` would pick that config up too and try to resolve
`autograder`'s own crates.io dependencies against the assignment's (empty)
vendored set instead of the real registry.

Generated artifacts (`vendor/`, `.cargo/`, `.autograder-store/`, and the
`driver/` crate `ci` overlays into `instructor/solution/`) are gitignored.
