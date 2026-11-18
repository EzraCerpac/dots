#[path = "support/github_env.rs"]
mod github_env;
mod support;

use std::{fs, os::unix::fs::symlink, process::Command};

use support::World;

fn dots(world: &World) -> Command {
    let mut cmd = world.dots();
    cmd.envs(github_env::private_environment(
        world.home().parent().unwrap(),
        world.home(),
        world.root(),
    ))
    .env(
        "PATH",
        std::env::var_os("DOTS_TEST_TOOL_PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
    );
    cmd
}

#[test]
fn unchanged_homebrew_prefix_link_launches_dots_without_dots_on_path() {
    let world = World::new();
    let baseline = dots(&world);
    let expected = dots(&world).arg("--version").output().unwrap();
    assert!(expected.status.success(), "{expected:?}");
    let prefix = world.home().join("brew with spaces");
    let keg = prefix.join("Cellar/dots/fixture/bin");
    let bin = prefix.join("bin");
    fs::create_dir_all(&keg).unwrap();
    fs::create_dir(&bin).unwrap();
    symlink(baseline.get_program(), keg.join("dots")).unwrap();
    let stable = bin.join("dots");
    symlink("../Cellar/dots/fixture/bin/dots", &stable).unwrap();

    let result = Command::new("/bin/sh")
        .args(["-c", "\"$1\" --version", "sh"])
        .arg(stable)
        .current_dir(world.root())
        .env_clear()
        .envs(
            baseline
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, expected.stdout);
}

#[test]
#[ignore = "P-01"]
fn unchanged_rejects_a_github_release_page_as_a_repository() {
    let world = World::new();
    let config = world.root().join("config.toml");
    let before = fs::read(&config).unwrap();
    let result = dots(&world)
        .args(["add", "--base", "https://github.com/example/tool/releases"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "{result:?}");
    assert!(stderr.contains("HTTPS GitHub repository URL"), "{stderr}");
    assert_eq!(fs::read(config).unwrap(), before);
}

#[test]
fn new_callback_preflight_needs_no_dots_configuration() {
    let world = World::new();
    fs::remove_file(world.root().join("config.toml")).unwrap();

    let result = dots(&world)
        .args(["install-release-app", "example/tool"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1), "{result:?}");
    let stderr = String::from_utf8_lossy(&result.stderr);
    if cfg!(target_os = "macos") {
        assert!(stderr.contains("MISE_TOOL_INSTALL_PATH"), "{stderr}");

        let release = world.root().join("release");
        for name in ["One.app", "Two.app"] {
            let contents = release.join(name).join("Contents");
            fs::create_dir_all(&contents).unwrap();
            fs::write(contents.join("Info.plist"), b"synthetic").unwrap();
        }
        let result = dots(&world)
            .args(["install-release-app", "example/tool"])
            .env("MISE_TOOL_INSTALL_PATH", &release)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(1), "{result:?}");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains("exactly one app bundle"), "{stderr}");
    } else {
        assert!(stderr.contains("requires macOS"), "{stderr}");
    }
    assert!(
        !world
            .home()
            .parent()
            .unwrap()
            .join("xdg-state/dots/github-apps")
            .exists()
    );
}
