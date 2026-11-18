mod support;

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;
use support::World;

#[test]
fn unchanged_help_without_configuration() {
    let world = World::new();
    let output = world
        .dots()
        .current_dir(world.home())
        .env_remove("DOTS_ROOT")
        .arg("help")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.split_whitespace().any(|word| word == "--version"),
        "{stdout}"
    );
}

#[test]
fn unchanged_help_runs_through_a_relative_link_to_a_separate_manager() {
    let world = World::new();
    let base = world.root().parent().unwrap();
    let manager = base.join("tools/manager");
    let bin = base.join("bin");
    fs::create_dir_all(&manager).unwrap();
    fs::create_dir(&bin).unwrap();
    let dots = world.dots();
    symlink(dots.get_program(), manager.join("dots")).unwrap();
    let entry = bin.join("dots");
    symlink("../tools/manager/dots", &entry).unwrap();

    let output = Command::new(entry)
        .env_clear()
        .envs(
            dots.get_envs()
                .filter_map(|(name, value)| value.map(|value| (name, value))),
        )
        .current_dir(world.root())
        .arg("help")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.split_whitespace().any(|word| word == "--version"),
        "{stdout}"
    );
}

#[test]
#[ignore = "P-02"]
fn unchanged_status_names_missing_mise_in_one_line() {
    let world = World::new();
    let bin = world.home().join(".local/bin");
    fs::create_dir(&bin).unwrap();
    // v0.4.0 needs these before it reaches its mise lookup. No system mise
    // can leak into this PATH, even on systems that install it in /usr/bin.
    symlink("/bin/bash", bin.join("bash")).unwrap();
    symlink("/usr/bin/dirname", bin.join("dirname")).unwrap();

    let output = world
        .dots()
        .env("PATH", bin)
        .arg("status")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains("mise") && stderr.contains("https://mise.jdx.dev"),
        "{stderr}"
    );
}
