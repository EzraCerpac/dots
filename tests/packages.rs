#[path = "support/github_env.rs"]
mod github_env;
mod support;

use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{FixtureCommand, World};
use toml_edit::DocumentMut;

const BASE: &str = "[vars]\ndots_profiles = \"desktop server\"\n";

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn failed(output: Output, code: i32) -> String {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    String::from_utf8(output.stderr).unwrap()
}

fn fixture(base: &str) -> World {
    let world = World::new();
    fs::write(world.root().join("config.toml"), format!("{BASE}{base}")).unwrap();
    fs::remove_file(world.root().join("dots/exceptions.tsv")).ok();
    world
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn bin(world: &World) -> PathBuf {
    let bin = world.home().join("bin");
    fs::create_dir_all(&bin).unwrap();
    bin
}

fn dots(world: &World) -> FixtureCommand<'_> {
    let mut cmd = world.dots(); // World clears the caller's environment.
    let home = world.home();
    let scratch = world.root().parent().unwrap();
    let bin = bin(world);
    let selected_dots = bin.join("dots");
    if !selected_dots.exists() {
        symlink(cmd.get_program(), &selected_dots).unwrap();
    }
    let old = env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let path = env::join_paths(std::iter::once(bin.clone()).chain(env::split_paths(&old))).unwrap();
    cmd.env("SETUP_PROFILE", "desktop")
        .env("SETUP_MACHINE_ID", "fixture")
        .env("DOTS_UP_BIN", "/nonexistent/dots-up")
        .env("MISE_TRUSTED_CONFIG_PATHS", scratch)
        .env("MISE_CEILING_PATHS", scratch)
        .env("MISE_AUTO_INSTALL", "0")
        .env("MISE_SELF_UPDATE_AVAILABLE", "0")
        .env("PATH", path)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("FIXTURE_SANDBOX", scratch);
    let mut defaults = github_env::private_environment(scratch, home, world.root());
    // Package operations switch MISE_CONFIG_DIR when staging one request.
    // A fixed global file would hide both the fixture config and those stages.
    defaults.retain(|(key, _)| key != "MISE_GLOBAL_CONFIG_FILE");
    let private = [
        ("HOME", home.to_owned()),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("XDG_STATE_HOME", home.join(".local/state")),
        ("XDG_CACHE_HOME", home.join(".cache")),
        ("CODEX_HOME", home.join(".codex")),
        ("CLAUDE_CONFIG_DIR", home.join(".claude")),
        ("TMPDIR", scratch.to_owned()),
        ("DOTS_ROOT", world.root().to_owned()),
        ("MISE_CONFIG_DIR", world.root().to_owned()),
        ("MISE_DATA_DIR", home.join(".local/share/mise")),
        ("MISE_STATE_DIR", home.join(".local/state/mise")),
        ("MISE_CACHE_DIR", home.join(".cache/mise")),
        ("MISE_SYSTEM_CONFIG_DIR", scratch.join("mise-system-config")),
        ("MISE_SYSTEM_DATA_DIR", scratch.join("mise-system-data")),
        ("MISE_TMP_DIR", scratch.join("mise-tmp")),
        ("MISE_INSTALLS_DIR", home.join(".local/share/mise/installs")),
        ("MISE_PLUGINS_DIR", home.join(".local/share/mise/plugins")),
    ];
    for directory in ["mise-system-config", "mise-system-data", "mise-tmp"] {
        fs::create_dir_all(scratch.join(directory)).unwrap();
    }
    cmd.envs(defaults.iter().map(|(key, path)| (key, path)))
        .envs(private.iter().map(|(key, path)| (key, path)));
    if bin.join("mise").exists() {
        cmd.env("DOTS_MISE_BIN", bin.join("mise"));
    }
    for (key, expected) in &private {
        let actual = cmd
            .get_envs()
            .find_map(|(name, value)| (name == *key).then_some(value).flatten());
        assert_eq!(
            actual,
            Some(expected.as_os_str()),
            "fixture must isolate {key}"
        );
        assert!(expected.starts_with(scratch), "{key} escaped the fixture");
    }
    for (key, _) in &defaults {
        if key != "DOTS_SKILL_LINKS" {
            let actual = cmd
                .get_envs()
                .find_map(|(name, value)| (name == key).then_some(value).flatten())
                .expect("private directory is set");
            assert!(
                Path::new(actual).starts_with(scratch),
                "{key:?} escaped the fixture"
            );
        }
    }
    cmd
}

fn fixture_child<'a>(world: &'a World, program: &Path) -> FixtureCommand<'a> {
    let environment = dots(world);
    let mut child = world.command(program);
    child
        .envs(
            environment
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .current_dir(world.root());
    child
}

fn document(world: &World, name: &str) -> DocumentMut {
    fs::read_to_string(world.root().join(name))
        .unwrap()
        .parse()
        .unwrap()
}

fn holds(world: &World, rule: &str) {
    let dots = world.root().join("dots");
    fs::create_dir_all(dots.join("exceptions")).unwrap();
    fs::write(dots.join("exceptions.tsv"), rule).unwrap();
    script(&dots.join("exceptions/owner"), "exit 0");
}

#[test]
fn unchanged_remove_suggests_only_a_unique_managed_match() {
    for (held, ambiguous) in [(false, false), (true, false), (true, true), (false, true)] {
        let formula = if ambiguous {
            "\"brew:codexbar\" = \"latest\"\n"
        } else {
            ""
        };
        let world = fixture(&format!(
            "\n[bootstrap.packages]\n\"brew-cask:codexbar\" = \"latest\"\n{formula}"
        ));
        if held {
            holds(&world, "base\towner\thold=brew-cask:codexbar\n");
        }
        let config = world.root().join("config.toml");
        let before = fs::read(&config).unwrap();
        let output = dots(&world)
            .args(["remove", "--base", "codex-bar"])
            .output()
            .unwrap();
        assert!(output.stdout.is_empty(), "{output:?}");
        let diagnostic = failed(output, 1);
        assert_eq!(diagnostic.lines().count(), 1, "{diagnostic}");
        assert!(diagnostic.contains("codex-bar"), "{diagnostic}");
        assert_eq!(
            diagnostic.contains("brew-cask:codexbar"),
            !held && !ambiguous,
            "{diagnostic}"
        );
        assert_eq!(fs::read(config).unwrap(), before);
    }
}

