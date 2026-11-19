mod support;

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{FixtureCommand, World};

fn minimal(world: &World, declarations: &str) {
    fs::write(
        world.root().join("config.toml"),
        format!("[vars]\ndots_profiles='laptop server'\n{declarations}"),
    )
    .unwrap();
    fs::remove_file(world.root().join("config.laptop.toml")).unwrap();
    fs::remove_file(world.root().join("dots/exceptions.tsv")).unwrap();
}

fn real_mise() -> PathBuf {
    mise_on_path(&env::var_os("PATH").unwrap_or_default()).expect("standalone mise on PATH")
}

fn mise_on_path(search: &OsStr) -> Option<PathBuf> {
    env::split_paths(search)
        .map(|directory| directory.join("mise"))
        .find(|path| {
            path.is_absolute()
                && fs::metadata(path).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
        })
}

fn with_mise(world: &World) -> FixtureCommand<'_> {
    let mut command = world.dots();
    let mise = real_mise();
    command
        .env("DOTS_MISE_BIN", &mise)
        .env("SETUP_MISE_BIN", mise)
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root())
        .env("SETUP_PROFILE", "laptop");
    command
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn output(command: &mut FixtureCommand<'_>, status: i32) -> Output {
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(status), "{output:?}");
    output
}

fn read_report(world: &World) -> serde_json::Value {
    let reports = world.home().join(".local/state/dots/reports");
    let entries = fs::read_dir(reports)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1);
    let path = entries[0].path();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn mise_lookup_skips_a_non_executable_path_entry() {
    let world = World::new();
    let blocked = world.home().join("blocked");
    let bin = world.home().join("bin");
    fs::create_dir(&blocked).unwrap();
    fs::create_dir(&bin).unwrap();
    let non_executable = blocked.join("mise");
    fs::write(&non_executable, "").unwrap();
    fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o600)).unwrap();
    let mise = bin.join("mise");
    script(&mise, "exit 0");
    let search = env::join_paths([blocked, bin]).unwrap();
    assert_eq!(mise_on_path(&search), Some(mise));
}

#[test]
fn new_status_prints_missing_programs_and_unreadable_versions_before_config_errors() {
    let world = World::new();
    fs::remove_file(world.root().join("config.toml")).unwrap();
    let path = world.home().join("bin");
    fs::create_dir(&path).unwrap();
    script(&path.join("gh"), "exit 1");
    let status = output(world.dots().env("PATH", path).arg("status"), 2);
    let stdout = String::from_utf8(status.stdout).unwrap();
    assert!(stdout.starts_with("Programs\n"), "{stdout}");
    assert!(
        stdout.lines().any(|line| line.contains("gitleaks")
            && line.contains("missing")
            && line.contains("sync")),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("gh") && line.contains("found, version unavailable")),
        "{stdout}"
    );
    assert!(!stdout.contains("Agent skill links"), "{stdout}");
    assert!(String::from_utf8_lossy(&status.stderr).contains("config.toml"));
    assert_eq!(read_report(&world)["status"], 2);
}

#[test]
fn new_status_keeps_an_unreadable_optional_version_informational() {
    let world = World::new();
    minimal(&world, "");
    let path = world.home().join("bin");
    fs::create_dir(&path).unwrap();
    script(&path.join("gh"), "exit 1");
    let search = env::join_paths(
        std::iter::once(path).chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut dots = with_mise(&world);
    let status = output(dots.env("PATH", search).arg("status"), 0);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("gh") && line.contains("found, version unavailable")),
        "{stdout}"
    );
    assert!(stdout.contains("local-only"), "{stdout}");
    assert_eq!(read_report(&world)["status"], 0);
}

