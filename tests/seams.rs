mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use dots_cli::config::Root;
use dots_cli::report::{self, Command as ReportCommand};
use dots_cli::run::{Change, Issue, Outcome, Policy, Run, Sink, exit_code};
use dots_cli::{Ctx, Env, Flags, cli};

fn invoke(world: &support::World, cwd: &Path, args: &[&str]) -> Output {
    world
        .dots()
        .current_dir(cwd)
        .env("DOTS_ROOT", cwd)
        .args(args)
        .output()
        .unwrap()
}

fn assert_diagnostic(output: Output, status: i32, expected: &str) {
    assert_eq!(output.status.code(), Some(status), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("dots: ") && stderr.contains(expected),
        "{stderr}"
    );
}

#[test]
fn seams_cli_grammar_and_config_boundary() {
    let world = support::World::with_dots(env!("CARGO_BIN_EXE_dots"));
    let cwd = world.base();
    let grammar = cli::tree();
    grammar.clone().debug_assert();
    assert!(grammar.find_subcommand("__exec").is_none());
    assert!(grammar.find_subcommand("__complete").is_none());
    let help = invoke(&world, cwd, &["--help"]);
    assert!(help.status.success());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(!stdout.contains("--no-wait"));
    for command in grammar.get_subcommands() {
        assert!(
            stdout.lines().any(|line| line
                .trim_start()
                .starts_with(&format!("{} ", command.get_name()))),
            "{} is missing from the top-level help",
            command.get_name()
        );
    }
    let bare = invoke(&world, cwd, &[]);
    assert!(bare.status.success());
    assert_eq!(bare.stdout, stdout.as_bytes());
    assert!(
        invoke(&world, cwd, &["bootstrap", "complete", "--help"])
            .status
            .success()
    );
    assert!(invoke(&world, cwd, &["init", "--help"]).status.success());
    assert!(
        invoke(&world, cwd, &["completions", "--help"])
            .status
            .success()
    );
    assert_diagnostic(
        invoke(&world, cwd, &["completions", "bash", "fish"]),
        2,
        "dots completions bash|zsh|fish",
    );
    let brew_without_config = invoke(&world, cwd, &["brew", "path"]);
    assert!(
        matches!(brew_without_config.status.code(), Some(0 | 1)),
        "{brew_without_config:?}"
    );
    assert_diagnostic(invoke(&world, cwd, &["sync", "--other"]), 2, "dots help");
    assert_diagnostic(
        invoke(&world, cwd, &["publish", "--bookmark=secret", "--all"]),
        2,
        "dots help",
    );
    assert_diagnostic(
        invoke(&world, cwd, &["help", "status"]),
        2,
        "dots COMMAND --help",
    );
    assert_diagnostic(
        invoke(&world, cwd, &["missing", "--help"]),
        2,
        "unknown command",
    );
    assert_diagnostic(
        invoke(&world, cwd, &["--plain", "update", "--verbose"]),
        2,
        "cannot be combined",
    );

    for args in [
        &["--", "up"][..],
        &["--", "help", "extra"],
        &["--", "completions", "bash"],
        &["init", "--bogus"],
    ] {
        assert_diagnostic(invoke(&world, cwd, args), 2, "dots help");
    }
    assert_diagnostic(
        invoke(&world, cwd, &["--plain", "update", "--verbose", "--help"]),
        2,
        "cannot be combined",
    );
    for args in [&["--no-wait", "up"][..], &["update", "--no-wait"]] {
        assert_diagnostic(invoke(&world, cwd, args), 2, "unknown option '--no-wait'");
    }

    for args in [
        &["root"][..],
        &["profile"],
        &["update"],
        &["uninstall", "example"],
        &["brew-invalid-command"],
        &["test", "--help"],
        &["mise", "--help"],
    ] {
        let expected = if args[0] == "brew-invalid-command" {
            "use dots help"
        } else {
            "config.toml not found"
        };
        assert_diagnostic(invoke(&world, cwd, args), 2, expected);
    }

    for args in [
        &["dots", "update", "--mas", "--plain"][..],
        &["dots", "remove", "--path", "-config=example", "ripgrep"],
        &["dots", "init", "--", "-config=example"],
        &["dots", "init", "./--bogus"],
        &["dots", "test", "--", "-j", "post data"],
        &["dots", "mise", "--help"],
        &[
            "dots",
            "bootstrap",
            "-E",
            "laptop,host-example",
            "--only=tools,task",
            "--only",
            "files",
        ],
        &[
            "dots", "brew", "record", "cask", "example", "updated", "1.0",
        ],
    ] {
        assert!(
            grammar.clone().try_get_matches_from(args).is_ok(),
            "{args:?}"
        );
    }
    assert_diagnostic(
        invoke(
            &world,
            cwd,
            &["bootstrap", "--only", "tools", "--skip", "files"],
        ),
        2,
        "dots help",
    );
    assert_diagnostic(
        invoke(&world, cwd, &["remove", "--keep-installed", "./example"]),
        2,
        "file request",
    );
    assert_diagnostic(
        invoke(&world, cwd, &["add", "--asset", "example", "npm:example"]),
        2,
        "one GitHub repository URL",
    );
}