#[test]
fn unchanged_remove_hint_respects_unreadable_app_ownership() {
    let world = fixture("\n[tools]\n\"github:example/codexbar\" = \"latest\"\n");
    let markers = world.home().join(".local/state/dots/github-apps");
    fs::create_dir_all(&markers).unwrap();
    fs::write(markers.join("broken.json"), "{").unwrap();
    for keep in [false, true] {
        let mut command = dots(&world);
        command.arg("remove");
        if keep {
            command.arg("--keep-installed");
        }
        let error = failed(command.arg("codex-bar").output().unwrap(), 1);
        assert_eq!(error.contains("github:example/codexbar"), keep, "{error}");
    }
}

#[test]
fn unchanged_remove_accepts_alias_from_second_declaration() {
    let world = fixture(
        "\n[bootstrap.packages]\n\"brew:fd\" = \"latest\"\n\"brew:homebrew/core/fd\" = \"latest\"\n",
    );
    let config = world.root().join("config.toml");
    let before = fs::read(&config).unwrap();
    let out = success(
        dots(&world)
            .args([
                "remove",
                "--base",
                "--keep-installed",
                "--dry-run",
                "homebrew/core/fd",
            ])
            .output()
            .unwrap(),
    );
    assert!(out.contains("Forget brew:fd"), "{out}");
    assert_eq!(fs::read(config).unwrap(), before);
}

#[test]
fn new_add_and_remove_file_preserve_contents_mode_and_other_bytes() {
    let original = "# header\n\n[dotfiles] # declarations\n# keep this\n\"~/other\" = { source = \"dotfiles/other\", mode = \"symlink\" } # inline\n\n[tools]\nrg = \"latest\"  # keep order\n";
    let world = fixture(original);
    let config = world.root().join("config.toml");
    let before = fs::read_to_string(&config).unwrap();
    let target = world.home().join("directory with spaces/settings");
    fs::create_dir(target.parent().unwrap()).unwrap();
    fs::write(&target, b"first\0version\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    let mut add = dots(&world);
    add.current_dir(world.home())
        .args(["add", "--base", "./directory with spaces/settings"]);
    success(add.output().unwrap());
    let source = world.root().join("dotfiles/directory with spaces/settings");
    assert_eq!(fs::read_link(&target).unwrap(), source);
    assert_eq!(fs::read(&source).unwrap(), b"first\0version\n");
    assert_eq!(
        fs::metadata(&source).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let adopted = fs::read_to_string(&config).unwrap();
    assert_eq!(
        adopted
            .lines()
            .filter(|line| !line.contains("~/directory with spaces/settings"))
            .collect::<Vec<_>>(),
        before.lines().collect::<Vec<_>>()
    );
    success(add.output().unwrap());
    assert_eq!(fs::read_to_string(&config).unwrap(), adopted);
    fs::write(&target, b"edited through link\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    success(
        dots(&world)
            .args(["remove", "--dry-run"])
            .arg(&target)
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_link(&target).unwrap(), source);
    success(dots(&world).arg("remove").arg(&target).output().unwrap());
    assert!(fs::symlink_metadata(&target).unwrap().is_file());
    assert_eq!(fs::read(&target).unwrap(), b"edited through link\n");
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!source.exists());
    assert_eq!(fs::read_to_string(config).unwrap(), before);
}

#[test]
fn new_file_requests_reject_unsuitable_configurations_and_sources() {
    let world = fixture("");
    let target = world.home().join("settings");
    fs::write(&target, b"untouched").unwrap();
    let linked = world.root().join("outside.toml");
    let outside = world.home().join("outside.toml");
    fs::write(&outside, "").unwrap();
    symlink(&outside, &linked).unwrap();
    failed(
        dots(&world)
            .args(["add", "--path"])
            .arg(&linked)
            .arg(&target)
            .output()
            .unwrap(),
        2,
    );
    failed(
        dots(&world)
            .args(["add", "--base"])
            .arg(&target)
            .arg(world.home().join("./settings"))
            .output()
            .unwrap(),
        2,
    );
    let ignore = world.root().join(".gitignore");
    fs::write(&ignore, "/dotfiles/settings\n").unwrap();
    let error = failed(
        dots(&world)
            .args(["add", "--profile", "desktop"])
            .arg(&target)
            .output()
            .unwrap(),
        2,
    );
    assert!(error.contains("Git rules ignore"), "{error}");
    assert!(!world.root().join("config.desktop.toml").exists());
    fs::remove_file(ignore).unwrap();
    let spare = world.root().join("config.host-spare.toml");
    fs::write(
        &spare,
        "[dotfiles]\n\"~/other\" = { source = \"dotfiles/settings\", mode = \"symlink\" }\n",
    )
    .unwrap();
    failed(
        dots(&world)
            .args(["add", "--base"])
            .arg(&target)
            .output()
            .unwrap(),
        2,
    );
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
    assert!(!world.root().join("dotfiles/settings").exists());
}

#[test]
fn new_remove_file_refuses_an_unreadable_or_duplicate_declaration() {
    let world = fixture("");
    let target = world.home().join("adopted");
    fs::write(&target, "initial").unwrap();
    success(
        dots(&world)
            .args(["add", "--base"])
            .arg(&target)
            .output()
            .unwrap(),
    );
    let source = fs::read_link(&target).unwrap();
    let local = world.root().join("config.host-fixture.toml");
    for invalid in [
        "[[",
        "[dotfiles]\n\"~/another\" = { source = \"dotfiles/adopted\", mode = \"symlink\" }",
    ] {
        fs::write(&local, invalid).unwrap();
        failed(dots(&world).arg("remove").arg(&target).output().unwrap(), 2);
        assert_eq!(fs::read_link(&target).unwrap(), source);
        assert_eq!(fs::read_to_string(&source).unwrap(), "initial");
    }
}

fn fake_mise(world: &World, body: &str) {
    script(
        &bin(world).join("mise"),
        &format!(
            r#"
if [ "$1" = "--quiet" ]; then shift; fi
while [ "$1" = "-C" ] || [ "$1" = "-E" ]; do
    if [ "$1" = "-C" ]; then cd "$2"; fi
    shift 2
done
case "$*" in
  "config ls --json")
    IFS= read -r inventory < "$HOME/active.json" || :
    printf '%s\n' "$inventory"
    exit;;
  "--version") echo '2026.11.18 macos-x64'; exit;;
esac
{body}"#
        ),
    );
    active(world, &["config.toml"]);
    // Load the freshly created script before dots' tightly bounded offline probe.
    success(
        fixture_child(world, &bin(world).join("mise"))
            .args(["--quiet", "config", "ls", "--json"])
            .output()
            .unwrap(),
    );
}