#[test]
fn new_status_prints_version_and_links_before_an_unusable_mise_diagnostic() {
    let world = World::new();
    minimal(&world, "");
    let mise = world.home().join("unusable-mise");
    script(&mise, "exit 1");
    let result = output(world.dots().env("DOTS_MISE_BIN", mise).arg("status"), 2);
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains("dots ") && stdout.contains("Agent skill links"),
        "{stdout}"
    );
    assert!(!stdout.contains("Machine ID:"), "{stdout}");
    assert!(String::from_utf8_lossy(&result.stderr).contains("mise --version"));
    assert_eq!(read_report(&world)["status"], 2);
}

#[test]
fn new_status_keeps_an_invalid_selected_profile_in_the_startup_report() {
    let world = World::new();
    minimal(&world, "");
    let status = output(
        with_mise(&world).env("SETUP_PROFILE", "typo").arg("status"),
        2,
    );
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains("typo") && stderr.contains("laptop, server"),
        "{stderr}"
    );
    let report = read_report(&world);
    let startup = &report["stages"][1];
    assert_eq!(startup["label"], "Startup");
    assert_eq!(startup["outcome"], "failed");
    assert!(
        startup["issues"][0]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("typo")
    );
    assert!(
        startup["issues"][0]["next"]
            .as_str()
            .unwrap()
            .contains("dots profile")
    );
}

#[test]
fn unchanged_plan_previews_declared_resources_without_writing_them() {
    let world = World::new();
    let directory = world.home().join("planned-output.txt");
    minimal(
        &world,
        &format!(
            "[bootstrap.files]\n\"{}\" = {{ content = 'planned output' }}\n",
            directory.display()
        ),
    );
    let plan = output(with_mise(&world).arg("plan"), 0);
    let stdout = String::from_utf8_lossy(&plan.stdout);
    assert!(stdout.contains("planned-output.txt"), "{plan:?}");
    assert!(!directory.exists());
}

#[test]
fn new_plan_saves_a_private_metadata_only_report() {
    let world = World::new();
    minimal(&world, "");
    let fake = world.home().join("fixture-mise");
    script(
        &fake,
        r#"
case "$*" in
  'bootstrap packages status --json') printf '{}\n' ;;
  'bootstrap plan') printf 'synthetic native plan output\n' ;;
  *) exit 12 ;;
esac
"#,
    );
    let plan = output(world.dots().env("DOTS_MISE_BIN", fake).arg("plan"), 0);
    assert_eq!(plan.stdout, b"synthetic native plan output\n");
    let report = read_report(&world);
    assert_eq!(report["command"], "plan");
    assert_eq!(report["status"], 0);
    assert_eq!(report["stages"][0]["outcome"], "completed");
    assert!(!report.to_string().contains("synthetic native plan output"));
}

#[test]
fn unchanged_apply_selected_target_through_mise() {
    let world = World::new();
    minimal(
        &world,
        r#"
[dotfiles]
"~/first file" = { content = "one" }
"~/other file" = { content = "two" }
"#,
    );
    output(with_mise(&world).args(["apply", "~/first file"]), 0);
    assert_eq!(
        fs::read_to_string(world.home().join("first file"))
            .unwrap()
            .trim(),
        "one"
    );
    assert!(!world.home().join("other file").exists());
}

