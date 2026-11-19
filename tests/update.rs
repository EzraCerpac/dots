mod support;

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use nix::libc;
use serde_json::Value;
use support::{FixtureCommand, World};

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
        let caller_path = std::env::var_os("PATH").unwrap_or_default();
        // node and jq are only needed by the v0.4.0 executable.
        for tool in ["git", "node", "jq"] {
            if let Some(path) = std::env::split_paths(&caller_path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(tool))
                .find(|path| {
                    path.is_file() && path.metadata().unwrap().permissions().mode() & 0o111 != 0
                })
            {
                symlink(path, fixture.bin.join(tool)).unwrap();
            }
        }
        fixture
    }

    fn write(&self, path: &str, data: &str) {
        fs::write(self.world.root().join(path), data).unwrap();
    }

    fn executable(&self, path: &Path, data: &str) {
        fs::write(path, data).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn command(&self) -> FixtureCommand<'_> {
        let mut command = self.world.dots();
        let paths = [self.bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")];
        let home = self.world.home();
        for (key, dir) in [
            ("CODEX_HOME", ".codex"),
            ("CLAUDE_CONFIG_DIR", ".claude"),
            ("MISE_DATA_DIR", "mise/data"),
            ("MISE_STATE_DIR", "mise/state"),
            ("MISE_CACHE_DIR", "mise/cache"),
            ("MISE_TMP_DIR", "mise/tmp"),
            ("MISE_SYSTEM_CONFIG_DIR", "mise/system"),
            ("TMPDIR", "tmp"),
        ] {
            let dir = home.join(dir);
            fs::create_dir_all(&dir).unwrap();
            command.env(key, dir);
        }
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("SETUP_PROFILE", "laptop")
            .env("SETUP_MACHINE_ID", "fixture")
            .env("DOTS_MISE_BIN", self.bin.join("mise"))
            .env("SETUP_MISE_BIN", self.bin.join("mise"))
            .env("MISE_CONFIG_DIR", self.world.root())
            .env("MISE_GLOBAL_CONFIG_FILE", home.join("mise/global.toml"))
            .env("MISE_TRUSTED_CONFIG_PATHS", self.world.root())
            .env("DOTS_UP_STATE_DIR", self.world.home().join("old-reports"))
            .env("MISE_SELF_UPDATE_AVAILABLE", "true");
        command
    }

    fn run(&self, expected: i32) -> Value {
        self.run_args(&["--plain", "up"], expected)
    }

    fn run_args(&self, args: &[&str], expected: i32) -> Value {
        self.run_command(self.command().args(args), expected)
    }

    fn run_command(&self, command: &mut FixtureCommand<'_>, expected: i32) -> Value {
        let result = command.output().unwrap();
        assert_exit(&result, expected);
        self.read_report()
    }

    fn read_report(&self) -> Value {
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

#[cfg(target_os = "macos")]
const BREW_RECIPE: &str = r#"#!/bin/bash
set -euo pipefail
if [ -n "${DOTS_HOME:-}" ]; then
    source "$DOTS_HOME/lib/brew-exception.sh"
    "update_brew_${KIND}_exception" "$TOKEN"
else
    dots brew "update-$KIND" "$TOKEN"
fi
"#;

#[cfg(target_os = "macos")]
fn brew_up(fixture: &Fixture, kind: &str, expected: i32) -> Value {
    fixture.write("fail-version", "");
    fixture.run_command(
        fixture
            .command()
            .env("KIND", kind)
            .env("TOKEN", "fixture")
            .args(["--plain", "up"]),
        expected,
    )
}

#[test]
#[cfg(target_os = "macos")]
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
fn unchanged_brew_recipe_without_a_change_sink_can_install() {
    let fixture = Fixture::new();
    fixture.executable(
        &fixture.world.root().join("dots/commands/fixture-brew"),
        BREW_RECIPE,
    );
    let output = fixture
        .command()
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
fn unchanged_standalone_mise_snapshot_uses_its_own_version() {
    let fixture = Fixture::new();
    assert_eq!(
        changes(&fixture.run(0)),
        [("mise", "2026.11.8 macos-x64", "2026.11.9 macos-x64")]
    );
}

#[test]
fn unchanged_unavailable_standalone_version_has_no_false_change() {
    let fixture = Fixture::new();
    fixture.write("fail-version", "");
    assert!(changes(&fixture.run(0)).is_empty());
    assert!(fixture.calls().contains("self-update"));
}

#[test]
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
fn unchanged_package_apply_keeps_an_explicit_absent_only_scope() {
    let fixture = Fixture::new();
    fixture.write("config.toml", "[vars]\ndots_profiles = 'laptop'\n[bootstrap.packages]\n'pacman:retired' = { state = 'absent' }\n");
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
fn unchanged_mixed_hold_and_pin_preserves_the_manager_state() {
    let fixture = Fixture::new();
    fixture.write("config.toml", "[vars]\ndots_profiles = 'laptop'\n[bootstrap.packages]\n'apt:held' = 'latest'\n'apt:pinned' = '2'\n");
    fixture.recipe("held-package", "#!/bin/sh\nexit 0\n");
    fixture.write("dots/exceptions.tsv", "base\theld-package\thold=apt:held\n");
    fixture.write("packages.json", r#"{"apt":{"available":true,"packages":[
        {"package":"held","requested_version":"latest","desired_state":"present","state":"installed","installed_version":"1"},
        {"package":"pinned","requested_version":"2","desired_state":"present","state":"missing","installed_version":""}
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

fn sync_fixture_git(fixture: &Fixture, args: &[&str]) -> String {
    let output = fixture
        .world
        .git()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .args(args)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn seed_sync_fixture(fixture: &Fixture) -> String {
    fixture.write(
        ".gitignore",
        concat!(
            "/config.local.toml\n/miserc.toml\n/config.host-*.toml\n",
            "/locks/dots-sync\n/.setup-state/dots-sync\n/calls\n/installed\n/updated\n",
        ),
    );
    sync_fixture_git(fixture, &["init", "--quiet", "--initial-branch=dev"]);
    sync_fixture_git(fixture, &["add", "."]);
    sync_fixture_git(fixture, &["commit", "--quiet", "-m", "Seed fixture"]);
    sync_fixture_git(fixture, &["rev-parse", "HEAD"])
}

#[test]
fn new_local_only_update_skips_sync_cleanly_and_checks_history_twice() {
    let fixture = Fixture::new();
    let seed = seed_sync_fixture(&fixture);
    assert_eq!(
        sync_fixture_git(&fixture, &["branch", "--show-current"]),
        "dev"
    );
    assert!(sync_fixture_git(&fixture, &["remote"]).is_empty());
    let output = fixture
        .command()
        .args(["--plain", "update"])
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        text.lines()
            .filter(|line| line.to_ascii_lowercase().contains("no remote configured"))
            .count(),
        1,
        "{text}"
    );
    assert!(!text.contains("Needs attention"), "{text}");
    let report = fixture.read_report();
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
    let calls = fixture.calls();
    assert_eq!(calls.matches("checkpoint inventory").count(), 2);
    assert_eq!(calls.matches("checkpoint local inventory").count(), 2);
    assert!(!calls.contains("checkpoint save"));
    assert_eq!(sync_fixture_git(&fixture, &["rev-parse", "HEAD"]), seed);
}

#[cfg(target_os = "macos")]
fn store_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write(
        "config.toml",
        "[vars]\ndots_profiles = 'laptop'\n[bootstrap.packages]\n'mas:42' = 'latest'\n",
    );
    fixture.write(
        "packages.json",
        r#"{"mas":{"available":true,"packages":[
        {"package":"42","requested_version":"latest","desired_state":"present",
         "state":"installed","installed_version":"1"}
    ]}}"#,
    );
    fixture.executable(
        &fixture.bin.join("sudo"),
        r#"#!/bin/sh
case "$*" in
    "-n -v"|"-n true") ;;
    *) touch "$DOTS_ROOT/authorization-prompt"; exit 74 ;;
esac
printf 'cached authorization\n' >> "$DOTS_ROOT/calls"
if [ -f "$DOTS_ROOT/expire-authorization" ]; then
    [ ! -f "$DOTS_ROOT/authorization-used" ] || exit 1
    touch "$DOTS_ROOT/authorization-used"
fi
"#,
    );
    fixture.executable(&fixture.bin.join("mas"), "#!/bin/sh\nexit 0\n");
    fixture
}

