# Rust black-box tests

Each suite is its own `tests/<suite>.rs` file that starts with `mod support;`.
The fixture type then lives at `support::World`, so either spell it that way or add `use support::World;`.
`World::new()` copies `tests/fixtures/config` into a private temporary directory.
Hold on to the world until every child it started has exited.
`world.dots()` and `world.git()` each hand back a new command that has not started yet
and runs in the fixture root.
For a shell launcher, or a binary at a test install path, use `world.command(program)`.
`FixtureCommand` passes native OS arguments and stdio, and before any output, status
or spawn it checks the working directory and application state paths it will really use.
Agent directories do not exist until a scenario creates them.
To test skill linking, remove `DOTS_SKILL_LINKS` from the fixture command.
`world.bare_remote("origin")` makes a local bare Git repository on `main`
and returns its absolute path.

```rust
mod support;

use support::World;

#[test]
fn unchanged_help_without_configuration() {
    let world = World::new();
    let output = world.dots().env_remove("DOTS_ROOT").arg("help").output().unwrap();
    assert!(output.status.success(), "{output:?}");
}
```

## Prerequisites

`mise.toml` pins only Rust.
Run Cargo as `mise exec -- cargo ...`, so that Cargo and rustc both come from that toolchain without activating mise in your shell.

The suites also run real programs inside their fixtures: `bash`, `zsh`, `fish`, `git` 2.43 or newer, `jq`, `age`, `age-keygen`, `mise` and `gitleaks`.
The receipt suite, for example, stops when no real `gitleaks` is on `PATH`.
A fixture replaces `HOME`, the XDG and mise directories and the Git configuration, but starts from the caller's `PATH`, and some suites drop mise shim directories from it.
So put each tool's real executable in a directory on `PATH` before running Cargo, rather than relying on mise shims or shell activation.
CI does this with `.github/scripts/setup-scenario-tools`, which installs pinned `mise`, `age`, `age-keygen`, `gitleaks` and `fish` under one prefix and puts its `bin` directories on `PATH`.

These are test needs only: what the `dots` executable needs at run time is listed in [Platforms and programs](../docs/contract.md#platforms-and-programs).

## Run and count cases

Run everything with `cargo test --locked -- --test-threads 1`, or one suite with `cargo test --locked --test SUITE`, through `mise exec` as above.
`dots test` is something else: it runs a configuration's own `tests/run`, not these suites.
The library tests have terminal and signal fixtures that must not overlap, so the full local run is serial.
CI serializes only the library tests, and runs the binaries, integration suites and doctests separately.

```sh
mise exec -- cargo test --locked -- --test-threads 1
mise exec -- cargo test --locked --test contract
mise exec -- cargo test --locked --test contract -- --list
mise exec -- cargo test --locked --lib -- --test-threads 1
```

On DriehuisNAS, run each test command through `mise run nas:build --`.

In CI, a pull request into `main` that changes code runs the Linux tests and one ARM macOS test job, and skips packaging.
A pull request that touches `packaging/**`, `.github/workflows/**`, `.github/scripts/**`, `.github/actions/**`, `Cargo.toml` or `Cargo.lock`, and every code push to `main` and every tag, takes the full route instead.
The full route adds sharded ARM and Intel macOS tests, all four package jobs and real install checks.

Name a case `unchanged_` when v0.4.0 passes it too,
and `new_` when it covers behaviour that v0.4.0 lacks.
While Rust does not have the feature yet, mark the case `#[ignore = "P-NN"]` with the task that implements it,
and take the mark off once that implementation lands.
`DOTS_BIN`, set to an absolute path, chooses a different executable;
when it is unset, Cargo provides `dots`.

Every suite has an `inventory/<suite>.tsv` file whose header is
`unchanged<TAB>new<TAB>dropped`, followed by one row of integer counts.
Take the `unchanged_` and `new_` counts from the Cargo listing, ignored cases included.
A case from the retired legacy suites that was dropped in translation is listed in `inventory/<suite>-dropped.tsv`,
by its `Class.test_method` identifier and a concrete reason, under the header `legacy_case<TAB>reason`.
The dropped count is the number of entries in that file.
