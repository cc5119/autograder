# autograder

## Tests

```sh
cargo test                                              # unit + integration
cargo test --features container-tests --test container  # + real-podman suite
```

The `container` suite needs podman and the base image (`container/Containerfile`);
override its tag with `AUTOGRADER_TEST_IMAGE`. A plain `cargo test` skips it.
