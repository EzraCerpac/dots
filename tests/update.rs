mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

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
#[ignore = "P-06"]
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
#[ignore = "P-06, P-08"]
fn new_local_only_update_skips_sync_cleanly_and_checks_history_twice() {
    let fixture = Fixture::new();
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
    assert_eq!(fixture.calls().matches("checkpoint inventory").count(), 2);
    assert!(!fixture.calls().contains("checkpoint save"));
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
#[ignore = "P-06"]
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
#[ignore = "P-06"]
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
#[ignore = "P-06"]
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
#[ignore = "P-06"]
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
#[ignore = "P-06"]
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
#[ignore = "P-06"]
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
