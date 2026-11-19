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

## Run and count cases

Run every suite with `cargo test --locked`, or one suite with `cargo test --locked --test SUITE`; `mise run test` does the same with the Rust that `mise.toml` pins.
`dots test` is something else: it runs a configuration's own `tests/run`, not these suites.

```sh
cargo test --locked --test contract
cargo test --locked --test contract -- --list
```

On DriehuisNAS, run each test command through `mise run nas:build --`.

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