fn active(world: &World, files: &[&str]) {
    let files: Vec<_> = files
        .iter()
        .map(|name| serde_json::json!({"path": world.root().join(name)}))
        .collect();
    fs::write(
        world.home().join("active.json"),
        serde_json::to_string(&files).unwrap(),
    )
    .unwrap();
}

#[test]
fn new_prune_failure_restores_exact_order_comments_and_mode() {
    let world = fixture(
        "\n[tools] # order matters\nzed = \"latest\"\n# keep nearby comment\nfixture = \"1\" # pin\nalpha = \"latest\"\n",
    );
    let path = world.root().join("config.toml");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::read(&path).unwrap();
    fake_mise(
        &world,
        r#"
printf '%s\n' "$*" >> "$HOME/mise-actions"
case "$1" in
  unuse) [ "$2" = --path ]; [ "$4" = --no-prune ]; printf '[tools]\n' > "$3";;
  prune)
    if [ "${FIXTURE_LATER_EDIT:-0}" = 1 ]; then
        printf '\n[env]\nLATER = "kept"\n' >> "$DOTS_ROOT/config.toml"
    fi
    exit 5;;
  *) exit 90;;
esac"#,
    );
    failed(
        dots(&world)
            .args(["remove", "--base", "fixture"])
            .output()
            .unwrap(),
        1,
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let actions = fs::read_to_string(world.home().join("mise-actions")).unwrap();
    assert!(actions.contains("unuse --path"), "{actions}");
    assert!(
        actions.contains("prune --yes --tools -- fixture"),
        "{actions}"
    );
    failed(
        dots(&world)
            .args(["remove", "--base", "fixture"])
            .env("FIXTURE_LATER_EDIT", "1")
            .output()
            .unwrap(),
        1,
    );
    let expected = [before, b"\n[env]\nLATER = \"kept\"\n".to_vec()].concat();
    assert_eq!(fs::read(&path).unwrap(), expected);
}

#[test]
fn new_selector_protects_other_active_declaration_and_fails_closed() {
    let world = fixture("\n[tools]\nfixture = \"1\" # base\n");
    let path = world.root().join("config.toml");
    let profile = world.root().join("config.desktop.toml");
    fs::write(&profile, "[tools]\nfixture = \"2\" # desktop\n").unwrap();
    fake_mise(
        &world,
        r#"printf '%s\n' "$*" >> "$HOME/mise-actions"; exit 90"#,
    );
    active(&world, &["config.toml", "config.desktop.toml"]);
    success(
        dots(&world)
            .args(["remove", "--base", "fixture"])
            .output()
            .unwrap(),
    );
    assert!(
        document(&world, "config.toml")["tools"]
            .get("fixture")
            .is_none()
    );
    assert_eq!(
        document(&world, "config.desktop.toml")["tools"]["fixture"].as_str(),
        Some("2")
    );
    assert!(!world.home().join("mise-actions").exists());
    fs::write(&path, format!("{BASE}\n[tools]\nfixture = \"1\"\n")).unwrap();
    fs::write(&profile, "[[").unwrap();
    failed(
        dots(&world)
            .args(["remove", "--base", "fixture"])
            .output()
            .unwrap(),
        2,
    );
    assert_eq!(
        document(&world, "config.toml")["tools"]["fixture"].as_str(),
        Some("1")
    );
    assert!(!world.home().join("mise-actions").exists());
    success(
        dots(&world)
            .args(["remove", "--base", "--keep-installed", "fixture"])
            .output()
            .unwrap(),
    );
}

fn package_backend(world: &World, platform: &str) {
    let mise = env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|path| path.join("mise"))
        .find(|path| path.is_file())
        .expect("mise on PATH");
    fs::write(world.home().join("real-mise"), mise.to_str().unwrap()).unwrap();
    fs::write(world.home().join("platform"), platform).unwrap();
    script(
        &bin(world).join("uname"),
        r#"
case "$1:$(cat "$HOME/platform")" in
  -s:macos) echo Darwin;;
  -s:linux) echo Linux;;
  *) exec /usr/bin/uname "$@";;
