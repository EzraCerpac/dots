mod support;

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{Value, json};
use support::World;

fn save(world: &World, name: &str, command: &str, time: u64, status: u8, stages: Value) -> PathBuf {
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
    serde_json::to_writer(
        file,
        &json!({
            "command": command,
            "finished_at_ms": time,
            "status": status,
            "next": null,
            "stages": stages,
        }),
    )
    .unwrap();
    path
}

#[test]
fn new_reports_list_newest_first_in_pipes_and_redirects() {
    let world = World::new();
    let old = save(
        &world,
        "z-old.json",
        "remove",
        1_704_067_200_000,
        0,
        json!([]),
    );
    let deferred = save(
        &world,
        "y-deferred.json",
        "up",
        1_704_067_201_001,
        0,
        json!([{
            "id": 0,
            "label": "Deferred maintenance",
            "alongside": false,
            "outcome": "deferred",
            "changes": [],
            "issues": [],
        }]),
    );
    let failed = save(
        &world,
        "b-failed.json",
        "up",
        1_704_067_202_002,
        19,
        json!([]),
    );
    let cancelled = save(
        &world,
        "a-cancelled.json",
        "sync",
        1_704_067_203_003,
        130,
        json!([]),
    );
    save(
        &world,
        ".pending-incomplete",
        "test",
        1_704_067_204_004,
        0,
        json!([]),
    );

    let expected = format!(
        "dots sync  2024-01-01 00:00:03.003 UTC  cancelled (exit 130)\n  {}\n\
         dots up  2024-01-01 00:00:02.002 UTC  failed (exit 19)\n  {}\n\
         dots up  2024-01-01 00:00:01.001 UTC  needs attention (exit 0)\n  {}\n\
         dots remove  2024-01-01 00:00:00.000 UTC  completed (exit 0)\n  {}\n",
        cancelled.display(),
        failed.display(),
        deferred.display(),
        old.display(),
    );

    let piped = world
        .dots()
        .arg("reports")
        .env("COLUMNS", "24")
        .output()
        .unwrap();
    assert!(piped.status.success(), "{piped:?}");
    assert!(piped.stderr.is_empty(), "{piped:?}");
    assert_eq!(piped.stdout, expected.as_bytes());

    let file = world.home().join("redirected-listing");
    let redirected = world
        .dots()
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
fn new_reports_filter_accepts_canonical_commands_and_aliases() {
    let world = World::new();
    save(&world, "older.json", "up", 1_704_067_200_000, 0, json!([]));
    save(
        &world,
        "newer.json",
        "remove",
        1_704_067_201_000,
        0,
        json!([]),
    );

    for (canonical, alias, other) in [("up", "update", "remove"), ("remove", "uninstall", "up")] {
        let output = world.dots().args(["reports", canonical]).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let text = std::str::from_utf8(&output.stdout).unwrap();
        assert!(text.starts_with(&format!("dots {canonical}  ")), "{text}");
        assert!(!text.contains(&format!("dots {other}  ")), "{text}");

        let aliased = world.dots().args(["reports", alias]).output().unwrap();
        assert!(aliased.status.success(), "{aliased:?}");
        assert!(aliased.stderr.is_empty(), "{aliased:?}");
        assert_eq!(output.stdout, aliased.stdout);
    }
}

#[test]
fn new_reports_without_saved_reports_is_empty() {
    let world = World::new();
    let output = world.dots().arg("reports").output().unwrap();
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
        let output = world.dots().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(!output.stderr.is_empty(), "{output:?}");
    }
}