#[test]
#[cfg(target_os = "macos")]
fn new_app_store_overlaps_tools_and_joins_before_standalone_mise() {
    let fixture = store_fixture();
    fixture.write("expect-store-overlap", "");
    let report = fixture.run_args(&["--plain", "up", "--mas"], 0);
    let stages = report["stages"].as_array().unwrap();
    let store = stages
        .iter()
        .find(|s| s["label"] == "Mac App Store updates")
        .unwrap();
    assert_eq!(store["alongside"], true);
    assert_eq!(store["outcome"], "completed");
    let calls = fixture.calls();
    assert!(
        calls.find("packages upgrade --manager mas").unwrap() < calls.find("self-update").unwrap()
    );
    assert!(!fixture.world.root().join("authorization-prompt").exists());
}

#[test]
#[cfg(target_os = "macos")]
fn new_app_store_defers_expired_authorization_without_reprompting() {
    let fixture = store_fixture();
    fixture.write("expire-authorization", "");
    let report = fixture.run_args(&["--plain", "up", "--mas"], 0);
    let stages = report["stages"].as_array().unwrap();
    let store = stages
        .iter()
        .find(|s| s["label"] == "Mac App Store updates")
        .unwrap();
    assert_eq!(store["outcome"], "deferred");
    assert!(
        store["issues"][0]["next"]
            .as_str()
            .unwrap()
            .contains("sudo -v")
    );
    assert!(!fixture.calls().contains("packages apply --manager mas"));
    assert!(fixture.calls().contains("self-update"));
    assert!(!fixture.world.root().join("authorization-prompt").exists());
}

