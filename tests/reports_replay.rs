mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dots_cli::{report::Report, ui::transcript};
use serde_json::{Value, json};
use support::World;

fn dots(world: &World) -> Command {
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

fn save(dir: &Path, name: &str, report: &Value) {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(name))
        .unwrap();
    serde_json::to_writer(file, report).unwrap();
}

fn snapshot(dir: &Path) -> BTreeMap<OsString, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect()
}

fn guard_commands(world: &World) -> (PathBuf, PathBuf) {
    let bin = world.home().join("guard-bin");
    let calls = world.home().join("unexpected-commands");
    fs::create_dir(&bin).unwrap();
    for name in ["mise", "git", "jj", "sudo", "brew", "mas", "retry-update"] {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(bin.join(name))
            .unwrap();
        file.write_all(
            b"#!/bin/sh\nprintf '%s\\n' \"$0\" >> \"$DOTS_TEST_COMMAND_LOG\"\nexit 97\n",
        )
        .unwrap();
    }
    (bin, calls)
}

#[test]
fn new_up_last_replays_latest_saved_update_read_only() {
    let world = World::new();
    let store = world.home().join(".local/state/dots/reports");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&store)
        .unwrap();
    save(&store, "z-older.json", &fixture("up", 1000, 0));
    save(&store, "newer-sync.json", &fixture("sync", 3000, 17));
    save(&store, ".pending-update", &fixture("up", 4000, 71));

    let latest = json!({
        "command": "up",
        "finished_at_ms": 2000,
        "status": 23,
        "cancelled": true,
        "next": "retry-update",
        "stages": [{
            "id": 0,
            "label": "Package updates",
            "alongside": false,
            "outcome": "cancelled",
            "changes": [{"item": "fixture-package", "before": "1.0", "after": "1.1"}],
            "issues": [{
                "diagnostic": "Native command stopped",
                "next": "retry-update",
                "terminal_exit": 23,
            }],
        }],
    });
    save(&store, "a-latest-update.json", &latest);
    let saved = snapshot(&store);
    let report: Report = serde_json::from_value(latest).unwrap();
    let mut expected = Vec::new();
    transcript::write_report(&mut expected, &report, true).unwrap();
    assert!(expected.starts_with(b"dots up: Cancelled (exit 23)\n"));

    let config = world.root().join("config.toml");
    let original_config = fs::read(&config).unwrap();
    let (bin, calls) = guard_commands(&world);
    let piped = dots(&world)
        .env("PATH", &bin)
        .env("DOTS_TEST_COMMAND_LOG", &calls)
        .args(["up", "--last"])
        .output()
        .unwrap();
    assert_eq!(piped.status.code(), Some(23), "{piped:?}");
    assert!(piped.stderr.is_empty(), "{piped:?}");
    assert!(
        piped
            .stdout
            .ends_with("Details and logs · dots up --last\n".as_bytes()),
        "{piped:?}"
    );
    assert_eq!(piped.stdout, expected);

    let redirected_file = world.home().join("redirected-update");
    let redirected = dots(&world)
        .env("PATH", &bin)
        .env("DOTS_TEST_COMMAND_LOG", &calls)
        .args(["update", "--last"])
        .stdout(Stdio::from(File::create(&redirected_file).unwrap()))
        .output()
        .unwrap();
    assert_eq!(redirected.status.code(), Some(23), "{redirected:?}");
    assert!(redirected.stderr.is_empty(), "{redirected:?}");
    assert_eq!(fs::read(redirected_file).unwrap(), expected);
    assert_eq!(snapshot(&store), saved);
    assert_eq!(fs::read(config).unwrap(), original_config);
    assert!(!calls.exists(), "replay invoked a command: {calls:?}");
}

#[test]
fn unchanged_up_last_reports_missing_history() {
    let world = World::new();
    let output = dots(&world).args(["up", "--last"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        output
            .stderr
            .ends_with(b": no completed update report is available yet\n"),
        "{output:?}"
    );
    assert!(!world.home().join(".local/state/dots").exists());
}
