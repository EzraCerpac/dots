//! CLI scenarios ported from v0.4.0's CommandBackendTests plus the saved plan
//! report. The R-01 completion engine's tests/completion.rs
//! (new_completion_contract_grammar) covers the "dots reports pl" candidate
//! and both help flags on "dots completions".
mod support;

use serde_json::Value;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use support::World;

const PLAN_OUTPUT: &str = "fixture plan output: preview editor settings\n";

fn executable(path: &Path, script: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn dots(world: &World) -> Command {
    let tools = world.home().join("fixture-tools");
    let temporary = world.home().join("tmp");
    fs::create_dir_all(&temporary).unwrap();
    executable(
        &tools.join("mise"),
        include_str!("fixtures/contract_reports/mise.sh"),
    );
    executable(
        &tools.join("gitleaks"),
        include_str!("fixtures/contract_reports/gitleaks.sh"),
    );
    let path = env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let path =
        env::join_paths(std::iter::once(tools.clone()).chain(env::split_paths(&path))).unwrap();
    let mut command = world.dots();
    command
        .env("PATH", path)
        .env("TMPDIR", temporary)
        .env("DOTS_MISE_BIN", tools.join("mise"))
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SETUP_PROFILE", "laptop")
        .env("SETUP_MACHINE_ID", "fixture");
    command
}

fn assert_code(output: &Output, code: i32) {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn state(world: &World) -> PathBuf {
    world.home().join(".local/state/dots")
}

// The state location and content are observable. Apart from locating and
// identifying the completed report, inspect only the fields a user can see.
fn saved_report(world: &World, command: &str) -> Value {
    let state = state(world);
    let reports = state.join("reports");
    let paths: Vec<_> = if reports.is_dir() {
        fs::read_dir(reports)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    } else {
        // The v0.4.0 renderer saves one completed report in each run directory.
        fs::read_dir(state.join(command))
            .unwrap()
            .map(|entry| entry.unwrap().path().join("report.json"))
            .collect()
    };
    let mut matches = paths.into_iter().filter_map(|path| {
        let contents = fs::read(&path).unwrap();
        let report: Value = serde_json::from_slice(&contents).unwrap();
        (report["command"] == command).then_some((path, report))
    });
    let (path, report) = matches.next().expect("missing completed report");
    assert!(matches.next().is_none(), "more than one {command} report");
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    report
}

fn assert_report_status(report: &Value, code: u64) {
    assert_eq!(
        report["status"]
            .as_u64()
            .or_else(|| report["exit_code"].as_u64()),
        Some(code),
        "{report}"
    );
}

fn git(world: &World, args: &[&str]) -> String {
    let output = world
        .git()
        .env("GIT_ALLOW_PROTOCOL", "file")
        .args(args)
        .output()
        .unwrap();
    assert_code(&output, 0);
    text(&output.stdout).trim().to_owned()
}

fn sync_checkout(world: &World) -> PathBuf {
    fs::write(
        world.root().join("config.toml"),
        "[vars]\ndots_profiles = \"laptop server\"\ndots_source_repo = \"fixture/config\"\n",
    )
    .unwrap();
    fs::write(
        world.root().join(".gitignore"),
        "/miserc.toml\n/config.local.toml\n/config.host-*.toml\n/mise.lock\n/mise.*.lock\n/.setup-state/\n/locks/\n",
    )
    .unwrap();
    let editor = world.root().join("editor.ini");
    let origin = world.bare_remote("origin");
    let url = "https://github.com/fixture/config";
    git(world, &["init", "-q", "-b", "main"]);
    git(world, &["add", "."]);
    git(world, &["commit", "-qm", "base"]);
    git(world, &["remote", "add", "origin", url]);
    git(
        world,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.display()),
            url,
        ],
    );
    git(world, &["switch", "-qc", "dev"]);
    git(world, &["push", "-q", "origin", "main", "dev"]);
    editor
}

#[test]
#[ignore = "P-08"]
fn unchanged_standalone_sync_outside_git_reports_the_problem_and_exits_zero() {
    let world = World::new();
    let output = dots(&world).arg("sync").output().unwrap();
    assert_code(&output, 0);
    let receipt = text(&output.stdout).to_lowercase();
    assert_no_repository(&receipt);

    let report = saved_report(&world, "sync");
    assert_report_status(&report, 0);
    let metadata = report.to_string().to_lowercase();
    assert_no_repository(&metadata);
    assert!(metadata.contains("dots sync"), "{metadata}");
}

fn assert_no_repository(message: &str) {
    assert!(
        [
            "not a git repository",
            "not in a git repository",
            "no git repository"
        ]
        .iter()
        .any(|diagnostic| message.contains(diagnostic)),
        "{message}"
    );
}