#[test]
fn unchanged_failed_manager_does_not_block_healthy_packages() {
    let fixture = Fixture::new();
    fixture.write("config.toml", "[vars]\ndots_profiles = 'laptop'\n[bootstrap.packages]\n'apt:bad' = 'latest'\n'pacman:good' = 'latest'\n");
    fixture.write(
        "packages.json",
        r#"{
        "apt":{"available":true,"packages":[
            {"package":"bad","requested_version":"latest","desired_state":"present",
             "state":"missing","installed_version":""}]},
        "pacman":{"available":true,"packages":[
            {"package":"good","requested_version":"latest","desired_state":"present",
             "state":"missing","installed_version":""}]}
    }"#,
    );
    fixture.write("fail-apt-status", "");
    fixture.run(1);
    let calls = fixture.calls();
    assert!(calls.contains("packages apply --manager pacman --yes"));
    assert!(!calls.contains("packages apply --manager apt"));
    assert!(calls.contains("install tools"));
}

#[test]
fn new_unresolved_bare_blocks_filtering_but_keeps_unfiltered_managers() {
    let fixture = Fixture::new();
    fixture.write("config.toml", "[vars]\ndots_profiles = 'laptop'\n[bootstrap.packages]\n'bare' = 'latest'\n'apt:held' = 'latest'\n'pacman:good' = 'latest'\n");
    fixture.recipe("held-package", "#!/bin/sh\nexit 0\n");
    fixture.write("dots/exceptions.tsv", "base\theld-package\thold=apt:held\n");
    fixture.write(
        "packages.json",
        r#"{
        "apt":{"available":true,"packages":[
            {"package":"held","requested_version":"latest","desired_state":"present",
             "state":"installed","installed_version":"1"}]},
        "unprefixed":{"available":false,"packages":[
            {"package":"bare","requested_version":"latest","desired_state":"present",
             "state":"skipped","installed_version":""}]},
        "pacman":{"available":true,"packages":[
            {"package":"good","requested_version":"latest","desired_state":"present",
             "state":"missing","installed_version":""}]}
    }"#,
    );
    fixture.write(
        "apt-packages.json",
        r#"{"apt":{"available":true,"packages":[
        {"package":"held","requested_version":"latest","desired_state":"present",
         "state":"installed","installed_version":"1"},
        {"package":"bare","requested_version":"latest","desired_state":"present",
         "state":"missing","installed_version":""}
    ]}}"#,
    );
    fixture.write("fail-bare-assignment", "");
    let report = fixture.run(1);
    let calls = fixture.calls();
    assert!(!calls.contains("packages apply --manager apt"));
    assert!(calls.contains("packages apply --manager pacman --yes"));
    let scopes = fs::read_to_string(fixture.world.root().join("package-scopes")).unwrap();
    assert!(
        scopes.lines().any(|line| {
            let (manager, scope) = line.split_once('\t').unwrap();
            manager == "pacman" && scope.split(',').any(|candidate| candidate == "apt")
        }),
        "{scopes}"
    );
    assert!(report["stages"].as_array().unwrap().iter().any(|stage| {
        stage["label"] == "apt packages"
            && stage["issues"].as_array().unwrap().iter().any(|issue| {
                issue["diagnostic"]
                    .as_str()
                    .unwrap()
                    .contains("unqualified package names")
            })
    }));
}

