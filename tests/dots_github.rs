#[path = "support/github_env.rs"]
mod github_env;
mod support;

use std::{fs, os::unix::fs::symlink};

use support::{FixtureCommand, World};

fn dots(world: &World) -> FixtureCommand<'_> {
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

    let result = world
        .command("/bin/sh")
        .args(["-c", "\"$1\" --version", "sh"])
        .arg(stable)
        .current_dir(world.root())
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
fn new_pin_provenance_refuses_a_symlink_sidecar() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::new();
    let pinned = world.home().join("dots-run-fixture");
    fs::create_dir(&pinned).unwrap();
    fs::set_permissions(&pinned, fs::Permissions::from_mode(0o700)).unwrap();
    let exe = pinned.join("dots");
    support::stage_executable(world.dots().get_program(), &exe).unwrap();
    let forged = world.home().join("forged");
    fs::write(&forged, b"mise\0/forged/installation/bin/dots").unwrap();
    fs::set_permissions(&forged, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&forged, pinned.join("installation")).unwrap();
    let result = world.command(exe).arg("--version").output().unwrap();
    assert_eq!(result.status.code(), Some(1), "{result:?}");
}

#[test]
fn new_nested_command_keeps_the_original_mise_installation_with_a_different_data_dir() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::new();
    let baseline = dots(&world);
    let original_data = world.home().join("original-data");
    let original_installs = original_data.join("installs");
    let installed = original_installs.join("fixture/1/bin/dots");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    support::stage_executable(baseline.get_program(), &installed).unwrap();

    let commands = world.root().join("dots/commands");
    fs::create_dir_all(&commands).unwrap();
    let command = commands.join("nested-version");
    fs::write(
        &command,
        r#"#!/bin/sh
set -eu
case "$(command -v dots)" in
    "$TMPDIR"/dots-run-*/dots) ;;
    *) exit 97 ;;
esac
export MISE_DATA_DIR="$HOME/changed-data"
export MISE_INSTALLS_DIR="$MISE_DATA_DIR/installs"
exec dots --version
"#,
    )
    .unwrap();
    fs::set_permissions(command, fs::Permissions::from_mode(0o755)).unwrap();

    let run = |arg| {
        let output = world
            .command(&installed)
            .envs(
                baseline
                    .get_envs()
                    .filter_map(|(key, value)| value.map(|value| (key, value))),
            )
            .env("MISE_DATA_DIR", &original_data)
            .env("MISE_INSTALLS_DIR", &original_installs)
            .arg(arg)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let direct = run("--version");
    assert!(direct.trim_end().ends_with("(mise)"), "{direct}");
    assert_eq!(run("nested-version"), direct);
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "downloads a public fixture release from GitHub"]
fn new_nested_github_app_add_records_the_durable_homebrew_command() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::new();
    let baseline = dots(&world);
    let prefix = world.home().join("brew with spaces");
    let keg = prefix.join("Cellar/dots/fixture/bin/dots");
    fs::create_dir_all(keg.parent().unwrap()).unwrap();
    support::stage_executable(baseline.get_program(), &keg).unwrap();
    let stable = prefix.join("bin/dots");
    fs::create_dir(stable.parent().unwrap()).unwrap();
    symlink(&keg, &stable).unwrap();
    let mise = world.home().join("mise");
    fs::write(
        &mise,
        r#"#!/bin/sh
set -eu
test "$1 $2 $3 $4" = "install --raw -- github:sindresorhus/plash"
test -f "$MISE_CONFIG_DIR/mise.toml"
"#,
    )
    .unwrap();
    fs::set_permissions(&mise, fs::Permissions::from_mode(0o755)).unwrap();

    let commands = world.root().join("dots/commands");
    fs::create_dir_all(&commands).unwrap();
    for (name, script) in [
        ("nested-app", "#!/bin/sh\nset -eu\nexec dots nested-add\n"),
        (
            "nested-add",
            r#"#!/bin/sh
set -eu
case "$(command -v dots)" in
    "$TMPDIR"/dots-run-*/dots) ;;
    *) exit 97 ;;
esac
printf '%s\n' "$(command -v dots)" > "$HOME/nested-dots"
exec dots add --base --asset 'Plash.2.16.0.-.macOS.15.zip' https://github.com/sindresorhus/Plash
"#,
        ),
    ] {
        let command = commands.join(name);
        fs::write(&command, script).unwrap();
        fs::set_permissions(command, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = world
        .command(&stable)
        .envs(
            baseline
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .env("DOTS_MISE_BIN", mise)
        .arg("nested-app")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let pinned = fs::read_to_string(world.home().join("nested-dots")).unwrap();
    assert!(!std::path::Path::new(pinned.trim_end()).exists());
    let config: toml_edit::DocumentMut = fs::read_to_string(world.root().join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let hook = config["tools"]["github:sindresorhus/plash"]["postinstall"]["run"]
        .as_str()
        .unwrap();
    assert_eq!(
        hook,
        format!(
            "'{}' install-release-app sindresorhus/plash",
            stable.display()
        )
    );
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