esac"#,
    );
    script(
        &bin(world).join("mise"),
        r#"
for directory in "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" \
    "$CLAUDE_CONFIG_DIR" "$CODEX_HOME" "$MISE_CONFIG_DIR" "$MISE_STATE_DIR" "$MISE_CACHE_DIR" \
    "$MISE_DATA_DIR" "$MISE_SYSTEM_CONFIG_DIR" "$MISE_SYSTEM_DATA_DIR" "$MISE_TMP_DIR" \
    "$MISE_DEFAULT_CONFIG_DIR" "$MISE_SYSTEM_DIR" "$MISE_PLUGINS_DIR" "$MISE_SYSTEM_PLUGINS_DIR" \
    "$MISE_SYSTEM_PLUGINS_DIRS" "$MISE_SYSTEM_SHIMS_DIR" \
    "$MISE_DOWNLOADS_DIR" "$MISE_INSTALLS_DIR" "$MISE_SYSTEM_INSTALLS_DIR" "$MISE_INSTALL_STORE_DIR" \
    "$MISE_SHARED_INSTALL_DIR" "$MISE_SHIMS_DIR" "$MISE_SYSTEM_CONFIG_FILE"; do
    case "$directory" in "$FIXTURE_SANDBOX"|"$FIXTURE_SANDBOX"/*) :;;
        *) echo 'mise fixture directory escaped the test sandbox' >&2; exit 91;;
    esac
done
if [ "$1" = "--quiet" ]; then shift; fi
while [ "$1" = "-C" ] || [ "$1" = "-E" ]; do
    if [ "$1" = "-C" ]; then cd "$2"; else export MISE_ENV="$2"; fi
    shift 2
done
real=$(cat "$HOME/real-mise")
case "$*" in
  "bootstrap packages where brew:mas")
    if [ -e "$HOME/installed/brew:mas" ] && [ -n "${FIXTURE_MAS_PREFIX:-}" ]; then
        printf '%s\n' "$FIXTURE_MAS_PREFIX"
    else
        exit 1
    fi
    exit;;
  "self-update"*|"upgrade"*|"bootstrap apply"*) exit 0;;
esac
case "$1 ${2:-} ${3:-}" in
  "bootstrap packages status")
    if [ "${FIXTURE_NATIVE_MAS:-0}" = 1 ]; then exec "$real" "$@"; fi
    options=$*
    if [ "${MISE_SYSTEM_PACKAGES_MANAGERS-unset}" != "" ] &&
       [ "${FIXTURE_RECORD_STATUS:-0}" = 1 ]; then
        printf '%s\n' "$options" >> "$HOME/package-calls"
    fi
    if [ "${MISE_SYSTEM_PACKAGES_MANAGERS-unset}" = "" ]; then
        MISE_SYSTEM_PACKAGES_MANAGERS= exec "$real" "$@"
    fi
    status=$(MISE_SYSTEM_PACKAGES_MANAGERS= "$real" "$@")
    printf '%s\n' "$status" | awk -v home="$HOME" '
      /^  "[^"]+": *[{]/ { manager = $1; gsub(/[":]/, "", manager) }
      /"available": false/ { sub(/false/, "true") }
      /"package": "/ {
        name = $2; gsub(/[",]/, "", name)
        marker = home "/installed/" manager ":" name
        installed = ((getline marker_line < marker) >= 0)
        close(marker)
      }
      /"state": "skipped"/ { sub(/skipped/, installed ? "installed" : "missing") }
      /"installed_version": ""/ && installed { sub(/""/, "\"1\"") }
      { print }
    ';;
  "bootstrap packages apply"|"bootstrap packages upgrade")
    if [ "${FIXTURE_NATIVE_MAS:-0}" = 1 ]; then
        case " $* " in *" --manager mas "*) exec "$real" "$@";; esac
    fi
    printf '%s\n' "$*" >> "$HOME/package-calls"
    shift 2
    operation=$1; shift
    mkdir -p "$HOME/installed"
    manager=all
    requested=
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --manager) manager=$2; shift;;
            --dry-run) exit 0;;
            *:*) requested="$requested $1";;
        esac
        shift
    done
    if [ -z "$requested" ]; then
        requested=$(MISE_SYSTEM_PACKAGES_MANAGERS= "$real" bootstrap packages status --json | awk -v wanted="$manager" '
            /^  "[^"]+": *[{]/ { manager = $1; gsub(/[":]/, "", manager) }
            /"package": "/ && (wanted == manager || wanted == "all") {
                name = $2; gsub(/[",]/, "", name)
                print manager ":" name
            }
        ')
    fi
    if [ "$operation" = apply ]; then
        if [ "${FIXTURE_SKIP_APPLY:-0}" = 1 ]; then exit 0; fi
        for key in $requested; do
            if [ "$manager" != all ] && [ "${key%%:*}" != "$manager" ]; then continue; fi
            case "$key" in *:fixture-broken) echo 'fixture package installer failed' >&2; exit 1;; esac
            case "$key" in */*) continue;; esac
            [ -e "$HOME/installed/$key" ] || touch "$HOME/installed/$key"
        done
    fi;;
  *) exec "$real" "$@";;
esac"#,
    );
    script(
        &bin(world).join("sudo"),
        r#"
if [ "$*" = "-n true" ] || [ "$*" = "-n -v" ]; then
    [ "${FIXTURE_SUDO:-0}" = 1 ]
elif [ "$1" = "--" ]; then shift; exec "$@"
else exec "$@"
fi"#,
    );
    script(&bin(world).join("apt-get"), "exit 90");
    script(&bin(world).join("brew"), "exit 90");
    script(
        &bin(world).join("mas"),
        r#"
printf '%s\n' "$*" >> "$HOME/mas.log"
case "$*" in
    "version") echo 'mas 1.0.0';;
    "list --json")
        if [ -e "$HOME/mas-installed" ]; then
            printf '[{"adamID":123,"name":"Fixture App","version":"1"}]\n'
        else
            printf '[{"adamID":999,"name":"Another Fixture App","version":"1"}]\n'
        fi;;
    "install 123") touch "$HOME/mas-installed";;
    "upgrade 123") :;;
    *) exit 90;;
esac"#,
    );
}

fn calls(world: &World) -> String {
    fs::read_to_string(world.home().join("package-calls")).unwrap_or_default()
}

fn installed(world: &World, key: &str) -> bool {
    world.home().join("installed").join(key).is_file()
}

fn up(world: &World) -> FixtureCommand<'_> {
    let mut command = dots(world);
    command
        .args(["--verbose", "up"])
        .env("DOTS_SOURCE_SYNC_DONE", "1");
    command
}