#[test]
#[cfg(target_os = "macos")]
fn unchanged_app_store_tool_preparation_replaces_a_stale_shim_in_scope() {
    let fixture = store_fixture();
    let shim = fixture.bin.join("mas");
    fs::remove_file(&shim).unwrap();
    symlink(fixture.bin.join("mise"), &shim).unwrap();
    fixture.write("prepare-brew-mas", "");
    fixture.run_args(&["--plain", "up", "--mas"], 0);
    let calls = fixture.calls();
    let authorization = calls.find("cached authorization").unwrap();
    let install = calls
        .find("packages apply --manager brew --yes brew:mas")
        .unwrap();
    let upgrade = calls
        .find("packages upgrade --manager brew --yes brew:mas")
        .unwrap();
    let store = calls.find("packages apply --manager mas --yes").unwrap();
    assert!(
        authorization < install && install < upgrade && upgrade < store,
        "{calls}"
    );
    assert!(fs::symlink_metadata(shim).unwrap().file_type().is_symlink());
    assert!(!fixture.world.root().join("authorization-prompt").exists());
}

#[test]
#[cfg(target_os = "macos")]
fn unchanged_app_store_with_missing_tool_and_x64_mise_defers() {
    let fixture = store_fixture();
    fs::remove_file(fixture.bin.join("mas")).unwrap();
    fixture.run_args(&["--plain", "up", "--mas"], 0);
    let calls = fixture.calls();
    assert!(!calls.contains("cached authorization"));
    assert!(!calls.contains("packages apply --manager brew"));
    assert!(!calls.contains("packages apply --manager mas"));
    assert!(calls.contains("install tools"));
}

#[test]
fn new_manager_discovery_reuses_the_supervised_configuration_listing() {
    let fixture = Fixture::new();
    fixture.write(
        "config.toml",
        "[vars]\ndots_profiles = 'laptop'\n[tools]\n'github:EzraCerpac/dots' = 'latest'\n",
    );
    fixture.run(0);
    let calls = fixture.calls();
    let (before_install, _) = calls.split_once("install tools").unwrap();
    assert_eq!(before_install.matches("configuration listing").count(), 1);
    assert!(before_install.contains("manager lookup"));
}

