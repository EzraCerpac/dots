# dots

[README.md](README.md) describes the manager and its commands, and [the contract](docs/contract.md) what 1.0 promises.

Run the Rust tests with `cargo test --locked`, or a focused set with `cargo test --locked FILTER`, using the Rust that `mise.toml` pins; `mise run test` does the same.
`dots test` is different: it runs a configuration's own `tests/run`, not this repository's tests.

> TODO(D-01): Until D-01 deletes them, the legacy suites under `tests/` still run with `tests/run`; drop this line with them.

## Code Review Rules

When reviewing a pull request, read [REVIEW.md](REVIEW.md) first and follow it.