#[test]
fn new_exceptions_map_recipe_statuses_and_continue_after_a_failure() {
    let world = World::new();
    minimal(&world, "");
    let manifest = world.root().join("dots/exceptions.tsv");
    fs::write(
        &manifest,
        "base\tdefer\nlaptop\tskip\nlaptop\tfail\nlaptop\tlast\nserver\tother\n",
    )
    .unwrap();
    let recipes = world.root().join("dots/exceptions");
    for (recipe, code) in [("defer", 3), ("skip", 4), ("fail", 7), ("last", 0)] {
        script(
            &recipes.join(recipe),
            &format!(
                "[ \"$#\" = 1 ] && [ \"$1\" = --install-only ] && [ \"$DOTS_PROFILE\" = laptop ] && [ -z \"${{DOTS_CHANGES+x}}\" ] || exit 99\nprintf 'ran {recipe}\\n'\nexit {code}"
            ),
        );
    }
    let run = |status| {
        output(
            with_mise(&world)
                .env("DOTS_CHANGES", "untrusted")
                .arg("exceptions"),
            status,
        )
    };
    let result = run(1);
    assert_eq!(
        String::from_utf8_lossy(&result.stderr),
        "Failed installer exception: fail\n"
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    for recipe in ["defer", "skip", "fail", "last"] {
        assert!(
            stdout.contains(&format!("Installer exception: {recipe}\nran {recipe}\n")),
            "{stdout}"
        );
    }
    fs::write(&manifest, "base\tdefer\nlaptop\tskip\nlaptop\tlast\n").unwrap();
    run(0);
}

#[test]
fn new_retire_dotfiles_removes_only_dead_links_into_the_config() {
    let world = World::new();
    let alias = world.home().join("config-alias");
    symlink(world.root(), &alias).unwrap();
    symlink("config-alias/old/path", world.home().join(".retired")).unwrap();
    symlink(
        world.root().join("config.toml"),
        world.home().join(".still-live"),
    )
    .unwrap();
    symlink(
        world.home().join("missing"),
        world.home().join(".unrelated"),
    )
    .unwrap();
    fs::write(world.home().join(".regular"), "keep me").unwrap();
    fs::write(
        world.root().join("dots/retired-dotfiles"),
        "~/../outside\n~/.retired\n~/.still-live\n~/.unrelated\n~/.regular\n",
    )
    .unwrap();
    let result = output(
        world.dots().env("DOTS_ROOT", alias).arg("retire-dotfiles"),
        0,
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("Removed retired link ~/.retired"));
    assert!(String::from_utf8_lossy(&result.stderr).contains("~/../outside"));
    assert!(fs::symlink_metadata(world.home().join(".retired")).is_err());
    for name in [".still-live", ".unrelated", ".regular"] {
        assert!(fs::symlink_metadata(world.home().join(name)).is_ok());
    }
}

#[test]
fn new_missing_configuration_saves_one_startup_issue_without_a_child() {
    for command in ["plan", "apply", "test"] {
        let world = World::new();
        fs::remove_file(world.root().join("config.toml")).unwrap();
        let mise = world.home().join("unwanted-mise");
        script(&mise, r#": > "$HOME/mise-ran"; exit 99"#);
        let mut invocation = world.dots();
        invocation.env("DOTS_MISE_BIN", mise).arg(command);
        if command == "apply" {
            invocation.args(["--dry-run", "shell"]);
        }
        let result = output(&mut invocation, 2);
        assert!(result.stdout.is_empty(), "{result:?}");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert_eq!(stderr.lines().count(), 1);
        assert!(stderr.contains("config.toml"));
        assert!(!world.home().join("mise-ran").exists());
        let report = read_report(&world);
        assert_eq!(report["command"], command);
        assert_eq!(report["status"], 2);
        let stages = report["stages"].as_array().unwrap();
        assert_eq!(stages.len(), 1);
        let issues = stages[0]["issues"].as_array().unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["diagnostic"], stderr.trim_end());
        if command == "apply" {
            assert!(
                issues[0]["next"]
                    .as_str()
                    .unwrap()
                    .contains("same arguments"),
                "unsafe startup recovery advice: {}",
                issues[0]["next"]
            );
        }
    }
}

#[test]
fn new_inspection_uses_all_persisted_selectors_and_lists_uninstalled_tools() {
    let world = World::new();
    minimal(&world, "");
    fs::write(
        world.root().join("miserc.toml"),
        "env = ['laptop', 'host-fixture', 'extra']\n",
    )
    .unwrap();
    let mise = world.home().join("fixture-mise");
    script(
        &mise,
        r#"
case "$*" in
  --version) printf '2026.11.9\n'; exit 0 ;;
  'which ruby') exit 1 ;;
  'cli-selector') printf '%s\n' "$MISE_ENV"; exit 0 ;;