#[test]
fn unchanged_native_add_keeps_earlier_success_and_honors_versions() {
    let world = fixture("\n[bootstrap.packages] # kept\n\"apt:other\" = \"latest\" # own entry\n");
    package_backend(&world, "linux");
    let result = dots(&world)
        .args([
            "add",
            "--base",
            "apt:fixture-second@2",
            "apt:fixture-broken",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success(), "{result:?}");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("noninteractive run"),
        "{result:?}"
    );
    let config = document(&world, "config.toml");
    assert_eq!(
        config["bootstrap"]["packages"]["apt:fixture-second"].as_str(),
        Some("2")
    );
    assert!(
        config["bootstrap"]["packages"]
            .get("apt:fixture-broken")
            .is_none()
    );
    assert!(installed(&world, "apt:fixture-second"));
    assert!(!installed(&world, "apt:other"));
    assert!(
        fs::read_to_string(world.root().join("config.toml"))
            .unwrap()
            .contains("\"apt:other\" = \"latest\" # own entry")
    );
}

#[test]
fn unchanged_empty_host_never_calls_a_package_manager() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let world = fixture("");
    package_backend(&world, "macos");
    success(up(&world).output().unwrap());
    assert!(calls(&world).is_empty());
    fs::write(world.root().join("config.toml"), format!("{BASE}\n[settings.system_packages]\nmanagers = []\n[bootstrap.packages]\n\"brew:disabled-formula\" = \"latest\"\n")).unwrap();
    success(up(&world).output().unwrap());
    assert!(calls(&world).is_empty());
}

#[test]
fn unchanged_routine_packages_respect_exception_holds() {
    let world = fixture(
        "\n[bootstrap.packages]\n\"apt:held\" = \"latest\"\n\"apt:regular\" = \"latest\"\n",
    );
    package_backend(&world, "linux");
    holds(&world, "desktop\towner\thold=apt:held\n");
    success(up(&world).output().unwrap());
    assert!(installed(&world, "apt:regular"));
    assert!(!installed(&world, "apt:held"));
    let log = calls(&world);
    assert!(
        log.lines()
            .any(|call| call.contains("apply") && call.contains("apt:regular")),
        "{log}"
    );
    assert!(
        log.lines()
            .any(|call| call.contains("upgrade") && call.contains("apt:regular")),
        "{log}"
    );
    holds(&world, "server\towner\thold=apt:held\n");
    success(up(&world).output().unwrap());
    assert!(installed(&world, "apt:held"));
}

#[test]
fn unchanged_cask_discovery_respects_toml_forms_profiles_and_filters() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let world = fixture(
        "dots_self_updating_casks = \"manually-updated\"\n[bootstrap]\npackages = { \"brew-cask:inline-app\" = \"latest\", \"brew-cask:manually-updated\" = \"latest\", \"mas:456\" = \"latest\" }\n",
    );
    package_backend(&world, "macos");
    for (file, name, os) in [
        ("config.desktop.toml", "dotted-app", "macos"),
        ("config.host-fixture.toml", "filtered-app", "linux"),
        ("config.server.toml", "server-app", "macos"),
    ] {
        fs::write(
            world.root().join(file),
            format!("bootstrap.packages.\"brew-cask:{name}\" = {{ os = \"{os}\" }}\n"),
        )
        .unwrap();
    }
    let output = success(up(&world).output().unwrap());
    for name in ["inline-app", "dotted-app", "manually-updated"] {
        assert!(
            installed(&world, &format!("brew-cask:{name}")),
            "missing {name}\n{output}\n{}",
            calls(&world)
        );
    }
    for name in ["filtered-app", "server-app"] {
        assert!(!installed(&world, &format!("brew-cask:{name}")));
    }
    assert!(!world.home().join("mas.log").exists());
    let log = calls(&world);
    assert!(
        !log.lines()
            .any(|call| call.contains("upgrade") && call.contains("manually-updated")),
        "{log}"
    );
}

#[test]
fn unchanged_app_store_requires_opt_in_and_authorization() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let world = fixture("\n[bootstrap.packages]\n\"mas:123\" = \"latest\"\n");
    package_backend(&world, "macos");
    success(up(&world).output().unwrap());
    assert!(calls(&world).is_empty());
    let out = success(up(&world).arg("--mas").output().unwrap());
    assert!(out.contains("authorization"), "{out}");
    assert!(!installed(&world, "mas:123"));
    success(
        up(&world)
            .arg("--mas")
            .env("FIXTURE_SUDO", "1")
            .output()
            .unwrap(),
    );
    assert!(installed(&world, "mas:123"));
    let log = calls(&world);
    assert!(log.contains("apply") && log.contains("upgrade"), "{log}");
}

#[test]
fn unchanged_mas_resolves_bare_packages_and_respects_holds() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let world = fixture(
        "\n[settings.system_packages]\nprefer = [\"mas\"]\n[bootstrap.packages]\n\"123\" = \"latest\"\n\"124\" = \"latest\"\n",
    );
    package_backend(&world, "macos");
    holds(&world, "desktop\towner\thold=mas:124\n");
    let log = world.home().join("mas.log");
    let mas = || {
        let mut command = up(&world);
        command
            .env("FIXTURE_SUDO", "1")
            .env("FIXTURE_NATIVE_MAS", "1");
        command
    };
    success(mas().output().unwrap());
    assert!(!log.exists());
    success(mas().arg("--mas").output().unwrap());
    let log = fs::read_to_string(log).unwrap();
    assert!(
        log.contains("install 123") && log.contains("upgrade 123") && !log.contains("124"),
        "{log}"
    );
}

#[test]
fn unchanged_mas_tool_formula_keeps_its_normal_brew_pass() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let world = fixture("\n[bootstrap.packages]\n\"brew:mas\" = \"latest\"\n");
    package_backend(&world, "macos");
    success(up(&world).output().unwrap());
    assert!(installed(&world, "brew:mas"));
    assert!(!world.home().join("mas.log").exists());
}

fn no_path_mas(world: &World) -> FixtureCommand<'_> {
    let mut command = up(world);
    let fallback = env::var_os("PATH").unwrap_or_default();
    let mut path = vec![bin(world)];
    // The fallback may contain real mas or brew. Keep only tools the oracle
    // uses and put them in our fixture bin.
    for program in ["jq", "git", "gitleaks", "node", "uv"] {
        if let Some(source) = env::split_paths(&fallback)
            .map(|dir| dir.join(program))
            .find(|file| file.is_file())
        {
            let destination = bin(world).join(program);
            if !destination.exists() {
                symlink(source, destination).unwrap();
            }
        }
    }
    path.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    command
        .env("PATH", env::join_paths(path).unwrap())
        .env("FIXTURE_SUDO", "1")
        .arg("--mas");
    command
}