#[test]
#[ignore = "P-08"]
fn unchanged_piped_sync_replays_metadata_without_syncing() {
    let world = World::new();
    let editor = sync_checkout(&world);
    fs::write(&editor, "tab_size = 4\n").unwrap();

    let run = dots(&world).arg("sync").output().unwrap();
    assert_code(&run, 0);
    let receipt = text(&run.stdout).to_lowercase();
    assert!(
        receipt.contains("shared") && receipt.contains("dev"),
        "{receipt}"
    );
    assert!(receipt.contains("editor.ini"), "{receipt}");
    assert!(!receipt.contains("tab_size"), "{receipt}");
    let head = git(&world, &["rev-parse", "HEAD"]);
    assert_eq!(git(&world, &["show", "HEAD:editor.ini"]), "tab_size = 4");

    fs::write(&editor, "tab_size = 8\n").unwrap();
    let mise_was_run = world.home().join("mise-called-on-replay");
    let replay = dots(&world)
        .args(["sync", "--last"])
        .env("FORBID_MISE", &mise_was_run)
        .env("GIT_ALLOW_PROTOCOL", "")
        .output()
        .unwrap();
    assert_code(&replay, 0);
    let receipt = text(&replay.stdout);
    assert!(
        receipt.contains("editor.ini") && !receipt.contains("tab_size"),
        "{receipt}"
    );
    assert!(!mise_was_run.exists());
    assert_eq!(git(&world, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&world, &["status", "--porcelain"]), "M editor.ini");
    assert_eq!(fs::read_to_string(&editor).unwrap(), "tab_size = 8\n");

    git(&world, &["restore", "editor.ini"]);
    let clean = dots(&world).arg("sync").output().unwrap();
    assert_code(&clean, 0);
    let receipt = text(&clean.stdout).to_lowercase();
    assert_eq!(receipt.trim().lines().count(), 1, "{receipt}");
    assert!(receipt.contains("up to date"), "{receipt}");
    let replay = dots(&world).args(["sync", "--last"]).output().unwrap();
    assert_code(&replay, 0);
    let receipt = text(&replay.stdout).to_lowercase();
    assert!(
        receipt.contains("up to date") && !receipt.contains("editor.ini"),
        "{receipt}"
    );
}

#[test]
#[ignore = "P-02"]
fn unchanged_status_prints_one_profile_diagnostic_and_saves_it() {
    let world = World::new();
    let output = dots(&world)
        .arg("status")
        .env("SETUP_PROFILE", "unknown")
        .output()
        .unwrap();
    assert_code(&output, 2);
    let diagnostic = text(&output.stderr).trim();
    assert_eq!(diagnostic.lines().count(), 1, "{output:?}");
    assert!(
        diagnostic.contains("dots_profiles") && diagnostic.contains("laptop"),
        "{output:?}"
    );
    let report = saved_report(&world, "status");
    assert_report_status(&report, 2);
    let metadata = report.to_string();
    assert!(
        metadata.contains(diagnostic) && metadata.contains("dots status"),
        "{metadata}"
    );
}

#[test]
#[ignore = "P-02"]
fn unchanged_failed_apply_report_names_the_target_to_rerun() {
    let world = World::new();
    let output = dots(&world)
        .args(["apply", "missing-target"])
        .output()
        .unwrap();
    assert_code(&output, 1);
    assert_eq!(output.stderr, b"no such target\n");
    let report = saved_report(&world, "apply");
    assert_report_status(&report, 1);
    let metadata = report.to_string();
    assert!(metadata.contains("dots apply missing-target"), "{metadata}");
    assert!(!metadata.contains("no such target"), "{metadata}");
}

#[test]
#[ignore = "P-02"]
fn new_plan_prints_the_preview_and_saves_private_stage_metadata() {
    let world = World::new();
    let config = world.root().join("config.toml");
    let before = fs::read(&config).unwrap();
    let output = dots(&world)
        .arg("plan")
        .env("EXPECT_PLAN_SELECTOR", "laptop,host-fixture")
        .output()
        .unwrap();
    assert_code(&output, 0);
    assert_eq!(output.stdout, PLAN_OUTPUT.as_bytes(), "{output:?}");
    assert_eq!(fs::read(&config).unwrap(), before);
    assert!(!world.home().join(".config/example/settings.toml").exists());

    let report = saved_report(&world, "plan");
    assert_report_status(&report, 0);
    assert!(
        report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["outcome"] == "completed"),
        "{report}"
    );
    assert!(!report.to_string().contains(PLAN_OUTPUT.trim()), "{report}");
}
