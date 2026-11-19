mod support;

use std::fs;
use std::io::{Error, ErrorKind};
use std::os::unix::fs::symlink;
use std::path::Path;

use support::{FixtureCommand, World};

fn marker<'a>(world: &'a World, path: &Path) -> FixtureCommand<'a> {
    let mut child = world.command("/bin/sh");
    child.args(["-eu", "-c", r#"printf '%s' "$HOME" > "$1""#, "marker"]);
    child.arg(path);
    child
}

fn refused(error: Error, key: &str) {
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(error.to_string().starts_with(key), "{error}");
}

#[test]
fn fixture_rejects_caller_home_and_symlink_alias_before_launch() {
    let world = World::new();
    let home = std::env::var_os("HOME").expect("test runner HOME");
    let alias = world.base().join("home-alias");
    symlink(&home, &alias).unwrap();
    let path = world.base().join("ran");
    refused(
        marker(&world, &path)
            .env("HOME", home)
            .output()
            .unwrap_err(),
        "HOME",
    );
    refused(
        marker(&world, &path)
            .env("HOME", alias)
            .spawn()
            .unwrap_err(),
        "HOME",
    );
    assert!(!path.exists());
}

#[test]
fn fixture_checks_effective_state_paths_and_keeps_agent_roots_absent() {
    let world = World::new();
    let outside = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let untouched = outside.path().join("untouched");
    fs::write(&untouched, "original").unwrap();
    let alias = world.base().join("escape");
    symlink(outside.path(), &alias).unwrap();
    let path = world.base().join("ran");
    for (key, location) in [
        ("XDG_CACHE_HOME", outside.path().to_owned()),
        ("XDG_STATE_HOME", alias.join("not-yet-created")),
        ("DOTS_UP_STATE_DIR", alias.join("old-reports")),
        ("CLAUDE_CONFIG_DIR", alias.clone()),
        ("CODEX_HOME", alias.clone()),
        ("MISE_CONFIG_DIR", alias.clone()),
        ("MISE_SYSTEM_DATA_DIR", alias.clone()),
        ("MISE_INSTALLS_DIR", alias.clone()),
        ("TMPDIR", alias.clone()),
    ] {
        refused(
            marker(&world, &path)
                .env(key, location)
                .status()
                .unwrap_err(),
            key,
        );
    }
    refused(
        marker(&world, &path)
            .env_remove("HOME")
            .output()
            .unwrap_err(),
        "HOME",
    );
    refused(
        marker(&world, &path)
            .env("DOTS_UP_STATE_DIR", "old-reports")
            .status()
            .unwrap_err(),
        "DOTS_UP_STATE_DIR",
    );
    refused(
        marker(&world, &path).env_clear().output().unwrap_err(),
        "HOME",
    );
    let state = world.home().join(".local/state/dots");
    fs::create_dir(&state).unwrap();
    symlink(&untouched, state.join("skill-notices")).unwrap();
    refused(
        marker(&world, &path)
            .env_remove("DOTS_SKILL_LINKS")
            .output()
            .unwrap_err(),
        "skill-notices",
    );
    assert!(!path.exists());
    assert_eq!(fs::read(&untouched).unwrap(), b"original");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);

    let mut child = marker(&world, &path);
    child.env("XDG_CACHE_HOME", world.base().join("private/nested/cache"));
    assert!(child.output().unwrap().status.success());
    assert_eq!(
        fs::read(&path).unwrap(),
        world.home().as_os_str().as_encoded_bytes()
    );
    let output = world
        .command("/bin/sh")
        .without_home()
        .args([
            "-eu",
            "-c",
            r#"test "${HOME+x}" != x && test -n "$XDG_STATE_HOME""#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    for dir in [".agents", ".claude", ".codex"] {
        assert!(!world.home().join(dir).exists());
    }
}

#[test]
fn fixture_git_keeps_base_probes_below_the_ceiling() {
    let world = World::new();
    let mut git = world.git();
    git.current_dir(world.base());
    let ceiling = git
        .get_envs()
        .find_map(|(key, value)| {
            (key == "GIT_CEILING_DIRECTORIES")
                .then_some(value)
                .flatten()
        })
        .unwrap();
    let parent = world.base().parent().unwrap();
    assert!(
        std::env::split_paths(ceiling).any(|path| path == parent),
        "Git needs a fixture-owned ceiling above its starting directory"
    );
    let init = world
        .git()
        .current_dir(parent)
        .args(["init", "-q"])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    let probe = git
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .unwrap();
    assert_eq!(probe.status.code(), Some(128), "{probe:?}");
}