esac
[ "$MISE_ENV" = laptop,host-fixture,extra ] || exit 91
case "$*" in
  'bootstrap packages status --json') printf '{}\n' ;;
  'ls --current --json') printf '%s\n' '{"fixture-tool":[{"active":false,"installed":false,"requested_version":"2.0"}]}' ;;
  'bootstrap dotfiles status --json') printf '{}\n' ;;
  'bootstrap plan') printf 'planned\n' ;;
  'bootstrap dotfiles apply') printf 'applied\n' ;;
  *) exit 92 ;;
esac
"#,
    );
    let dots = || {
        let mut command = world.dots();
        command.env("DOTS_MISE_BIN", &mise);
        command
    };
    let status = output(dots().arg("status"), 0);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("fixture-tool@2.0") && stdout.contains("Install the selected tools"),
        "{stdout}"
    );
    assert_eq!(output(dots().arg("plan"), 0).stdout, b"planned\n");
    assert_eq!(output(dots().arg("apply"), 0).stdout, b"applied\n");
    assert_eq!(
        output(dots().args(["mise", "cli-selector"]), 0).stdout,
        b"laptop,host-fixture\n"
    );
}

#[test]
fn new_status_reads_origin_from_a_local_git_include() {
    let world = World::new();
    minimal(&world, "");
    output(world.git().args(["init", "--initial-branch=dev"]), 0);
    fs::write(
        world.root().join(".git/source.inc"),
        "[remote \"origin\"]\nurl = https://example.invalid/owner/repo.git\n",
    )
    .unwrap();
    output(
        world
            .git()
            .args(["config", "--local", "include.path", "source.inc"]),
        0,
    );
    let status = output(with_mise(&world).arg("status"), 0);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("shared through https://example.invalid/owner/repo.git"),
        "{stdout}"
    );
}

#[test]
fn new_status_marks_unreadable_git_config_as_attention() {
    let world = World::new();
    minimal(&world, "");
    output(world.git().args(["init", "--initial-branch=dev"]), 0);
    fs::write(world.root().join(".git/config"), "[broken\n").unwrap();
    let status = output(with_mise(&world).arg("status"), 0);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("repository — attention"), "{stdout}");
    assert!(!stdout.contains("local-only"), "{stdout}");
    assert_eq!(read_report(&world)["status"], 0);
}

#[test]
fn new_status_records_an_invalid_recipe_as_a_failed_section() {
    let world = World::new();
    minimal(&world, "");
    fs::write(world.root().join("dots/exceptions.tsv"), "invalid\n").unwrap();
    output(with_mise(&world).arg("status"), 1);
    let report = read_report(&world);
    assert_eq!(report["status"], 1);
    assert!(
        report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["label"] == "Installer exceptions" && stage["outcome"] == "failed")
    );
}

#[test]
fn new_status_records_loss_of_its_executable_during_program_probes() {
    let world = World::new();
    minimal(&world, "");
    let binary = world.home().join("private-dots");
    fs::copy(world.dots().get_program(), &binary).unwrap();
    let mise = world.home().join("mise");
    script(&mise, r#"/bin/rm "$HOME/private-dots" && echo 2026.11.9"#);
    let status = output(
        world
            .command(&binary)
            .env("DOTS_MISE_BIN", mise)
            .arg("status"),
        1,
    );
    assert!(!binary.exists());
    assert!(String::from_utf8_lossy(&status.stderr).contains("locate the running dots"));
    let report = read_report(&world);
    assert_eq!(report["status"], 1);
    let startup = report["stages"].as_array().unwrap().last().unwrap();
    assert_eq!(startup["label"], "Startup");
    assert!(
        startup["issues"][0]["next"]
            .as_str()
            .unwrap()
            .contains("dots status")
    );
}