#[test]
fn new_cancel_reaps_the_configuration_probe_and_its_child() {
    let fixture = Fixture::new();
    fixture.write("block-config-lookup", "");
    let report = cancel_blocked_update(&fixture, &["--plain", "up"], "config-ready", false);
    let last = report["stages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["label"], "Reload configuration");
    assert_eq!(last["outcome"], "cancelled");
    assert!(!fixture.calls().contains("install tools"));
}

#[test]
#[cfg(target_os = "macos")]
fn new_held_app_store_work_skips_authorization_and_tool_preparation() {
    for tool_missing in [false, true] {
        let fixture = store_fixture();
        fixture.recipe("held-app", "#!/bin/sh\nexit 0\n");
        fixture.write("dots/exceptions.tsv", "base\theld-app\thold=mas:42\n");
        if tool_missing {
            fs::remove_file(fixture.bin.join("mas")).unwrap();
            fixture.write("prepare-brew-mas", "");
        }
        fixture.run_args(&["--plain", "up", "--mas"], 0);
        let calls = fixture.calls();
        assert!(!calls.contains("cached authorization"), "{calls}");
        assert!(!calls.contains("packages apply --manager"), "{calls}");
        assert!(!fixture.world.root().join("authorization-prompt").exists());
        assert!(calls.contains("install tools"), "{calls}");
    }
}

fn declared_fixture_package(name: &str) -> Value {
    serde_json::json!({
        "package": name,
        "requested_version": "latest",
        "desired_state": "present",
        "state": "missing",
        "installed_version": ""
    })
}

#[test]
fn new_filtered_bare_cask_fails_before_discarding_its_options() {
    let fixture = Fixture::new();
    fixture.write(
        "config.toml",
        "[vars]\ndots_profiles = 'laptop'\n[bootstrap.packages]\n'brew-cask:held' = 'latest'\n         browser = { appdir = '~/fixture-apps', adopt = true }\n",
    );
    fixture.recipe("held-cask", "#!/bin/sh\nexit 0\n");
    fixture.write(
        "dots/exceptions.tsv",
        "base\theld-cask\thold=brew-cask:held\n",
    );
    fixture.write(
        "packages.json",
        &serde_json::json!({
            "brew-cask": {"available": true, "packages": [declared_fixture_package("held")]},
            "unprefixed": {"available": false, "packages": [declared_fixture_package("browser")]}
        })
        .to_string(),
    );
    fixture.write(
        "resolved-packages.json",
        &serde_json::json!({
            "brew-cask": {
                "available": true,
                "packages": [declared_fixture_package("held"), declared_fixture_package("browser")]
            }
        })
        .to_string(),
    );
    let report = fixture.run(1);
    assert!(
        !fixture
            .calls()
            .contains("packages apply --manager brew-cask")
    );
    assert!(
        !fixture
            .calls()
            .contains("packages upgrade --manager brew-cask")
    );
    assert!(fixture.calls().contains("install tools"));
    let stages = report["stages"].as_array().unwrap();
    let issue = stages
        .iter()
        .find(|stage| stage["label"] == "brew-cask packages")
        .unwrap()["issues"][0]["diagnostic"]
        .as_str()
        .unwrap();
    assert!(issue.contains("brew-cask:browser") && issue.contains("appdir or adopt"));
}

#[test]
fn new_qualified_cask_pass_cannot_reinstall_an_excluded_bare_dependency() {
    for held in [false, true] {
        let fixture = Fixture::new();
        fixture.write(
            "config.toml",
            "[vars]\ndots_profiles = 'laptop'\n[bootstrap.packages]\n'brew-cask:parent' = 'latest'\n         'helper' = { appdir = '~/fixture-apps', adopt = true }\n",
        );
        if held {
            fixture.recipe("held-helper", "#!/bin/sh\nexit 0\n");
            fixture.write(
                "dots/exceptions.tsv",
                "base\theld-helper\thold=brew-cask:helper\n",
            );
        }
        fixture.write(
            "packages.json",
            &serde_json::json!({
                "brew-cask": {"available": true, "packages": [declared_fixture_package("parent")]},
                "unprefixed": {"available": false, "packages": [declared_fixture_package("helper")]}
            })
            .to_string(),
        );
        fixture.write(
            "resolved-packages.json",
            &serde_json::json!({
                "brew-cask": {
                    "available": true,
                    "packages": [declared_fixture_package("parent"), declared_fixture_package("helper")]
                }
            })
            .to_string(),
        );
        let report = fixture.run(i32::from(held));
        let calls = fixture.calls();
        assert!(calls.contains("install tools"), "{calls}");
        if held {
            assert!(
                !calls.contains("packages apply --manager brew-cask"),
                "{calls}"
            );
            assert!(
                !calls.contains("packages upgrade --manager brew-cask"),
                "{calls}"
            );
            let stages = report["stages"].as_array().unwrap();
            let stage = stages
                .iter()
                .find(|stage| stage["label"] == "brew-cask packages")
                .unwrap();
            assert_eq!(stage["outcome"], "failed");
            let diagnostic = stage["issues"][0]["diagnostic"].as_str().unwrap();
            assert!(diagnostic.contains("held brew-cask:helper as a dependency"));
            assert!(diagnostic.contains("appdir or adopt"));
        } else {
            assert!(
                calls.contains("packages apply --manager brew-cask --yes\n"),
                "{calls}"
            );
        }
    }
}

#[test]
fn new_failed_grouped_status_skips_unconfirmed_bare_only_managers() {
    let fixture = Fixture::new();
    fixture.write(
        "config.toml",
        "[vars]\ndots_profiles = 'laptop'\n[bootstrap.packages]\n'pacman:good' = 'latest'\n         bare = 'latest'\n",
    );
    fixture.write(
        "package-settings.json",
        r#"{"system_packages":{"prefer":["pacman","apt"]}}"#,
    );
    fixture.write(
        "packages.json",
        &serde_json::json!({
            "pacman": {"available": true, "packages": [declared_fixture_package("good")]},
            "unprefixed": {"available": false, "packages": [declared_fixture_package("bare")]}
        })
        .to_string(),
    );
    fixture.write(
        "apt-packages.json",
        &serde_json::json!({
            "apt": {"available": true, "packages": [declared_fixture_package("bare")]}
        })
        .to_string(),
    );
    fixture.write("fail-bare-assignment", "");
    let report = fixture.run(0);
    let calls = fixture.calls();
    assert!(
        calls.contains("packages apply --manager pacman --yes"),
        "{calls}"
    );
    assert!(!calls.contains("packages apply --manager apt"), "{calls}");
    let probes = fs::read_to_string(fixture.world.root().join("package-status-scopes")).unwrap();
    assert!(
        probes.lines().any(|scope| scope == "apt,pacman"),
        "{probes}"
    );
    assert!(!probes.lines().any(|scope| scope == "apt"), "{probes}");
    assert!(report["stages"].as_array().unwrap().iter().any(|stage| {
        stage["label"] == "Resolve unqualified packages" && stage["outcome"] == "deferred"
    }));
}

const UPDATE_POLL: Duration = Duration::from_millis(10);

struct RunningUpdate {
    child: Child,
    release: PathBuf,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl RunningUpdate {
    fn spawn(fixture: &Fixture, command: &mut FixtureCommand<'_>) -> Self {
        let home = fixture.world.home();
        let release = home.join("release-update");
        let stdout = home.join("update.stdout");
        let stderr = home.join("update.stderr");
        let child = command
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            release,
            stdout,
            stderr,
        }
    }

    fn streams(&self) -> (Vec<u8>, Vec<u8>) {
        (
            fs::read(&self.stdout).unwrap(),
            fs::read(&self.stderr).unwrap(),
        )
    }

    fn diagnostics(&self) -> String {
        let (stdout, stderr) = self.streams();
        format!(
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )
    }
}

impl Drop for RunningUpdate {
    fn drop(&mut self) {
        // All blocking fixture workers, including the detached one, observe
        // this file. Let dots reap them before forcing the direct child down.
        let _ = fs::write(&self.release, "");
        if matches!(
            wait_update_exit(&mut self.child, Duration::from_secs(1)),
            Ok(None)
        ) {
            let _ = self.child.kill();
            let _ = wait_update_exit(&mut self.child, Duration::from_secs(1));
        }
    }
}

fn wait_update_exit(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        let exit = child.try_wait()?;
        if exit.is_some() || Instant::now() >= deadline {
            return Ok(exit);
        }
        std::thread::sleep(UPDATE_POLL);
    }
}

