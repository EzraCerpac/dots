mod support;

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{FixtureCommand, World};

fn dots(world: &World) -> FixtureCommand<'_> {
    let home = world.home();
    let mut command = world.dots();
    command
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("MISE_CONFIG_DIR", home.join(".config/mise"))
        .env("MISE_DATA_DIR", home.join(".local/share/mise"))
        .env("MISE_STATE_DIR", home.join(".local/state/mise"))
        .env("MISE_CACHE_DIR", home.join(".cache/mise"));
    command
}

fn fixture(command: &str, time: u64, status: u8) -> Value {
    json!({
        "command": command,
        "finished_at_ms": time,
        "status": status,
        "next": null,
        "stages": [],
    })
}

fn save(world: &World, name: &str, report: Value) -> PathBuf {
    let dir = world.home().join(".local/state/dots/reports");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    let path = dir.join(name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    serde_json::to_writer(file, &report).unwrap();
    path
}

#[test]
fn new_reports_list_newest_first_in_pipes_and_redirects() {
    let world = World::new();
    let old = save(
        &world,
        "z-old's report.json",
        fixture("remove", 1_704_067_200_000, 0),
    );
    let mut deferred = fixture("up", 1_704_067_201_001, 0);
    deferred["stages"] = json!([{
        "id": 0,
        "label": "Deferred maintenance",
        "alongside": false,
        "outcome": "deferred",
        "changes": [],
        "issues": [],
    }]);
    let deferred = save(&world, "y-deferred.json", deferred);
    let failed = save(
        &world,
        "b-failed.json",
        fixture("test", 1_704_067_202_002, 130),
    );
    let mut cancelled = fixture("sync", 1_704_067_203_003, 0);
    cancelled["cancelled"] = json!(true);
    let cancelled_sync = save(&world, "a-cancelled-sync.json", cancelled);
    let mut cancelled = fixture("test", 1_704_067_204_004, 23);
    cancelled["cancelled"] = json!(true);
    let cancelled_child = save(&world, "a-cancelled-child.json", cancelled);
    save(
        &world,
        ".pending-incomplete",
        fixture("test", 1_704_067_205_005, 0),
    );

    let expected = format!(
        "dots test  2024-01-01 00:00:04.004 UTC  cancelled (exit 23)\n  {}\n\
         dots sync  2024-01-01 00:00:03.003 UTC  cancelled (exit 0)\n  {}\n\
         dots test  2024-01-01 00:00:02.002 UTC  failed (exit 130)\n  {}\n\
         dots up  2024-01-01 00:00:01.001 UTC  needs attention (exit 0)\n  {}\n\
         dots remove  2024-01-01 00:00:00.000 UTC  completed (exit 0)\n  {}\n",
        cancelled_child.display(),
        cancelled_sync.display(),
        failed.display(),
        deferred.display(),
        old.display(),
    );

    let piped = dots(&world)
        .arg("reports")
        .env("COLUMNS", "24")
        .output()
        .unwrap();
    assert!(piped.status.success(), "{piped:?}");
    assert!(piped.stderr.is_empty(), "{piped:?}");
    assert_eq!(piped.stdout, expected.as_bytes());

    let file = world.home().join("redirected-listing");
    let redirected = dots(&world)
        .arg("reports")
        .env("COLUMNS", "120")
        .stdout(Stdio::from(File::create(&file).unwrap()))
        .output()
        .unwrap();
    assert!(redirected.status.success(), "{redirected:?}");
    assert!(redirected.stderr.is_empty(), "{redirected:?}");
    assert_eq!(fs::read(file).unwrap(), expected.as_bytes());
}

#[test]
fn new_reports_distinguish_newlines_from_literal_backslashes_in_paths() {
    let world = World::new();
    save(
        &world,
        "line\nnext.json",
        fixture("up", 1_704_067_200_000, 0),
    );
    save(
        &world,
        r"line\nnext.json",
        fixture("up", 1_704_067_201_000, 0),
    );

    let output = dots(&world).arg("reports").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert!(lines[1].ends_with(r"/line\\nnext.json"), "{text}");
    assert!(lines[3].ends_with(r"/line\nnext.json"), "{text}");
}

#[test]
fn new_reports_filter_accepts_canonical_commands_and_aliases() {
    let world = World::new();
    save(&world, "older.json", fixture("up", 1_704_067_200_000, 0));
    save(
        &world,
        "newer.json",
        fixture("remove", 1_704_067_201_000, 0),
    );

    for (canonical, alias, other) in [("up", "update", "remove"), ("remove", "uninstall", "up")] {
        let output = dots(&world).args(["reports", canonical]).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let text = std::str::from_utf8(&output.stdout).unwrap();
        assert!(text.starts_with(&format!("dots {canonical}  ")), "{text}");
        assert!(!text.contains(&format!("dots {other}  ")), "{text}");

        let aliased = dots(&world).args(["reports", alias]).output().unwrap();
        assert!(aliased.status.success(), "{aliased:?}");
        assert!(aliased.stderr.is_empty(), "{aliased:?}");
        assert_eq!(output.stdout, aliased.stdout);
    }
}

#[test]
fn new_reports_without_saved_reports_is_empty() {
    let world = World::new();
    let output = dots(&world).arg("reports").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!world.home().join(".local/state/dots/reports").exists());
}

#[test]
fn unchanged_reports_rejects_invalid_arguments() {
    let world = World::new();
    for args in [
        &["reports", "does-not-exist"][..],
        &["reports", "up", "remove"],
    ] {
        let output = dots(&world).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(!output.stderr.is_empty(), "{output:?}");
    }
}