#[test]
fn seams_cli_leading_dash_is_not_short_help() {
    let world = support::World::with_dots(env!("CARGO_BIN_EXE_dots"));
    let cwd = world.base();
    fs::write(cwd.join("config.toml"), "").unwrap();
    assert_diagnostic(invoke(&world, cwd, &["-hello"]), 2, "use dots help");

    let commands = cwd.join("dots/commands");
    fs::create_dir_all(&commands).unwrap();
    let hello = commands.join("hello");
    fs::write(&hello, "#!/bin/sh\nprintf '[%s]\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&hello, fs::Permissions::from_mode(0o755)).unwrap();
    assert_diagnostic(invoke(&world, cwd, &["-hello"]), 2, "use dots help");

    let short = invoke(&world, cwd, &["-h"]);
    let long = invoke(&world, cwd, &["--help"]);
    assert!(short.status.success() && long.status.success());
    assert_eq!(short.stdout, long.stdout);
    assert!(
        String::from_utf8(short.stdout)
            .unwrap()
            .contains("  hello [ARGS...]")
    );

    let output = invoke(&world, cwd, &["hello", "-hello", "--help", "--"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"[-hello]\n[--help]\n[--]\n");
}

#[test]
fn seams_cli_menu_is_an_ordinary_configuration_command() {
    let world = support::World::with_dots(env!("CARGO_BIN_EXE_dots"));
    let cwd = world.base();
    fs::write(cwd.join("config.toml"), "").unwrap();
    let commands = cwd.join("dots/commands");
    fs::create_dir_all(&commands).unwrap();
    let menu = commands.join("menu");
    fs::write(&menu, "#!/bin/sh\nprintf '[%s]\\n' \"$@\"\nexit 23\n").unwrap();
    assert_diagnostic(invoke(&world, cwd, &["menu"]), 2, "unknown command 'menu'");
    fs::set_permissions(&menu, fs::Permissions::from_mode(0o755)).unwrap();

    let bare = invoke(&world, cwd, &[]);
    assert!(bare.status.success());
    let help = String::from_utf8(bare.stdout).unwrap();
    let (_, config_commands) = help.split_once("Configuration commands").unwrap();
    assert!(config_commands.contains("  menu [ARGS...]\n"));

    let output = invoke(
        &world,
        cwd,
        &["menu", "--help", "two words", "", "--", "--no-wait"],
    );
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        output.stdout,
        b"[--help]\n[two words]\n[]\n[--]\n[--no-wait]\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn seams_direct_child_helper() {
    if std::env::var_os("DOTS_TEST_DIRECT_HELPER").is_none() {
        return;
    }
    let env = Env::current().unwrap();
    let root = Root::resolve(&env).unwrap();
    let ctx = Ctx {
        env,
        root,
        flags: Flags::default(),
    };
    let run = Run::begin(&ctx, Sink::Report(ReportCommand::Test)).with_rerun("dots test shell");
    let stage = run.start("Native fixture");
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "IFS= read -r line; printf 'out:%s' \"$line\"; printf 'err:%s' \"$line\" >&2; exit 23",
    ]);
    let status = run.run(stage, command, Policy::Interactive).unwrap();
    run.issue(
        stage,
        Issue {
            diagnostic: "native fixture failed".into(),
            next: "dots test".into(),
            tail: Some("ignored stream".into()),
            log: Some("ignored path".into()),
            terminal_exit: None,
        },
    );
    run.finish(stage, Outcome::Failed);
    std::process::exit(i32::from(run.end(exit_code(status))));
}

