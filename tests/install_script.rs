mod support;

use nix::libc;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;
use support::{FixtureCommand, World};

const RELEASES: &str = "https://github.com/EzraCerpac/dots/releases";

fn fixture() -> World {
    let world = World::new();
    let bin = world.base().join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, script) in [
        (
            "curl",
            r#"printf 'curl\n' >> "$TRACE"
test "$*" = '-fsSLI -o /dev/null -w %{url_effective} https://github.com/EzraCerpac/dots/releases/latest'
if [ "$REDIRECT" = fail ]; then echo 'test transport failure' >&2; exit 22; fi
printf '%s' "$REDIRECT""#,
        ),
        (
            "git",
            r#"printf 'git\n' >> "$TRACE"
exit 71"#,
        ),
        (
            "mise",
            r#"if [ "$1" = --version ]; then echo 2026.11.8; exit 0; fi
if [ "$1" = -q ]; then shift; fi
test "$1" = -C && test "$2" = "$HOME"
shift 2
tool="github:EzraCerpac/dots@$EXPECTED"
printf '%s\n' "$1" >> "$TRACE"
case "$1" in
    install) test "$2" = "$tool" ;;
    which)
        test "$2 $3 $4" = "--tool $tool dots"
        printf '%s\n' "${0%/*}/candidate" ;;
    reshim)
        mkdir -p "$MISE_SHIMS_DIR"
        ln -s "${0%/*}/candidate" "$MISE_SHIMS_DIR/dots" ;;
    settings) printf '%s\n' "$MISE_SHIMS_DIR" ;;
    *) exit 72 ;;
esac"#,
        ),
        (
            "candidate",
            r#"printf 'dots %s\n' "$*" >> "$TRACE"
test "$*" = --version
test "$DOTS_SKILL_LINKS" = 0
printf 'dots %s (mise)\n' "$ACTUAL""#,
        ),
    ] {
        let path = bin.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    world
}

fn installer<'a>(world: &'a World, version: &str) -> FixtureCommand<'a> {
    let bin = world.base().join("bin");
    let mut command = world.command("/bin/sh");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
        .env(
            "PATH",
            std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap(),
        )
        .env("DOTS_MISE_BIN", bin.join("mise"))
        .env("DOTS_SKILL_LINKS", "1")
        .env("TRACE", world.base().join("trace"))
        .env("REDIRECT", format!("{RELEASES}/tag/v{version}"))
        .env("EXPECTED", version)
        .env("ACTUAL", version);
    // Detach the shell so /dev/tty stays unavailable when run from a terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

fn trace(world: &World) -> String {
    fs::read_to_string(world.base().join("trace")).unwrap()
}

fn assert_refusal(output: &Output, reason: &str) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains(reason), "{stderr}");
}

#[test]
fn headless_installer_installs_the_exact_release_and_prints_init_without_writing_config() {
    for requested_latest in [false, true] {
        let world = fixture();
        let mut command = installer(&world, "0.5.0");
        if requested_latest {
            command
                .env("DOTS_VERSION", "0.4.0")
                .args(["--version", "latest"]);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            trace(&world),
            "curl\ninstall\nwhich\ndots --version\nreshim\nsettings\n"
        );
        assert_eq!(
            fs::read_link(world.home().join(".local/bin/dots")).unwrap(),
            world.base().join("mise/data/shims/dots")
        );
        assert!(!world.root().join("conf.d/00-dots-manager.toml").exists());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!(
                "'{}' init",
                world.base().join("bin/candidate").display()
            )),
            "{output:?}"
        );
    }
}

#[test]
fn new_installer_prefers_cli_version_over_environment_without_lookup() {
    for (version, args) in [
        ("0.4.0", &[][..]),
        ("0.5.1-rc.1", &["--version", "v0.5.1-rc.1"][..]),
    ] {
        let world = fixture();
        let output = installer(&world, version)
            .env("DOTS_VERSION", "0.4.0")
            .env("REDIRECT", "fail")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            trace(&world),
            "install\nwhich\ndots --version\nreshim\nsettings\n"
        );
    }
}

#[test]
fn new_installer_refuses_failed_or_malformed_latest_redirect() {
    for redirect in [
        "fail".to_owned(),
        "0.5.0".to_owned(),
        format!("{RELEASES}/latest"),
        format!("{RELEASES}/tag/latest"),
        format!("{RELEASES}/tag/v0.5"),
        format!("{RELEASES}/tag/v0.5.0?query"),
        format!("{RELEASES}/tag/v0.5.0-rc.01"),
        "https://github.com/other/dots/releases/tag/v0.5.0".to_owned(),
    ] {
        let world = fixture();
        let output = installer(&world, "0.5.0")
            .env("REDIRECT", redirect)
            .output()
            .unwrap();
        assert_refusal(&output, "sh -s -- --version X.Y.Z");
        assert_eq!(trace(&world), "curl\n");
    }
}

#[test]
fn new_installer_refuses_a_wrong_binary_before_adoption_or_activation() {
    for args in [&[][..], &["--from", "example/dotfiles"][..]] {
        let world = fixture();
        let launcher = world.home().join(".local/bin/dots");
        fs::create_dir(launcher.parent().unwrap()).unwrap();
        fs::write(&launcher, "existing launcher").unwrap();
        let sentinel = world.root().join("keep");
        fs::write(&sentinel, "existing config").unwrap();
        let output = installer(&world, "0.5.0")
            .env("ACTUAL", "0.4.0")
            .args(args)
            .output()
            .unwrap();
        assert_refusal(&output, "the installed dots did not report version 0.5.0");
        assert_eq!(trace(&world), "curl\ninstall\nwhich\ndots --version\n");
        assert_eq!(fs::read_to_string(&launcher).unwrap(), "existing launcher");
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "existing config");
        assert!(!world.home().join(".config/mise/config.local.toml").exists());
    }
}