fn interrupt_update_when_ready(
    child: &mut Child,
    marker: &Path,
    timeout: Duration,
    send: impl FnOnce(u32, &[libc::pid_t]) -> io::Result<()>,
) -> io::Result<Vec<libc::pid_t>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "update exited before readiness ({status})"
            )));
        }
        if let Ok(ready) = fs::read_to_string(marker)
            && ready.ends_with('\n')
            && let Ok(pids) = ready
                .split_whitespace()
                .map(str::parse::<libc::pid_t>)
                .collect::<Result<Vec<_>, _>>()
            && !pids.is_empty()
            && pids.iter().all(|pid| *pid > 1)
        {
            // An un-reaped direct child owns this PID even if it exits now.
            send(child.id(), &pids)?;
            return Ok(pids);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "update did not reach readiness",
            ));
        }
        std::thread::sleep(UPDATE_POLL);
    }
}

fn make_update_workers_releasable(fixture: &Fixture) {
    fixture.executable(
        &fixture.bin.join("block-update"),
        r#"#!/bin/sh
set -eu
attempts=0
while [ -d "$DOTS_ROOT" ] && [ ! -e "$HOME/release-update" ] && [ "$attempts" -lt 600 ]; do
    attempts=$((attempts + 1))
    /bin/sleep 0.05
done
"#,
    );
    let mise = include_str!("fixtures/up/mise");
    let fallback = "kill -TERM \"$helper\" 2>/dev/null || :; ";
    assert_eq!(mise.matches("/bin/sleep 30").count(), 3);
    assert_eq!(mise.matches(fallback).count(), 1);
    fixture.executable(
        &fixture.bin.join("mise"),
        &mise
            .replace("/bin/sleep 30", "\"$HOME/fixture-bin/block-update\"")
            .replace(fallback, ""),
    );
}

