# containers

Builds and publishes the `autograder-base` container image used by the
grading sandbox.

## Base image

`Containerfile` builds from `rust:latest` (Docker Hub's rolling latest-stable
tag) and installs `cargo-nextest`. On every push to `main` that touches
`Containerfile`, `.github/workflows/build-base-image.yml` builds the image
and pushes it to GHCR as:

- `ghcr.io/cc5119/autograder-base:latest`
- `ghcr.io/cc5119/autograder-base:<resolved-rustc-version>` (e.g. `1.86.0`),
  extracted from `rustc --version` inside the built image — for pinning to a
  specific, reproducible version.

### One-time setup

The first successful workflow run creates the GHCR package as **private**
(GitHub doesn't allow setting visibility via the push itself). To make it
public: go to https://github.com/orgs/cc5119/packages →
**autograder-base** → **Package settings** → **Change visibility** →
**Public**.

### Install image podman

```sh
podman pull ghcr.io/cc5119/autograder-base:<channel>
podman tag ghcr.io/cc5119/autograder-base:<channel> autograder-base:<channel>
```
