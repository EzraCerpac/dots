# dots

[README.md](README.md) describes the manager and its commands, and [the contract](docs/contract.md) what 0.5 promises.

Run all the Rust tests with `cargo test --locked -- --test-threads 1`, since the library tests' terminal and signal fixtures must not overlap, or a focused set with `cargo test --locked FILTER`, using the Rust toolchain that `mise.toml` pins (`mise exec -- cargo ...` when mise is not activated).
`dots test` is different: it runs a configuration's own `tests/run`, not this repository's tests.

## Code Review Rules

When reviewing a pull request, read [REVIEW.md](REVIEW.md) first and follow it.