fn cancel_blocked_update(fixture: &Fixture, args: &[&str], marker: &str, detached: bool) -> Value {
    make_update_workers_releasable(fixture);
    let mut run = RunningUpdate::spawn(fixture, fixture.command().args(args));
    let marker = fixture.world.root().join(marker);
    let mut session = None;
    let ready = interrupt_update_when_ready(
        &mut run.child,
        &marker,
        Duration::from_secs(20),
        |pid, helpers| {
            if detached {
                session = Some(unsafe { libc::getsid(helpers[0]) });
            }
            if unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        },
    )
    .unwrap_or_else(|error| panic!("cancel at {marker:?}: {error}\n{}", run.diagnostics()));

    let status = wait_update_exit(&mut run.child, Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| panic!("update did not exit after SIGINT\n{}", run.diagnostics()));
    let (stdout, stderr) = run.streams();
    assert_exit(
        &Output {
            status,
            stdout,
            stderr,
        },
        130,
    );
    if detached {
        assert_eq!(
            session,
            Some(ready[0]),
            "worker must be its own session leader"
        );
    }
    for pid in ready {
        assert_eq!(
            unsafe { libc::getsid(pid) },
            -1,
            "fixture helper {pid} survived"
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    let report = fixture.read_report();
    assert_eq!(report["cancelled"], true);
    assert_eq!(fixture.calls().matches("checkpoint inventory").count(), 1);
    assert!(!fixture.calls().contains("self-update"));
    report
}

fn update_sync_evidence(fixture: &Fixture, expected_receipts: usize) -> Value {
    let output = fixture.command().args(["--plain", "up"]).output().unwrap();
    assert_exit(&output, 0);
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("dots sync: "))
            .count(),
        expected_receipts,
        "{text}"
    );
    fixture.read_report()
}

fn saved_sync_receipt(fixture: &Fixture) -> Option<Value> {
    let path = fixture.world.home().join(".local/state/dots/receipts");
    if !path.exists() {
        return None;
    }
    let mut files = fs::read_dir(path).unwrap();
    let first = files.next()?.unwrap();
    assert!(files.next().is_none());
    Some(
        serde_json::from_slice::<Value>(&fs::read(first.path()).unwrap()).unwrap()["receipt"]
            .clone(),
    )
}

#[test]
fn new_up_embedded_sync_local_only_does_not_emit_or_save_receipt() {
    let fixture = Fixture::new();
    seed_sync_fixture(&fixture);
    sync_fixture_git(&fixture, &["branch", "-m", "main"]);
    let report = update_sync_evidence(&fixture, 0);
    assert_eq!(report["stages"][1]["outcome"], "skipped");
    assert_eq!(report["stages"][1]["issues"], serde_json::json!([]));
    assert!(saved_sync_receipt(&fixture).is_none());
    assert_eq!(fixture.calls().matches("checkpoint inventory").count(), 2);
}

