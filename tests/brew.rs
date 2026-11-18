mod support;

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use support::World;

const TOKEN: &str = "fixture/tools/widget";
const CASK: &str = "widget";
const RECIPE: &str = r#"#!/bin/sh
if [ "$1" = --prefix ]; then
    printf '/opt/homebrew\n'
    if [ "${CAPTURE_CHILD:-}" = 1 ]; then
        printf '%s\n' "$DOTS_ROOT" "${MISE_ENV-unset}" > "$FIXTURE/child-environment"
        /bin/cat "$(command -v dots)" > "$FIXTURE/child-dots"
    fi
    exit 0
fi
[ "$HOMEBREW_NO_AUTO_UPDATE:$HOMEBREW_NO_AUTOREMOVE:$HOMEBREW_NO_INSTALL_CLEANUP" = 1:1:1 ] || exit 70
printf '%s\n' "$*" >> "$FIXTURE/calls"
case "$1" in
    list)
        [ -f "$FIXTURE/version" ] || exit 1
        if [ "$2" = --versions ]; then
            [ ! -f "$FIXTURE/unknown" ] || exit 1
            printf 'widget %s\n' "$(/bin/cat "$FIXTURE/version")"
        fi
        ;;
    outdated)
        if [ -f "$FIXTURE/outdated" ]; then
            printf 'widget\n'
            exit 1
        fi
        [ ! -f "$FIXTURE/probe-fails" ] || exit 1
        ;;
    install|upgrade)
        [ ! -f "$FIXTURE/fail" ] || exit 3
        if [ "$1" = install ]; then
            printf 1 > "$FIXTURE/version"
        else
            printf 2 > "$FIXTURE/version"
        fi
        ;;
    *) exit 70 ;;
esac
"#;

