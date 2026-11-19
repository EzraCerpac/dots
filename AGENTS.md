# dots

[README.md](README.md) describes the manager and its commands, and [the contract](docs/contract.md) what 0.5 promises.

Run the Rust tests with `cargo test --locked`, or a focused set with `cargo test --locked FILTER`, using the Rust toolchain that `mise.toml` pins.
`dots test` is different: it runs a configuration's own `tests/run`, not this repository's tests.

## Code Review Rules

When reviewing a pull request, read [REVIEW.md](REVIEW.md) first and follow it.