#[test]
fn unchanged_mas_defers_when_the_tool_and_brew_are_unavailable() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let world = fixture(
        "\n[settings.system_packages]\nmanagers = [\"mas\"]\n[bootstrap.packages]\n\"mas:123\" = \"latest\"\n",
    );
    package_backend(&world, "macos");
    fs::remove_file(bin(&world).join("mas")).unwrap();
    let output = success(no_path_mas(&world).output().unwrap());
    assert!(
        output.contains("mas") && output.contains("installed"),
        "{output}"
    );
    assert!(calls(&world).is_empty());
}

#[test]
fn unchanged_mas_can_use_the_brew_tool_outside_path() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let world = fixture("\n[bootstrap.packages]\n\"mas:123\" = \"latest\"\n");
    package_backend(&world, "macos");
    let prefix = world.home().join("mas-opt");
    fs::create_dir_all(prefix.join("bin")).unwrap();
    fs::rename(bin(&world).join("mas"), prefix.join("bin/mas")).unwrap();
    let command = || {
        let mut command = no_path_mas(&world);
        command.env("FIXTURE_NATIVE_MAS", "1");
        command
    };
    success(command().output().unwrap());
    let log = world.home().join("mas.log");
    assert!(!log.exists());
    fs::create_dir_all(world.home().join("installed")).unwrap();
    fs::write(world.home().join("installed/brew:mas"), "").unwrap();
    success(
        command()
            .env("FIXTURE_MAS_PREFIX", &prefix)
            .output()
            .unwrap(),
    );
    let log = fs::read_to_string(log).unwrap();
    assert!(
        log.contains("install 123") && log.contains("upgrade 123"),
        "{log}"
    );
}

#[test]
fn unchanged_status_and_plan_do_not_probe_the_app_store() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let world = fixture(
        "dots_source_repo = \"fixture/packages\"\n[bootstrap.packages]\n\"mas:123\" = \"latest\"\n",
    );
    package_backend(&world, "macos");
    success(
        world
            .git()
            .args(["init", "--initial-branch=dev"])
            .output()
            .unwrap(),
    );
    success(
        world
            .git()
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/fixture/packages.git",
            ])
            .output()
            .unwrap(),
    );
    success(world.git().args(["add", "config.toml"]).output().unwrap());
    success(
        world
            .git()
            .args(["commit", "-m", "Fixture config"])
            .output()
            .unwrap(),
    );
    let log = world.home().join("mas.log");
    for (command, allowed_call) in [("status", Some("version")), ("plan", None)] {
        success(
            dots(&world)
                .arg(command)
                .env("FIXTURE_NATIVE_MAS", "1")
                .output()
                .unwrap(),
        );
        if log.exists() {
            let calls = fs::read_to_string(&log).unwrap();
            assert!(
                calls.lines().all(|call| Some(call) == allowed_call),
                "{command} queried the App Store: {calls}"
            );
            fs::remove_file(&log).unwrap();
        }
    }
}

#[test]
fn unchanged_add_sync_up_and_repeat() {
    let a = fixture("");
    let b = fixture("");
    package_backend(&a, "linux");
    package_backend(&b, "linux");
    let remote = a.bare_remote("package-origin");
    let seed = |world: &World| {
        for name in ["dotfiles", "dots", "tests"] {
            let path = world.root().join(name);
            if path.is_dir() {
                fs::remove_dir_all(path).unwrap();
            }
        }
        for name in ["config.laptop.toml", "config.server.toml"] {
            let _ = fs::remove_file(world.root().join(name));
        }

        success(
            world
                .git()
                .args(["init", "--initial-branch=main"])
                .output()
                .unwrap(),
        );
        let setting = world.set_source_origin(&remote, "fixture/packages");
        fs::write(
            world.root().join("config.toml"),
            format!("{BASE}dots_source_repo = {setting:?}\n"),
        )
        .unwrap();
        fs::write(world.root().join(".gitignore"), "/miserc.toml\n/config.local.toml\n/config.host-*.toml\n/mise.lock\n/mise.*.lock\n/locks/\n/.setup-state/\n").unwrap();
    };
    seed(&a);
    seed(&b);
    success(
        a.git()
            .args(["add", "config.toml", ".gitignore"])
            .output()
            .unwrap(),
    );
    success(
        a.git()
            .args(["commit", "-m", "Initial fixture config"])
            .output()
            .unwrap(),
    );
    success(a.git().args(["push", "origin", "main"]).output().unwrap());
    success(a.git().args(["switch", "-c", "dev"]).output().unwrap());
    success(a.git().args(["push", "origin", "dev"]).output().unwrap());
    success(b.git().args(["fetch", "origin", "dev"]).output().unwrap());
    success(
        b.git()
            .args(["reset", "--hard", "origin/dev"])
            .output()
            .unwrap(),
    );
    success(b.git().args(["switch", "-C", "dev"]).output().unwrap());

    let plugin = a.root().with_file_name("fixture-plugin");
    fs::create_dir_all(plugin.join("bin")).unwrap();
    script(&plugin.join("bin/list-all"), "echo 1.0.0");
    script(
        &plugin.join("bin/install"),
        r#"
mkdir -p "$ASDF_INSTALL_PATH/bin"
printf '#!/bin/sh\necho fixture-tool\n' > "$ASDF_INSTALL_PATH/bin/fixture-tool"
chmod +x "$ASDF_INSTALL_PATH/bin/fixture-tool""#,
    );
    success(a.git().current_dir(&plugin).arg("init").output().unwrap());
    success(
        a.git()
            .current_dir(&plugin)
            .args(["add", "bin"])
            .output()
            .unwrap(),
    );
    success(
        a.git()
            .current_dir(&plugin)
            .args(["commit", "-m", "Fixture tool plugin"])
            .output()
            .unwrap(),
    );
    for world in [&a, &b] {
        let mut mise = fixture_child(world, &bin(world).join("mise"));
        success(
            mise.args(["plugins", "install", "fixture-tool"])
                .arg(format!("file://{}", plugin.display()))
                .output()
                .unwrap(),
        );
    }
    let tool = |world: &World| {
        world
            .home()
            .join(".local/share/mise/installs/fixture-tool/1.0.0/bin/fixture-tool")
    };
    let add = || {
        let mut cmd = dots(&a);
        cmd.args(["add", "--base", "fixture-tool@1.0.0", "apt:fixture-package"]);
        cmd
    };
    success(add().output().unwrap());
    assert!(installed(&a, "apt:fixture-package") && tool(&a).is_file());
    assert_eq!(
        document(&a, "config.toml")["tools"]["fixture-tool"].as_str(),
        Some("1.0.0")
    );
    let before = fs::read(a.root().join("config.toml")).unwrap();
    success(add().output().unwrap());
    assert_eq!(fs::read(a.root().join("config.toml")).unwrap(), before);
    let receipt = success(dots(&a).arg("sync").output().unwrap());
    let local_revision = success(a.git().args(["rev-parse", "HEAD"]).output().unwrap());
    let published_revision = success(
        a.git()
            .arg("--git-dir")
            .arg(&remote)
            .args(["rev-parse", "refs/heads/dev"])
            .output()
            .unwrap(),
    );
    assert_eq!(published_revision, local_revision, "{receipt}");
    assert!(!installed(&b, "apt:fixture-package") && !tool(&b).exists());
    success(dots(&b).args(["--verbose", "up"]).output().unwrap());
    assert!(installed(&b, "apt:fixture-package") && tool(&b).is_file());
    let mtimes = |world: &World| {
        [
            fs::metadata(tool(world)).unwrap().modified().unwrap(),
            fs::metadata(world.home().join("installed/apt:fixture-package"))
                .unwrap()
                .modified()
                .unwrap(),
        ]
    };
    let before = mtimes(&b);
    success(dots(&b).args(["--verbose", "up"]).output().unwrap());
    assert_eq!(mtimes(&b), before);
    assert!(
        success(
            b.git()
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .unwrap()
        )
        .is_empty()
    );
}