#[test]
fn seams_direct_inherited_streams_and_private_report() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let state = cwd.join("state");
    // The surrounding manager state can predate the Rust reports directory.
    fs::create_dir_all(state.join("dots")).unwrap();
    fs::set_permissions(state.join("dots"), fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .current_dir(cwd)
        .env_clear()
        .env("DOTS_TEST_DIRECT_HELPER", "1")
        .env("DOTS_ROOT", cwd)
        .env("XDG_STATE_HOME", &state)
        .args(["--exact", "seams_direct_child_helper"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"fixture-private-data\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert!(
        output.stdout.ends_with(b"out:fixture-private-data"),
        "{output:?}"
    );
    assert_eq!(output.stderr, b"err:fixture-private-data");
    let reports = state.join("dots/reports");
    assert_eq!(
        fs::metadata(&reports).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let report = fs::read_dir(reports)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::metadata(&report).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let json = fs::read_to_string(report).unwrap();
    assert!(!json.contains("fixture-private-data"));
    let report: report::Report = serde_json::from_str(&json).unwrap();
    assert_eq!(report.command, ReportCommand::Test);
    assert_eq!(report.status, 23);
    assert_eq!(report.next.as_deref(), Some("dots test shell"));
    assert_eq!(report.stages[0].issues[0].tail, None);
    assert_eq!(report.stages[0].issues[0].log, None);
}

#[test]
fn seams_report_round_trip_and_fragment_fold() {
    // The round trip uses child scope to test the real XDG path without changing the
    // test process environment shared with other test threads.
    if let Some(state) = std::env::var_os("DOTS_TEST_REPORT_ROUND_TRIP") {
        let env = Env::current().unwrap();
        let root = Root::resolve(&env).unwrap();
        assert!(report::list(&env, None).unwrap().is_empty());
        let ctx = Ctx {
            env,
            root,
            flags: Flags::default(),
        };
        let fragment_dir = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(state)
            .unwrap();
        let fragment = fragment_dir.path().join("stages.json");
        let child = Run::begin(&ctx, Sink::Fragment(fragment.clone()));
        let hook = child.start("Child");
        assert!(fragment.is_file());
        child.change(
            hook,
            Change {
                item: "tool".into(),
                before: "1".into(),
                after: "2".into(),
            },
        );
        child.finish(hook, Outcome::Completed);
        assert_eq!(child.end(0), 0);

        let parent = Run::begin(&ctx, Sink::Report(ReportCommand::Bootstrap));
        let stage = parent.start("Parent");
        assert_eq!(parent.snapshot().stages[0].id, stage);
        parent.finish(stage, Outcome::Completed);
        parent.fold(&fragment).unwrap();
        assert_ne!(parent.snapshot().stages[1].id, stage);
        assert_eq!(parent.end(0), 0);
        assert!(
            report::list(&ctx.env, Some(ReportCommand::Up))
                .unwrap()
                .is_empty()
        );
        let list = report::list(&ctx.env, Some(ReportCommand::Bootstrap)).unwrap();
        assert_eq!(list.len(), 1);
        let report = report::load(&ctx.env, &list[0].id).unwrap();
        assert_eq!(report.stages.len(), 2);
        assert_eq!(report.stages[1].changes[0].after, "2");

        let state = ctx.env.state_dir().unwrap();
        let reports = state.join("reports");
        let saved = state.join("saved-reports");
        fs::rename(&reports, &saved).unwrap();
        symlink(&saved, &reports).unwrap();
        assert!(report::list(&ctx.env, None).is_err());
        assert!(report::load(&ctx.env, &list[0].id).is_err());
        let run = Run::begin(&ctx, Sink::Report(ReportCommand::Test));
        assert_eq!(run.end(31), 31);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("DOTS_TEST_REPORT_ROUND_TRIP", dir.path())
        .env("XDG_STATE_HOME", dir.path())
        .env("DOTS_ROOT", dir.path())
        .args([
            "--exact",
            "seams_report_round_trip_and_fragment_fold",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not save the report for dots test")
    );
}
