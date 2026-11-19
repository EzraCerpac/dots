mod support;

use serde_json::Value;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use support::{FixtureCommand, World};

fn executable(path: &Path, script: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn dots(world: &World) -> FixtureCommand<'_> {
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
    git(world, &["init", "-q", "-b", "main"]);
    let origin = world.bare_remote("origin");
    let setting = world.set_source_origin(&origin, "fixture/config");
    fs::write(
        world.root().join("config.toml"),
        format!("[vars]\ndots_profiles = \"laptop server\"\ndots_source_repo = {setting:?}\n"),
    )
    .unwrap();
    fs::write(
        world.root().join(".gitignore"),
        "/miserc.toml\n/config.local.toml\n/config.host-*.toml\n/mise.lock\n/mise.*.lock\n/.setup-state/\n/locks/\n",
    )
    .unwrap();
    let editor = world.root().join("editor.ini");
    git(world, &["add", "."]);
    git(world, &["commit", "-qm", "base"]);
    git(world, &["switch", "-qc", "dev"]);
    git(world, &["push", "-q", "origin", "main", "dev"]);
    editor
}

#[test]
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
            "outside a git repository",
            "no git repository"
        ]
        .iter()
        .any(|diagnostic| message.contains(diagnostic)),
        "{message}"
    );
}

#[test]
fn new_valid_sync_reports_an_unusable_root_without_saving() {
    let world = World::new();
    let file = world.base().join("regular-file");
    fs::write(&file, "keep fixture file\n").unwrap();
    let blocked = file.join("configuration");
    let launch = |args: &[&str]| {
        world
            .dots()
            .env("DOTS_ROOT", &blocked)
            .args(args)
            .output()
            .unwrap()
    };
    for args in [&["sync"][..], &["--plain", "sync"]] {
        let output = launch(args);
        assert_code(&output, 0);
        let receipt = text(&output.stdout);
        assert!(receipt.contains("Could not start sync:"), "{receipt}");
        assert!(receipt.contains("regular-file/configuration"), "{receipt}");
        assert!(
            receipt.contains("DOTS_ROOT") && receipt.contains("rerun dots sync"),
            "{receipt}"
        );
        assert!(receipt.contains("not pushed"), "{receipt}");
        assert_eq!(
            receipt
                .lines()
                .filter(|line| line.starts_with("dots sync:"))
                .count(),
            1
        );
        assert_eq!(
            text(&output.stderr),
            "dots sync: could not save the sync receipt for replay\n"
        );
    }
    for args in [&["--plain", "sync", "unexpected"][..], &["status"]] {
        let output = launch(args);
        assert_code(&output, 2);
        assert!(output.stdout.is_empty(), "{output:?}");
    }
    assert!(!state(&world).join("receipts").exists());
    assert!(!state(&world).join("reports").exists());
    assert_eq!(fs::read_to_string(file).unwrap(), "keep fixture file\n");

    fs::write(
        world.root().join("config.toml"),
        "[vars]\ndots_profiles = 'laptop'\n",
    )
    .unwrap();
    git(&world, &["init", "-q", "-b", "dev"]);
    git(&world, &["add", "config.toml"]);
    git(&world, &["commit", "-qm", "Other checkout"]);
    let other = world.dots().arg("sync").output().unwrap();
    assert_code(&other, 0);
    assert!(text(&other.stdout).contains("local-only"), "{other:?}");
    let receipts = state(&world).join("receipts");
    let prior = fs::read_dir(&receipts)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let saved = fs::read(&prior).unwrap();

    let tools = world.home().join("replay-tools");
    for tool in ["git", "mise", "gitleaks", "less"] {
        executable(
            &tools.join(tool),
            "#!/bin/sh\nprintf 'unexpected child\n' >> \"$HOME/replay-child-ran\"\nexit 88\n",
        );
    }
    for args in [&["sync", "--last"][..], &["--plain", "sync", "--last"]] {
        let output = world
            .dots()
            .env("DOTS_ROOT", &blocked)
            .env("PATH", &tools)
            .env("DOTS_MISE_BIN", tools.join("mise"))
            .env("GIT_ALLOW_PROTOCOL", "")
            .args(args)
            .output()
            .unwrap();
        assert_code(&output, 0);
        assert_eq!(text(&output.stdout), "No sync receipt for this checkout.\n");
        assert!(output.stderr.is_empty(), "{output:?}");
    }
    assert!(!world.home().join("replay-child-ran").exists());
    assert_eq!(fs::read_dir(receipts).unwrap().count(), 1);
    assert_eq!(fs::read(prior).unwrap(), saved);
}

#[test]
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
fn new_reports_lists_saved_plan_and_apply_results() {
    let world = World::new();
    let reports = state(&world).join("reports");
    fs::create_dir_all(&reports).unwrap();
    fs::set_permissions(&reports, fs::Permissions::from_mode(0o700)).unwrap();
    for command in ["plan", "apply"] {
        let report = reports.join(format!("{command}.json"));
        fs::write(
            &report,
            serde_json::json!({
                "command": command, "finished_at_ms": 1, "status": 0, "next": null, "stages": []
            })
            .to_string(),
        )
        .unwrap();
        fs::set_permissions(&report, fs::Permissions::from_mode(0o600)).unwrap();
        let output = world.dots().args(["reports", command]).output().unwrap();
        assert_code(&output, 0);
        assert!(
            text(&output.stdout).contains(report.to_str().unwrap()),
            "{output:?}"
        );
    }
}