#[test]
fn new_up_embedded_sync_records_load_problem_once_and_continues() {
    let fixture = Fixture::new();
    fixture.write(
        "config.toml",
        "[vars]\ndots_profiles = 'laptop'\ndots_source_repo = 'example/unconfigured'\n",
    );
    seed_sync_fixture(&fixture);
    let report = update_sync_evidence(&fixture, 1);
    let receipt = saved_sync_receipt(&fixture).unwrap();
    let stage = &report["stages"][1];
    assert_eq!(stage["outcome"], receipt["stage_outcome"]);
    assert_eq!(stage["outcome"], "deferred");
    let issues = stage["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["diagnostic"], receipt["issues"][0]["diagnostic"]);
    assert_eq!(issues[0]["next"], receipt["issues"][0]["next_action"]);
    assert_eq!(stage["changes"], serde_json::json!([]));
    assert!(fixture.calls().contains("install tools"));
}

#[test]
fn new_up_embedded_sync_records_the_actual_adopted_revision_once() {
    let fixture = Fixture::new();
    let before = seed_sync_fixture(&fixture);
    let remote = format!("file://{}", fixture.world.bare_remote("origin").display());
    sync_fixture_git(&fixture, &["remote", "add", "origin", &remote]);
    sync_fixture_git(&fixture, &["push", "--quiet", "origin", "HEAD:dev"]);
    sync_fixture_git(&fixture, &["switch", "--quiet", "-c", "publisher"]);
    fixture.write("incoming.txt", "fixture from origin\n");
    sync_fixture_git(&fixture, &["add", "incoming.txt"]);
    sync_fixture_git(&fixture, &["commit", "--quiet", "-m", "Incoming fixture"]);
    let after = sync_fixture_git(&fixture, &["rev-parse", "HEAD"]);
    sync_fixture_git(&fixture, &["push", "--quiet", "origin", "HEAD:dev"]);
    sync_fixture_git(&fixture, &["switch", "--quiet", "dev"]);
    fs::rename(fixture.bin.join("mise"), fixture.bin.join("update-mise")).unwrap();
    fixture.executable(
        &fixture.bin.join("mise"),
        r#"#!/bin/sh
set -eu
if [ "${1-}" = -C ]; then
    checkout="${HOME%/*}/config"
    [ "$2" = "$(pwd -P)" ] && [ "$2" = "$DOTS_ROOT" ] && [ "$2" = "$MISE_CONFIG_DIR" ] || exit 75
    case "$2" in
        "$checkout"|"$checkout"/.git/dots-sync-*/validate-*) ;;
        *) exit 75 ;;
    esac
    case "${3-}:${4-}:${5-}:${6-}:${7-}:${8-}" in
        bootstrap:dotfiles:status:--json::)
            printf '{"files":[],"edits":[]}\n'
            exit 0 ;;
        config:ls:--json:::)
            if [ "$2" != "$checkout" ]; then
                printf '[{"path":"%s/config.toml"}]\n' "$2"
                exit 0
            fi ;;
        bootstrap:dotfiles:apply:--dry-run:--force:--yes)
            [ "$2" != "$checkout" ]
            exit ;;
    esac
    [ "$2" = "$checkout" ] || exit 75
fi
exec "${0%/*}/update-mise" "$@"
"#,
    );
    fixture.executable(
        &fixture.bin.join("gitleaks"),
        r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--report-path" ]; then
        shift
        case "$1" in "${HOME%/*}"/*) printf '[]' > "$1" ;; *) exit 75 ;; esac
        exit 0
    fi
    shift
done
exit 76
"#,
    );

    let report = update_sync_evidence(&fixture, 1);
    let receipt = saved_sync_receipt(&fixture).unwrap();
    assert_eq!(receipt["stage_outcome"], "completed", "{receipt}");
    let stage = &report["stages"][1];
    assert_eq!(stage["outcome"], receipt["stage_outcome"]);
    assert_eq!(stage["issues"], serde_json::json!([]));
    assert_eq!(
        receipt["adopted_revision"],
        serde_json::json!([&before, &after])
    );
    assert_eq!(
        stage["changes"],
        serde_json::json!([
            {"item":"configuration revision", "before":before, "after":after}
        ])
    );
    assert_eq!(sync_fixture_git(&fixture, &["rev-parse", "HEAD"]), after);
    assert_eq!(
        fs::read_to_string(fixture.world.root().join("incoming.txt")).unwrap(),
        "fixture from origin\n"
    );
}

#[test]
#[cfg(target_os = "macos")]
fn new_cancel_stops_a_blocking_ruby_lookup_before_updates() {
    let fixture = Fixture::new();
    fixture.write(
        "config.toml",
        "[vars]\ndots_profiles = 'laptop'\n[tools]\n'github:EzraCerpac/dots' = 'latest'\n",
    );
    fixture.write("miserc.toml", "env = ['laptop', 'host-fixture', 'extra']\n");
    seed_sync_fixture(&fixture);
    fixture.write("block-ruby-lookup", "");
    let report = cancel_blocked_update(&fixture, &["--plain", "up"], "ruby-ready", false);
    let last = report["stages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["label"], "Inspect declared packages");
    assert_eq!(last["outcome"], "cancelled");
    assert!(!fixture.calls().contains("install tools"));
}

#[test]
#[cfg(target_os = "macos")]
fn new_cancel_reaps_the_detached_app_store_worker_before_later_stages() {
    let fixture = store_fixture();
    seed_sync_fixture(&fixture);
    fixture.write("block-mas-worker", "");
    let report = cancel_blocked_update(&fixture, &["--plain", "up", "--mas"], "mas-ready", true);
    let store = report["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["label"] == "Mac App Store updates")
        .unwrap();
    assert_eq!(store["alongside"], true);
    assert_eq!(store["outcome"], "cancelled");
    assert!(!fixture.world.root().join("authorization-prompt").exists());
}