#[test]
fn new_native_remove_refuses_dependents_and_restores_a_failed_uninstall() {
    let world = fixture("\n[bootstrap.packages]\n\"apt:fixture\" = \"2\" # kept on failure\n");
    fake_mise(&world, "exit 91");
    script(
        &bin(&world).join("dpkg-query"),
        "printf 'fixture:amd64 installed\\n'",
    );
    script(&bin(&world).join("sudo"), "exec \"$@\"");
    let config = world.root().join("config.toml");
    let before = fs::read(&config).unwrap();
    for dependents in [true, false] {
        let additional = if dependents {
            "\nprintf 'Remv dependent [1]\\n'"
        } else {
            ""
        };
        script(
            &bin(&world).join("apt-get"),
            &format!(
                r#"
case "$1" in
    --simulate)
        printf 'Remv fixture [2]\n'{additional};;
    --yes)
        printf '%s\n' "$*" > "$HOME/apt-remove"
        exit 3;;
    *) exit 90;;
esac"#
            ),
        );
        failed(
            dots(&world)
                .args(["remove", "--base", "apt:fixture"])
                .output()
                .unwrap(),
            1,
        );
        assert_eq!(fs::read(&config).unwrap(), before);
        let log = fs::read_to_string(world.home().join("apt-remove")).unwrap_or_default();
        if dependents {
            assert!(log.is_empty());
        } else {
            assert_eq!(log.trim(), "--yes --no-auto-remove remove fixture:amd64");
        }
    }
}

#[test]
fn new_status_lists_the_adopted_target() {
    let world = fixture("");
    let target = world.home().join("status-fixture");
    fs::write(&target, "tracked as a link").unwrap();
    success(
        dots(&world)
            .args(["add", "--base"])
            .arg(&target)
            .output()
            .unwrap(),
    );
    let output = success(dots(&world).arg("status").output().unwrap());
    assert!(output.contains("status-fixture"), "{output}");
}

#[test]
fn new_qualified_homebrew_removal_leaves_the_other_tap_installed() {
    let world = fixture("\n[bootstrap.packages]\n\"brew:homebrew/core/fd\" = \"latest\"\n");
    fake_mise(&world, "exit 91");
    script(
        &bin(&world).join("brew"),
        r#"
case "$*" in
  "list --formula --full-name -1") echo vendor/tools/fd;;
  *) printf '%s\n' "$*" >> "$HOME/brew-mutation"; exit 91;;
esac"#,
    );
    success(
        dots(&world)
            .args(["remove", "--base", "brew:homebrew/core/fd"])
            .output()
            .unwrap(),
    );
    assert!(!world.home().join("brew-mutation").exists());
    assert!(
        document(&world, "config.toml")["bootstrap"]["packages"]
            .get("brew:homebrew/core/fd")
            .is_none()
    );
}

#[test]
fn new_file_removal_refuses_another_spelling_of_the_same_source() {
    let world = fixture("");
    let target = world.home().join("settings");
    fs::write(&target, "original").unwrap();
    success(
        dots(&world)
            .args(["add", "--base"])
            .arg(&target)
            .output()
            .unwrap(),
    );
    let source = fs::read_link(&target).unwrap();
    let alias = source.with_file_name("SETTINGS");
    if !alias.exists() {
        fs::hard_link(&source, &alias).unwrap();
    }
    let second_config = world.root().join("config.host-second.toml");
    fs::write(
        &second_config,
        "[dotfiles]\n\"~/other\" = { source = \"dotfiles/SETTINGS\", mode = \"symlink\" }\n",
    )
    .unwrap();
    let error = failed(dots(&world).arg("remove").arg(&target).output().unwrap(), 2);
    assert!(error.contains("config.host-second.toml"), "{error}");
    assert_eq!(fs::read_link(&target).unwrap(), source);
    assert_eq!(fs::read_to_string(alias).unwrap(), "original");
}