struct Fixture {
    world: World,
    directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let world = World::new();
        let directory = world.home().join("brew fixture");
        fs::create_dir(&directory).unwrap();
        executable(&directory.join("brew"), RECIPE);
        executable(
            &directory.join("pgrep"),
            r#"#!/bin/sh
[ "$#" = 3 ] && [ "$1" = -x ] && [ "$2" = -- ] && [ "$3" = 'Widget\.Helper' ] || exit 2
[ -f "$FIXTURE/running" ]
"#,
        );
        executable(
            &directory.join("sudo"),
            r#"#!/bin/sh
[ "$#" = 2 ] && [ "$1" = -n ] && [ "$2" = true ] && [ -f "$FIXTURE/admin" ]
"#,
        );
        Self { world, directory }
    }

    fn command(&self) -> std::process::Command {
        let mut command = self.world.dots();
        command
            .arg("brew")
            .env(
                "PATH",
                env::join_paths([
                    self.directory.as_path(),
                    Path::new("/usr/bin"),
                    Path::new("/bin"),
                ])
                .unwrap(),
            )
            .env("FIXTURE", &self.directory)
            .env("DOTS_CHANGES", self.directory.join("changes"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn write(&self, file: &str, value: &str) {
        fs::write(self.directory.join(file), value).unwrap();
    }

    fn remove(&self, file: &str) {
        fs::remove_file(self.directory.join(file)).unwrap();
    }

    fn read(&self, file: &str) -> String {
        match fs::read_to_string(self.directory.join(file)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("{file}: {error}"),
        }
    }
}

fn executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn status(output: &Output, code: i32) {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).unwrap()
}

fn skipped(fixture: &Fixture, args: &[&str], token: &str) -> bool {
    if cfg!(target_os = "macos") {
        return false;
    }
    let output = fixture.run(args);
    status(&output, 4);
    assert!(stdout(&output).contains(&format!("{token} is macOS-only")));
    assert!(fixture.read("calls").is_empty());
    true
}

#[test]
fn new_brew_path() {
    let fixture = Fixture::new();
    executable(&fixture.directory.join("dots"), "#!/bin/sh\nexit 99\n");
    let empty = fixture.world.home().join("empty");
    fs::create_dir(&empty).unwrap();
    let mut command = fixture.command();
    let expected = command.get_program().to_owned();
    let output = command
        .arg("path")
        .env("PATH", &fixture.directory)
        .env("DOTS_ROOT", &empty)
        .env("MISE_ENV", "ignored")
        .env("CAPTURE_CHILD", "1")
        .output()
        .unwrap();
    status(&output, 0);
    assert_eq!(
        stdout(&output).trim_end(),
        fixture.directory.join("brew").to_str().unwrap()
    );
    assert_eq!(
        fixture.read("child-environment"),
        format!("{}\nunset\n", empty.display())
    );
    let child = fs::read(fixture.directory.join("child-dots"))
        .expect("Homebrew fixture did not capture dots on its PATH");
    assert!(
        child == fs::read(expected).unwrap(),
        "Homebrew child did not find the running dots executable first on PATH"
    );
}

#[test]
fn new_brew_version() {
    let fixture = Fixture::new();
    fixture.write("version", "1.2_1");
    let output = fixture.run(&["version", "formula", TOKEN]);
    status(&output, 0);
    assert_eq!(stdout(&output), "1.2_1\n");
    fixture.write("version", "3.1,245");
    let output = fixture.run(&["version", "cask", CASK]);
    status(&output, 0);
    assert_eq!(stdout(&output), "3.1,245\n");
    fixture.write("unknown", "");
    let output = fixture.run(&["version", "cask", CASK]);
    status(&output, 1);
    assert!(output.stdout.is_empty());
}

#[test]
fn new_brew_admin_available() {
    let fixture = Fixture::new();
    let output = fixture.run(&["admin-available"]);
    status(&output, i32::from(!nix::unistd::geteuid().is_root()));
    fixture.write("admin", "");
    status(&fixture.run(&["admin-available"]), 0);
}

#[test]
fn new_brew_defer_for_admin() {
    let fixture = Fixture::new();
    let output = fixture.run(&["defer-for-admin", CASK]);
    status(&output, 3);
    assert!(stdout(&output).contains("widget needs interactive administrator access"));
    assert!(stdout(&output).contains("sudo -v && dots up"));
    assert!(fixture.read("calls").is_empty());
}

#[test]
fn new_brew_record() {
    let fixture = Fixture::new();
    status(&fixture.run(&["record", "formula", TOKEN, "installed"]), 0);
    assert!(fixture.read("changes").is_empty());
    fixture.write("version", "1");
    status(&fixture.run(&["record", "formula", TOKEN, "installed"]), 0);
    status(
        &fixture.run(&["record", "formula", TOKEN, "updated", "1"]),
        0,
    );
    status(&fixture.run(&["record", "formula", TOKEN, "updated"]), 0);
    assert_eq!(
        fixture.read("changes"),
        format!("brew-formula:{TOKEN}\tnot installed\t1\n")
    );
    fixture.write("version", "2");
    fixture.write("unknown", "");
    status(
        &fixture.run(&["record", "formula", TOKEN, "updated", "1"]),
        0,
    );
    fixture.remove("unknown");
    status(
        &fixture.run(&["record", "formula", TOKEN, "updated", "1"]),
        0,
    );
    status(
        &fixture
            .command()
            .env_remove("DOTS_CHANGES")
            .args(["record", "formula", TOKEN, "installed"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(
        fixture.read("changes"),
        format!("brew-formula:{TOKEN}\tnot installed\t1\nbrew-formula:{TOKEN}\t1\t2\n")
    );
}

#[test]
fn new_brew_update_cask() {
    let fixture = Fixture::new();
    let update = [
        "update-cask",
        "--auto-updates",
        "--admin",
        CASK,
        "Widget.Helper",
    ];
    if skipped(&fixture, &update, CASK) {
        return;
    }
    fixture.write("running", "");
    let output = fixture.run(&update);
    status(&output, 3);
    assert!(stdout(&output).contains("Widget.Helper is running"));
    assert!(!fixture.read("calls").contains("install "));
    fixture.remove("running");
    if !nix::unistd::geteuid().is_root() {
        let output = fixture.run(&update);
        status(&output, 3);
        assert!(stdout(&output).contains("sudo -v && dots up"));
        assert!(!fixture.read("calls").contains("install "));
    }
    fixture.write("admin", "");
    status(&fixture.run(&update), 0);
    fixture.write("outdated", "");
    fixture.write("running", "");
    let output = fixture.run(&["update-cask", CASK, "Widget.Helper", "--install-only"]);
    status(&output, 0);
    assert_eq!(stdout(&output), "Already installed: widget\n");
    assert!(!fixture.read("calls").contains("outdated"));
    let output = fixture.run(&update);
    status(&output, 3);
    assert!(stdout(&output).contains("before upgrading"));
    fixture.remove("running");
    status(&fixture.run(&update), 0);
    let calls = fixture.read("calls");
    assert!(calls.lines().any(|line| line == "install --cask widget"));
    assert!(
        calls
            .lines()
            .any(|line| line == "outdated --cask --quiet --greedy widget")
    );
    assert!(
        calls
            .lines()
            .any(|line| line == "upgrade --cask --greedy widget")
    );
    assert_eq!(
        fixture.read("changes"),
        "brew-cask:widget\tnot installed\t1\nbrew-cask:widget\t1\t2\n"
    );
}

#[test]
fn new_brew_update_formula() {
    let fixture = Fixture::new();
    let update = ["update-formula", TOKEN];
    if skipped(&fixture, &update, TOKEN) {
        return;
    }
    let invalid = fixture.run(&["update-formula", "--", "--all"]);
    status(&invalid, 2);
    assert!(fixture.read("calls").is_empty());

    status(
        &fixture.run(&["update-formula", TOKEN, "--install-only"]),
        0,
    );
    fixture.write("probe-fails", "");
    status(&fixture.run(&update), 1);
    assert!(!fixture.read("calls").contains("upgrade "));
    fixture.remove("probe-fails");

    fixture.write("outdated", "");
    status(
        &fixture.run(&["update-formula", TOKEN, "--install-only"]),
        0,
    );
    assert!(!fixture.read("calls").contains("upgrade "));
    fixture.write("fail", "");
    status(&fixture.run(&update), 1);
    assert_eq!(
        fixture.read("changes"),
        format!("brew-formula:{TOKEN}\tnot installed\t1\n")
    );
    fixture.remove("fail");
    status(&fixture.run(&update), 0);
    let calls = fixture.read("calls");
    assert!(
        calls
            .lines()
            .any(|line| line == format!("upgrade --formula {TOKEN}"))
    );
    assert_eq!(
        fixture.read("changes"),
        format!("brew-formula:{TOKEN}\tnot installed\t1\nbrew-formula:{TOKEN}\t1\t2\n")
    );
}

#[test]
fn new_brew_hold_formula() {
    let fixture = Fixture::new();
    let hold = ["hold-formula", TOKEN, "2"];
    if skipped(&fixture, &hold, TOKEN) {
        return;
    }
    let output = fixture.run(&hold);
    status(&output, 1);
    assert!(stderr(&output).contains("Refusing"));
    fixture.write("version", "1 2");
    let output = fixture.run(&hold);
    status(&output, 1);
    assert!(stderr(&output).contains("multiple installed versions (1 2)"));
    status(&fixture.run(&["hold-formula", TOKEN, "1 2"]), 1);
    fixture.write("version", "2");
    let output = fixture.run(&["hold-formula", TOKEN, "2", "fixture hold"]);
    status(&output, 0);
    assert_eq!(
        stdout(&output),
        format!("Held: {TOKEN} 2 (fixture hold).\n")
    );
    assert!(
        fixture
            .read("calls")
            .lines()
            .all(|line| line.starts_with("list --versions --formula "))
    );
    assert!(fixture.read("changes").is_empty());
}

#[test]
fn new_brew_hold_cask() {
    let fixture = Fixture::new();
    let app = fixture.directory.join("Widget Test.app");
    let app_path = app.to_str().unwrap();
    let hold = ["hold-cask", CASK, "2", app_path, "fixture hold"];
    if skipped(&fixture, &hold, CASK) {
        return;
    }
    let output = fixture.run(&hold);
    status(&output, 1);
    assert!(stderr(&output).contains("Refusing"));
    fs::create_dir_all(app.join("Contents")).unwrap();
    fs::write(
        app.join("Contents/Info.plist"),
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>CFBundleShortVersionString</key><string>2</string><key>CFBundleVersion</key><string>999</string></dict></plist>\n",
    )
    .unwrap();
    let output = fixture.run(&["hold-cask", CASK, "999", app_path]);
    status(&output, 1);
    assert!(stderr(&output).contains("widget is 2"));
    let output = fixture.run(&hold);
    status(&output, 0);
    assert_eq!(stdout(&output), "Held: widget 2 (fixture hold).\n");
    assert!(fixture.read("calls").is_empty());
    assert!(fixture.read("changes").is_empty());
}
