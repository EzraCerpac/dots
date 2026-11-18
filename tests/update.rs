mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use support::World;

struct Fixture {
    world: World,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let world = World::new();
        let bin = world.home().join("fixture-bin");
        fs::create_dir(&bin).unwrap();
        let fixture = Self { world, bin };
        fixture.write("config.toml", "[vars]\ndots_profiles = \"laptop\"\n");
        fixture.write("dots/exceptions.tsv", "");
        fs::remove_file(fixture.world.root().join("dots/hooks/update.d/10-Greeting")).unwrap();
        fs::remove_file(
            fixture
                .world
                .root()
                .join("dots/hooks/update.d/20-Server-backups@server"),
        )
        .unwrap();
        fixture.executable(&fixture.bin.join("mise"), include_str!("fixtures/up/mise"));
        fixture.executable(&fixture.bin.join("brew"), include_str!("fixtures/up/brew"));
        fixture
    }

    fn write(&self, path: &str, data: &str) {
        fs::write(self.world.root().join(path), data).unwrap();
    }

    fn executable(&self, path: &std::path::Path, data: &str) {
        fs::write(path, data).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn command(&self, mut command: Command) -> Command {
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("SETUP_PROFILE", "laptop")
            .env("SETUP_MACHINE_ID", "fixture")
            .env("DOTS_MISE_BIN", self.bin.join("mise"))
            .env("SETUP_MISE_BIN", self.bin.join("mise"))
            .env("DOTS_UP_STATE_DIR", self.world.home().join("old-reports"))
            .env("MISE_SELF_UPDATE_AVAILABLE", "true");
        command
    }

    fn run(&self, expected: i32) -> Value {
        self.run_args(&["--plain", "up"], expected)
    }

    fn run_args(&self, args: &[&str], expected: i32) -> Value {
        self.run_command(self.command(self.world.dots()).args(args), expected)
    }

    fn run_command(&self, command: &mut Command, expected: i32) -> Value {
        let result = command.output().unwrap();
        assert_exit(&result, expected);
        let rust = self.world.home().join(".local/state/dots/reports");
        let path = if rust.exists() {
            fs::read_dir(&rust)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .max()
                .unwrap()
        } else {
            fs::read_dir(self.world.home().join("old-reports"))
                .unwrap()
                .map(|entry| entry.unwrap().path().join("report.json"))
                .filter(|candidate| candidate.is_file())
                .max()
                .unwrap()
        };
        let contents = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();
        serde_json::from_slice(&contents).unwrap()
    }

    fn recipe(&self, name: &str, data: &str) {
        self.executable(&self.world.root().join("dots/exceptions").join(name), data);
        self.write("dots/exceptions.tsv", &format!("base\t{name}\n"));
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.world.root().join("calls")).unwrap_or_default()
    }
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn changes(report: &Value) -> Vec<(&str, &str, &str)> {
    let changes: Vec<_> = if let Some(events) = report["events"].as_array() {
        events
            .iter()
            .filter(|event| event["kind"] == "change")
            .collect()
    } else {
        report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|stage| stage["changes"].as_array().unwrap())
            .collect()
    };
    let mut rows: Vec<_> = changes
        .iter()
        .map(|change| {
            (
                change["item"].as_str().unwrap(),
                change["before"].as_str().unwrap_or_default(),
                change["after"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    rows.sort_unstable();
    rows
}

const BREW_RECIPE: &str = r#"#!/bin/bash
set -euo pipefail
if [ -n "${DOTS_HOME:-}" ]; then
    source "$DOTS_HOME/lib/brew-exception.sh"
    "update_brew_${KIND}_exception" "$TOKEN"
else
    dots brew "update-$KIND" "$TOKEN"
fi
"#;

fn brew_up(fixture: &Fixture, kind: &str, expected: i32) -> Value {
    fixture.write("fail-version", "");
    fixture.run_command(
        fixture
            .command(fixture.world.dots())
            .env("KIND", kind)
            .env("TOKEN", "fixture")
            .args(["--plain", "up"]),
        expected,
    )
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "P-06"]
fn unchanged_brew_formula_reports_only_verified_changes() {
    let fixture = Fixture::new();
    fixture.recipe("managed-brew", BREW_RECIPE);
    assert_eq!(
        changes(&brew_up(&fixture, "formula", 0)),
        [("brew-formula:fixture", "not installed", "1")]
    );
    assert!(changes(&brew_up(&fixture, "formula", 0)).is_empty());
    fixture.write("brew-outdated", "");
    assert_eq!(
        changes(&brew_up(&fixture, "formula", 0)),
        [("brew-formula:fixture", "1", "2")]
    );
    assert!(changes(&brew_up(&fixture, "formula", 0)).is_empty());
    fixture.write("brew-failed", "");
    assert!(changes(&brew_up(&fixture, "formula", 1)).is_empty());
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "P-06"]
fn unchanged_brew_cask_reports_verified_install_and_upgrade() {
    let fixture = Fixture::new();
    fixture.recipe("managed-brew", BREW_RECIPE);
    assert_eq!(
        changes(&brew_up(&fixture, "cask", 0)),
        [("brew-cask:fixture", "not installed", "1")]
    );
    fixture.write("brew-outdated", "");
    assert_eq!(
        changes(&brew_up(&fixture, "cask", 0)),
        [("brew-cask:fixture", "1", "2")]
    );
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "P-02"]
fn unchanged_brew_recipe_without_a_change_sink_can_install() {
    let fixture = Fixture::new();
    fixture.executable(
        &fixture.world.root().join("dots/commands/fixture-brew"),
        BREW_RECIPE,
    );
    let output = fixture
        .command(fixture.world.dots())
        .env("KIND", "cask")
        .env("TOKEN", "fixture")
        .arg("fixture-brew")
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert_eq!(
        fs::read_to_string(fixture.world.root().join("brew-version"))
            .unwrap()
            .trim(),
        "1"
    );
    assert!(!fixture.world.home().join("old-reports").exists());
    assert!(
        !fixture
            .world
            .home()
            .join(".local/state/dots/reports")
            .exists()
    );
}

#[test]
#[ignore = "P-06"]
fn unchanged_tool_snapshot_reports_installed_active_deltas() {
    let fixture = Fixture::new();
    fixture.write(
        "tools-before.json",
        r#"{
        "old":[{"active":true,"installed":true,"version":"1"}],
        "new":[{"active":true,"installed":false,"version":"b"}],
        "removed":[{"active":true,"installed":true,"version":"a"}],
        "same":[{"active":true,"installed":true,"version":"s"}]
    }"#,
    );
    fixture.write(
        "tools-after.json",
        r#"{
        "old":[{"active":true,"installed":true,"version":"2"}],
        "new":[{"active":true,"installed":true,"version":"b"}],
        "same":[{"active":true,"installed":true,"version":"s"}],
        "inactive":[{"active":false,"installed":true,"version":"x"}]
    }"#,
    );
    fixture.write("fail-version", "");
    assert_eq!(
        changes(&fixture.run(0)),
        [
            ("new", "not installed", "b"),
            ("old", "1", "2"),
            ("removed", "a", "not installed"),
        ]
    );
}

#[test]
#[ignore = "P-06"]
fn unchanged_standalone_mise_snapshot_uses_its_own_version() {
    let fixture = Fixture::new();
    assert_eq!(
        changes(&fixture.run(0)),
        [("mise", "2026.11.8 macos-x64", "2026.11.9 macos-x64")]
    );
}

#[test]
#[ignore = "P-06"]
fn unchanged_unavailable_standalone_version_has_no_false_change() {
    let fixture = Fixture::new();
    fixture.write("fail-version", "");
    assert!(changes(&fixture.run(0)).is_empty());
    assert!(fixture.calls().contains("self-update"));
}

#[test]
#[ignore = "P-06"]
fn unchanged_hook_and_exception_report_the_four_change_meanings() {
    let fixture = Fixture::new();
    fixture.write("fail-version", "");
    let script = r#"#!/bin/sh
printf '%s\tnot installed\t1\n%s\t1\t2\n%s\t3\t\n%s\t4\t4\n' \
    "$0-new" "$0-old" "$0-gone" "$0-rebuilt" |
    sed 's|[^[:space:]]*/||g' >> "$DOTS_CHANGES"
"#;
    fixture.recipe("managed", script);
    fixture.executable(
        &fixture.world.root().join("dots/hooks/update.d/10-Hook"),
        script,
    );
    let report = fixture.run(0);
    assert_eq!(
        changes(&report),
        [
            ("10-Hook-gone", "3", ""),
            ("10-Hook-new", "not installed", "1"),
            ("10-Hook-old", "1", "2"),
            ("10-Hook-rebuilt", "4", "4"),
            ("managed-gone", "3", ""),
            ("managed-new", "not installed", "1"),
            ("managed-old", "1", "2"),
            ("managed-rebuilt", "4", "4"),
        ]
    );
}

#[test]
#[ignore = "P-06"]
fn unchanged_package_apply_keeps_an_explicit_absent_only_scope() {
    let fixture = Fixture::new();
    fixture.write(
        "packages.json",
        r#"{"pacman":{"available":true,"packages":[
        {"package":"retired","requested_version":"latest","desired_state":"absent",
         "state":"unexpectedly_installed","installed_version":"1"}
    ]}}"#,
    );
    fixture.run(0);
    assert!(
        fixture
            .calls()
            .lines()
            .any(|line| line == "packages apply --manager pacman --yes")
    );
}