#[test]
fn new_interrupted_file_removal_names_parked_entries_and_the_declaration() {
    use sha2::{Digest, Sha256};
    let world = fixture("");
    let target = world.home().join("settings");
    fs::write(&target, "contents").unwrap();
    success(
        dots(&world)
            .args(["add", "--base"])
            .arg(&target)
            .output()
            .unwrap(),
    );
    let source = fs::read_link(&target).unwrap();
    let park = |path: &Path| {
        let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
        let prefix = u64::from_be_bytes(digest[..8].try_into().unwrap());
        let directory = path.with_file_name(format!(".dots-file-{prefix:016x}-fixture"));
        fs::create_dir(&directory).unwrap();
        let parked = directory.join("original");
        fs::rename(path, &parked).unwrap();
        parked
    };
    let saved_link = park(&target);
    fs::copy(&source, &target).unwrap();
    let saved_source = park(&source);
    let error = failed(dots(&world).arg("remove").arg(&target).output().unwrap(), 2);
    for path in [saved_link, saved_source, world.root().join("config.toml")] {
        assert!(error.contains(path.to_str().unwrap()), "{error}");
    }
    assert_eq!(fs::read_to_string(&target).unwrap(), "contents");
}

#[test]
fn new_add_refuses_disabled_native_manager_without_rewriting_pins() {
    let world = fixture(
        "\n[settings.system_packages]\nmanagers = []\n[bootstrap.packages]\n\"apt:fixture\" = \"2\" # pin\n",
    );
    package_backend(&world, "linux");
    let config = world.root().join("config.toml");
    let before = fs::read(&config).unwrap();
    let error = failed(
        dots(&world)
            .args(["add", "--base", "apt:fixture@3"])
            .output()
            .unwrap(),
        1,
    );
    assert!(
        error.contains("disabled") && error.contains("apt"),
        "{error}"
    );
    assert_eq!(fs::read(config).unwrap(), before);
    assert!(calls(&world).is_empty());
}

#[test]
fn new_add_applies_holds_to_mise_backed_requests_before_other_installs() {
    let world = fixture("");
    holds(&world, "server\towner\thold=npm:tool\n");
    fake_mise(&world, r#"touch "$HOME/unexpected-mise"; exit 91"#);
    let error = failed(
        dots(&world)
            .args(["add", "--base", "apt:other", "npm:@scope/tool@2"])
            .output()
            .unwrap(),
        2,
    );
    assert!(
        error.contains("held by installer exception owner"),
        "{error}"
    );
    assert!(!world.home().join("unexpected-mise").exists());
}

#[test]
fn unchanged_owned_app_stays_installed_while_another_project_uses_it() {
    use sha2::{Digest, Sha256};
    let world = fixture("\n[tools]\n\"github:example/release\" = \"latest\"\n");
    let name = "DotsFixtureUninstalledApplication.app";
    let markers = world.home().join(".local/state/dots/github-apps");
    fs::create_dir_all(&markers).unwrap();
    use std::fmt::Write as _;
    let mut marker_name = String::new();
    for byte in Sha256::digest(name.as_bytes()) {
        write!(marker_name, "{byte:02x}").unwrap();
    }
    let marker = markers.join(format!("{marker_name}.json"));
    let owner = serde_json::json!({
        "repo": "example/release",
        "path": format!("/Applications/{name}"),
        "bundle_id": "invalid.example.fixture"
    });
    fs::write(&marker, owner.to_string()).unwrap();
    let release = world.home().join("fixture-release");
    fs::create_dir(&release).unwrap();
    fs::write(release.join("binary"), "installed").unwrap();
    let other_project = world.root().with_file_name("other-project");
    fs::create_dir(&other_project).unwrap();
    let other_config = other_project.join("mise.toml");
    fs::write(
        &other_config,
        "[tools]\n\"github:example/release\" = \"latest\"\n",
    )
    .unwrap();
    fs::write(
        world.home().join("release-sources"),
        serde_json::json!([
            {"sources": [{"path": world.root().join("config.toml")}, {"path": other_config}]}
        ])
        .to_string(),
    )
    .unwrap();
    fake_mise(
        &world,
        r#"
case "$*" in
  "ls github:example/release --json --all-sources") cat "$HOME/release-sources";;
  "unuse "*)
      [ "$2" = --path ]
      [ "$4" = --no-prune ]
      [ "$5" = github:example/release ]
      printf '[vars]\ndots_profiles = "desktop server"\n\n[tools]\n' > "$3";;
  "prune --yes --tools github:example/release")
      grep -q 'github:example/release' "$FIXTURE_SANDBOX/other-project/mise.toml";;
  *) touch "$HOME/unexpected-mise"; exit 91;;
esac"#,
    );
    success(
        dots(&world)
            .args(["remove", "--base", "github:example/release"])
            .output()
            .unwrap(),
    );
    assert!(marker.exists());
    assert_eq!(
        fs::read_to_string(release.join("binary")).unwrap(),
        "installed"
    );
    assert!(!world.home().join("unexpected-mise").exists());
    assert!(
        document(&world, "config.toml")["tools"]
            .get("github:example/release")
            .is_none()
    );
}

#[test]
fn new_native_manager_success_without_install_leaves_no_declaration() {
    let world = fixture("");
    package_backend(&world, "linux");
    let profile = world.root().join("config.desktop.toml");
    fs::remove_file(&profile).ok();
    let error = failed(
        dots(&world)
            .args(["add", "pacman:fixture@2"])
            .env("FIXTURE_SKIP_APPLY", "1")
            .output()
            .unwrap(),
        1,
    );
    assert!(
        error.contains("did not confirm installation") && error.contains("pacman:fixture"),
        "{error}"
    );
    assert!(
        calls(&world).contains("apply"),
        "installer was never called"
    );
    assert!(!profile.exists());
}
