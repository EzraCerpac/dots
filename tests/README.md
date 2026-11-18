# Rust black-box tests

Place each suite in `tests/<suite>.rs` and import `mod support;`.
`World::new()` copies `tests/fixtures/config` into a private temporary directory.
Keep the world alive until its children exit.
`world.dots()` and `world.git()` return fresh, unstarted commands
with the fixture root as their working directory.
`world.bare_remote("origin")` creates a local bare Git repository on `main`
and returns its absolute path.

```rust
#[test]
fn unchanged_help_without_configuration() {
    let world = World::new();
    let output = world.dots().env_remove("DOTS_ROOT").arg("help").output().unwrap();
    assert!(output.status.success(), "{output:?}");
}
```

## Run and count cases

Prefix cases that also pass against v0.4.0 with `unchanged_`
and 1.0-only cases with `new_`.
Use `#[ignore = "P-NN"]` with the implementing task while Rust lacks the feature.
Remove the ignore when the implementation lands.
Set `DOTS_BIN` to an absolute path to select another executable.
With `DOTS_BIN` unset, Cargo supplies `dots`.
Use a release or archive of commit `f5c8a0e` for the v0.4.0 oracle.

```sh
mise run test -- --test contract
DOTS_BIN=/absolute/path/to/v0.4.0/bin/dots mise run test -- unchanged_ -- --include-ignored
mise run test -- --test contract -- --list
```

On DriehuisNAS, wrap each test command with `mise run nas:build --`.

Each suite owns `inventory/<suite>.tsv` with the header
`unchanged<TAB>new<TAB>dropped` and one row of integer counts.
Count `unchanged_` and `new_` cases from the Cargo listing, including ignored cases.
When a legacy case is dropped, add its `Class.test_method` identifier
and a specific reason to `inventory/<suite>-dropped.tsv` under `legacy_case<TAB>reason`.
The dropped count equals the number of entries in that file.
The initial counts cover only these first translations and explicitly dropped cases.
The remaining legacy cases are pending.