#[test]
#[ignore = "P-06"]
fn unchanged_mixed_hold_and_pin_preserves_the_manager_state() {
    let fixture = Fixture::new();
    fixture.recipe("held-package", "#!/bin/sh\nexit 0\n");
    fixture.write("dots/exceptions.tsv", "base\theld-package\thold=apt:held\n");
    fixture.write("packages.json", r#"{"apt":{"available":true,"packages":[
        {"package":"held","requested_version":"latest","desired_state":"present","state":"installed"},
        {"package":"pinned","requested_version":"2","desired_state":"present","state":"missing"}
    ]}}"#);
    let report = fixture.run(1);
    assert!(!fixture.calls().contains("packages apply"));
    assert!(!fixture.calls().contains("packages upgrade"));
    assert!(fixture.calls().contains("install tools"));
    assert!(fixture.calls().contains("self-update"));
    if let Some(stages) = report["stages"].as_array() {
        assert!(stages.iter().any(|stage| stage["label"] == "apt packages"
            && stage["issues"].as_array().unwrap().iter().any(|issue| {
                issue["diagnostic"]
                    .as_str()
                    .unwrap()
                    .contains("cannot preserve configured versions")
            })));
    }
}

#[test]
#[ignore = "P-06"]
fn new_pre_checkpoint_failure_stops_before_sync_and_updates() {
    let fixture = Fixture::new();
    fixture.write("fail-checkpoint", "");
    let report = fixture.run(1);
    let stages = report["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 1, "{stages:?}");
    assert_eq!(stages[0]["label"], "Pre-update settings checkpoint");
    assert_eq!(stages[0]["outcome"], "failed");
    assert_eq!(fixture.calls(), "checkpoint inventory\n");
}

#[test]
#[ignore = "P-08"]
fn new_local_only_update_skips_sync_cleanly_and_checks_history_twice() {
    let fixture = Fixture::new();
    let report = fixture.run_args(&["--plain", "update"], 0);
    let stages = report["stages"].as_array().unwrap();
    assert_eq!(stages[0]["label"], "Pre-update settings checkpoint");
    assert_eq!(stages[0]["outcome"], "skipped");
    assert_eq!(stages[1]["outcome"], "skipped");
    assert!(stages[1]["label"].as_str().unwrap().contains("no remote"));
    assert!(
        stages
            .iter()
            .all(|stage| stage["issues"].as_array().unwrap().is_empty())
    );
    assert_eq!(
        stages.last().unwrap()["label"],
        "Post-update settings checkpoint"
    );
    assert_eq!(stages.last().unwrap()["outcome"], "skipped");
    assert_eq!(fixture.calls().matches("checkpoint inventory").count(), 2);
    assert!(!fixture.calls().contains("checkpoint save"));
}
