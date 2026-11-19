use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use nix::libc;

use crate::Env;
use crate::report::{self, Command as ReportCommand, Outcome};

use super::fixture_command::{FixtureCommand, PRIVATE_DIRS};
use super::github::test_env::private_environment;

const FIXTURE: &str = "DOTS_PACKAGE_ADMISSION_FIXTURE";
const CONFIG: &str = "[vars]\ndots_profiles = \"test\"\n\n[dotfiles]\n";
const FIRST: &[u8] = b"first fixture\n";
const NEXT: &[u8] = b"next fixture\0\n";

thread_local! {
    static STARTS_UNTIL_INTERRUPT: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn before_stage_start() {
    STARTS_UNTIL_INTERRUPT.with(|starts| {
        let remaining = starts.get();
        if remaining == 0 {
            return;
        }
        starts.set(remaining - 1);
        if remaining == 1 {
            let base = PathBuf::from(std::env::var_os(FIXTURE).unwrap());
            fs::copy(
                base.join("config/config.toml"),
                base.join("config-before-denial"),
            )
            .unwrap();
            // Only the isolated test child gets this interrupt. Run has already
            // installed its handler and will observe it in the admission call.
            assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        }
    });
}

#[test]
fn add_denied_admission_leaves_next_file_and_config_untouched() {
    check(
        "add",
        ReportCommand::Add,
        "packages::tests::add_denied_admission_leaves_next_file_and_config_untouched",
    );
}

#[test]
fn remove_denied_admission_leaves_next_file_and_config_untouched() {
    check(
        "remove",
        ReportCommand::Remove,
        "packages::tests::remove_denied_admission_leaves_next_file_and_config_untouched",
    );
}

fn check(command: &str, report_command: ReportCommand, test: &str) {
    let Some(base) = std::env::var_os(FIXTURE).map(PathBuf::from) else {
        let directory = tempfile::Builder::new()
            .prefix("dots-package-admission-")
            .tempdir_in("/tmp")
            .unwrap();
        let base = directory.path().canonicalize().unwrap();
        let config = base.join("config");
        let home = base.join("home");
        fs::create_dir(&config).unwrap();
        fs::create_dir(&home).unwrap();
        let command_line = Command::new(std::env::current_exe().unwrap());
        let output = FixtureCommand::new(command_line, &base)
            .env_clear()
            .envs(fixture_environment(&base, &home, &config))
            .env(FIXTURE, &base)
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", base.join("gitconfig"))
            .env("GIT_CEILING_DIRECTORIES", &base)
            .current_dir(&config)
            .args(["--exact", test, "--nocapture"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    };

    assert_eq!(
        base.parent(),
        Some(fs::canonicalize("/tmp").unwrap().as_path())
    );
    assert!(
        base.file_name()
            .unwrap()
            .as_encoded_bytes()
            .starts_with(b"dots-package-admission-")
    );
    let env = Env::current().unwrap();
    let config = base.join("config/config.toml");
    let home = base.join("home");
    assert_eq!(env.cwd(), base.join("config"));
    assert_eq!(env.home(), Some(home.as_path()));
    assert!(env.get("__MISE_DIFF").is_none());
    for (key, expected) in fixture_environment(&base, &home, env.cwd()) {
        assert_eq!(env.get(key.to_str().unwrap()), Some(expected.as_os_str()));
    }

    let first = home.join("first");
    let next = home.join("next");
    let next_source = base.join("config/dotfiles/next");
    fs::write(&config, CONFIG).unwrap();
    fs::write(&first, FIRST).unwrap();
    fs::write(&next, NEXT).unwrap();
    if command == "remove" {
        assert_eq!(invoke("add"), 0);
    }
    let original_link = fs::read_link(&next).ok();

    STARTS_UNTIL_INTERRUPT.with(|starts| starts.set(2));
    assert_eq!(invoke(command), 130);
    assert_eq!(fs::read(&first).unwrap(), FIRST);
    assert_eq!(
        fs::symlink_metadata(&first).unwrap().is_symlink(),
        command == "add"
    );
    assert_eq!(fs::read(&next).unwrap(), NEXT);
    assert_eq!(fs::read_link(&next).ok(), original_link);
    if command == "remove" {
        assert_eq!(fs::read(&next_source).unwrap(), NEXT);
    } else {
        assert!(!next_source.exists());
    }
    assert_eq!(
        fs::read(config).unwrap(),
        fs::read(base.join("config-before-denial")).unwrap()
    );

    let reports = report::list(&env, Some(report_command)).unwrap();
    assert_eq!(reports.len(), 1);
    let saved = report::load(&env, &reports[0].id).unwrap();
    assert_eq!(saved.status, 130);
    assert!(saved.cancelled);
    assert_eq!(saved.stages.len(), 1);
    assert_eq!(saved.stages[0].outcome, Some(Outcome::Completed));
    assert!(saved.stages[0].issues.is_empty());
}

fn fixture_environment(base: &Path, home: &Path, config: &Path) -> Vec<(OsString, OsString)> {
    let mut environment = private_environment(base, home, config);
    for &(name, relative) in PRIVATE_DIRS {
        if !environment.iter().any(|(key, _)| key == name) {
            environment.push((name.into(), base.join(relative).into_os_string()));
        }
    }
    environment
}

fn invoke(command: &str) -> u8 {
    crate::cli::run(
        ["dots", command, "--base", "~/first", "~/next"]
            .map(Into::into)
            .to_vec(),
    )
    .unwrap()
}
