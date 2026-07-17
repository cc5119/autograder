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
(private full spec + driver + judge, public + hidden tests), `public/` (the
independent public-only copy students receive), `submissions/alice/` (a
correct sample solution).

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
# vendor the (empty) dependency allowlist
cargo run -- prefetch examples/linked-library-stack/instructor
cargo run -- prefetch examples/linked-library-stack/public

# authoritative grading, needs Podman (add --local-sandbox if you don't have it)
cargo run -- grade examples/linked-library-stack/instructor \
  --submissions examples/linked-library-stack/submissions
cargo run -- report linked-library-stack-example --format csv

# student-facing CI check, no Podman needed -- runs in the student's own
# checkout, so cd into it first
cd examples/linked-library-stack/submissions/alice
cargo run --manifest-path ../../../../Cargo.toml -- ci --harness ../../public
cd -

# generate the starter repo a student would clone
cargo run -- scaffold examples/linked-library-stack/public --out /tmp/starter-stack
```

Generated artifacts (`vendor/`, `.cargo/`, `.autograder-store/`, and the
`driver/` crate `ci` overlays into the submission) are gitignored.
