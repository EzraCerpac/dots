mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{FixtureCommand, World, stage_executable};

const BASE: &str = include_str!("r01_config_goldens/base.toml");
const ORIGIN: &str = "ssh://git@example.com/dots/history.git";
const SKILL: &[u8] = include_bytes!("../skills/dots/SKILL.md");

fn dots_bin() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("DOTS_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_dots").into()),
    );
    assert!(path.is_absolute(), "DOTS_BIN must be an absolute path");
    path
}

struct Fixture {
    world: World,
    base: PathBuf,
    home: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let world = World::new();
        let base = world.base().to_owned();
        let home = world.home().to_owned();
        let config = world.root().to_owned();
        fs::write(config.join("config.toml"), BASE).unwrap();
        // The R-01 scenarios create their own commands/hooks.
        fs::remove_dir_all(config.join("dots")).unwrap();
        Self {
            world,
            base,
            home,
            config,
        }
    }

    fn command(&self, executable: impl AsRef<OsStr>) -> FixtureCommand<'_> {
        let mut command = self.world.command(executable);
        command
            .current_dir(&self.base)
            .env("XDG_STATE_HOME", self.base.join("xdg-state"))
            .env("XDG_CONFIG_HOME", self.base.join("xdg-config"))
            .env("XDG_CACHE_HOME", self.base.join("xdg-cache"))
            .env("XDG_DATA_HOME", self.base.join("xdg-data"))
            .env("MISE_CONFIG_DIR", &self.config)
            .env("MISE_OFFLINE", "1")
            .env("MISE_NO_HOOKS", "1")
            .env("MISE_TRUSTED_CONFIG_PATHS", &self.config);
        command
    }

    fn dots(&self) -> FixtureCommand<'_> {
        self.command(dots_bin())
    }

    fn select(&self, args: &[&str]) -> Output {
        self.dots()
            .args([
                "profile",
                "--profile",
                "laptop",
                "--machine-id",
                "demo-machine",
            ])
            .args(args)
            .output()
            .unwrap()
    }

    fn write(&self, name: &str, contents: &str) {
        fs::write(self.config.join(name), contents).unwrap();
    }

    fn script(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.base.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Keep the executable's write descriptor out of the parallel
        // libtest process, where an unrelated fork could inherit it.
        let status = self
            .command("/bin/sh")
            .args([
                "-eu",
                "-c",
                r#"printf '%s\n' "$2" > "$1"; chmod 755 "$1""#,
                "fixture",
            ])
            .arg(&path)
            .arg(format!("#!/bin/sh\n{contents}\n"))
            .status()
            .unwrap();
        assert!(status.success(), "{status}");
        path
    }

    fn skill_state(&self) -> PathBuf {
        self.base.join("xdg-state/dots")
    }

    fn refresh_skill(&self) -> Output {
        self.dots()
            .env_remove("DOTS_SKILL_LINKS")
            .arg("--version")
            .output()
            .unwrap()
    }
}

fn successful(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn mode(path: &Path) -> u32 {
    path.metadata().unwrap().permissions().mode() & 0o777
}

#[test]
fn new_fresh_profile_bytes_match_the_legacy_fixture() {
    let f = Fixture::new();
    let output = String::from_utf8(successful(f.select(&["--origin", ORIGIN]))).unwrap();
    assert!(output.contains("laptop") && output.contains("demo-machine"));
    for (name, expected) in [
        (
            "miserc.toml",
            include_bytes!("r01_config_goldens/fresh/expected-miserc.toml").as_slice(),
        ),
        (
            "config.local.toml",
            include_bytes!("r01_config_goldens/fresh/expected-local.toml").as_slice(),
        ),
    ] {
        let path = f.config.join(name);
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(mode(&path), 0o600);
    }
}

#[test]
fn new_profile_edit_bytes_match_the_legacy_fixture() {
    let f = Fixture::new();
    f.write(
        "miserc.toml",
        include_str!("r01_config_goldens/managed/input-miserc.toml"),
    );
    let target = f.base.join("referent.toml");
    fs::write(
        &target,
        include_bytes!("r01_config_goldens/managed/input-local.toml"),
    )
    .unwrap();
    let outside = f.base.join("elsewhere");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, f.config.join("hop")).unwrap();
    let link = f.config.join("config.local.toml");
    symlink("hop/../referent.toml", &link).unwrap();
    f.write("referent.toml", "# literal path, distinct from referent");
    for _ in 0..2 {
        successful(f.select(&["--origin", ORIGIN]));
        assert_eq!(
            fs::read(f.config.join("miserc.toml")).unwrap(),
            include_bytes!("r01_config_goldens/managed/expected-miserc.toml")
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            include_bytes!("r01_config_goldens/managed/expected-local.toml")
        );
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("hop/../referent.toml")
        );
        assert_eq!(
            fs::read(f.config.join("referent.toml")).unwrap(),
            b"# literal path, distinct from referent"
        );
    }
}

#[test]
fn new_profile_persistence_tightens_both_existing_referents_to_0600() {
    let f = Fixture::new();
    let early = f.base.join("selector.toml");
    let local = f.base.join("local.toml");
    for (target, name, contents) in [
        (
            &early,
            "miserc.toml",
            include_str!("r01_config_goldens/fresh/expected-miserc.toml"),
        ),
        (
            &local,
            "config.local.toml",
            include_str!("r01_config_goldens/fresh/expected-local.toml"),
        ),
    ] {
        fs::write(target, contents).unwrap();
        fs::set_permissions(target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(target, f.config.join(name)).unwrap();
    }
    successful(f.select(&["--origin", ORIGIN]));
    assert_eq!(mode(&early), 0o600);
    assert_eq!(mode(&local), 0o600);
    assert_eq!(fs::read_link(f.config.join("miserc.toml")).unwrap(), early);
    assert_eq!(
        fs::read_link(f.config.join("config.local.toml")).unwrap(),
        local
    );
}

#[test]
fn new_profile_validation_keeps_both_files_and_modes() {
    let f = Fixture::new();
    let saved = [
        ("miserc.toml", "env = [\"server\", \"host-old\"]\n"),
        (
            "config.local.toml",
            "[env]\nSETUP_PROFILE = \"server\"\n[history.origin]\nbranch = \"host-old\"\n",
        ),
    ];
    for (name, source) in saved {
        f.write(name, source);
        fs::set_permissions(f.config.join(name), fs::Permissions::from_mode(0o640)).unwrap();
    }
    let output = f.select(&[]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mise rejected staged"), "{stderr}");
    for (name, source) in saved {
        assert_eq!(fs::read_to_string(f.config.join(name)).unwrap(), source);
        assert_eq!(mode(&f.config.join(name)), 0o640);
    }
    let output = f.select(&["--origin", "https://user:password@example.invalid/history"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("carries a password or token") && !stderr.contains("user:password"));
}

#[test]
fn new_profile_read_refuses_ambiguity_until_explicitly_repaired() {
    let f = Fixture::new();
    f.write(
        "miserc.toml",
        "env = [\"server\", \"laptop\", \"host-foo\", \"host-bar\", \"extra\"]\n",
    );
    f.write(
        "config.local.toml",
        "[env]\nSETUP_PROFILE = \"server\"\nSETUP_MACHINE_ID = \"bar\"\n",
    );
    let output = f
        .dots()
        .args(["profile"])
        .env("SETUP_PROFILE", "laptop")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("dots profile --profile PROFILE --machine-id ID")
    );
    successful(f.select(&[]));
    assert_eq!(
        fs::read(f.config.join("miserc.toml")).unwrap(),
        b"env = [\"laptop\", \"host-demo-machine\", \"extra\"]\n"
    );
    assert_eq!(
        successful(f.dots().arg("profile").output().unwrap()),
        b"laptop\n"
    );
    assert_eq!(
        successful(
            f.dots()
                .arg("profile")
                .env("SETUP_PROFILE", "server")
                .env("MISE_ENV", "laptop")
                .output()
                .unwrap()
        ),
        b"server\n"
    );
    f.write("config.local.toml", "[env]\nSETUP_PROFILE = \"server\"\n");
    let output = f.dots().arg("profile").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("records server"));
}

#[test]
fn rust_selection_validation_gives_mise_private_staged_directories() {
    let f = Fixture::new();
    let mise = f.script(
        "mise-check",
        r#"
set -eu
test "$HOME" != "$EXPECTED_FIXTURE"
case "$HOME" in "$EXPECTED_FIXTURE"/tmp/*) ;; *) exit 80 ;; esac
for dir in "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME" \
    "$TMPDIR" "$CODEX_HOME" "$CLAUDE_CONFIG_DIR" "$MISE_CONFIG_DIR" "$MISE_SYSTEM_CONFIG_DIR" \
    "$MISE_DATA_DIR" "$MISE_CACHE_DIR" "$MISE_STATE_DIR" "$MISE_TMP_DIR" \
    "$MISE_INSTALLS_DIR" "$MISE_INSTALL_STORE_DIR" "$MISE_DOWNLOADS_DIR" "$MISE_PLUGINS_DIR" \
    "$MISE_SHIMS_DIR" "$MISE_SYSTEM_DATA_DIR" "$MISE_SYSTEM_INSTALLS_DIR" \
    "$MISE_SYSTEM_PLUGINS_DIRS" "$MISE_SYSTEM_SHIMS_DIR"; do
    case "$dir" in "$HOME"|"$HOME"/*) ;; *) exit 81 ;; esac
done
test ! -e "$CODEX_HOME"
test ! -e "$CLAUDE_CONFIG_DIR"
test -f "$MISE_CONFIG_DIR/miserc.toml"
printf '[{"path":"%s/config.laptop.toml"},{"path":"%s/config.host-demo-machine.toml"},{"path":"%s/config.local.toml"}]\n' \
    "$MISE_CONFIG_DIR" "$MISE_CONFIG_DIR" "$MISE_CONFIG_DIR"
"#,
    );
    successful(
        f.command(env!("CARGO_BIN_EXE_dots"))
            .args([
                "profile",
                "--profile",
                "laptop",
                "--machine-id",
                "demo-machine",
            ])
            .env("DOTS_MISE_BIN", mise)
            .env("EXPECTED_FIXTURE", &f.base)
            .output()
            .unwrap(),
    );
}

#[test]
fn new_mise_queries_effective_settings_and_excludes_the_callers_env() {
    let f = Fixture::new();
    f.write("config.toml", &format!("{BASE}base_value = \"base\"\n"));
    f.write(
        "config.laptop.toml",
        "[vars]\nrole_value = \"laptop\"\ndots_profiles = \"decoy\"\n",
    );
    f.write(
        "config.host-demo-machine.toml",
        "[vars]\nhost_value = \"host\"\n",
    );
    f.write("config.server.toml", "[vars]\nrole_value = \"server\"\n");
    f.write("config.local.toml", "[vars]\nlocal_value = \"local\"\n");
    successful(f.select(&[]));
    let bytes = successful(
        f.dots()
            .args(["mise", "vars", "--json"])
            .env("MISE_ENV", "server")
            .output()
            .unwrap(),
    );
    let actual: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(actual["role_value"], "laptop");
    assert_eq!(actual["host_value"], "host");
    assert_eq!(actual["base_value"], "base");
    assert_eq!(actual["local_value"], "local");
    assert_eq!(actual["dots_profiles"], "decoy");
    assert_eq!(
        successful(f.dots().arg("profile").output().unwrap()),
        b"laptop\n"
    );
    assert_eq!(
        successful(
            f.dots()
                .args(["mise", "settings", "get", "exec_auto_install"])
                .env("MISE_EXEC_AUTO_INSTALL", "1")
                .output()
                .unwrap()
        ),
        b"false\n"
    );
}

#[test]
fn new_root_is_physical_and_passes_os_strings() {
    let f = Fixture::new();
    #[cfg(target_os = "linux")]
    let root = f.base.join(OsString::from_vec(b"raw-\xff".to_vec()));
    #[cfg(target_os = "macos")]
    let root = f.base.join("settings with spaces");
    fs::rename(&f.config, &root).unwrap();
    let child = root.join("child");
    fs::create_dir(&child).unwrap();
    symlink(&child, f.base.join("dotlink")).unwrap();
    let output = f
        .dots()
        .env("DOTS_ROOT", "dotlink/..")
        .arg("root")
        .output()
        .unwrap();
    let mut expected = root.as_os_str().as_bytes().to_vec();
    expected.push(b'\n');
    assert_eq!(successful(output), expected);
    let from_mise = f
        .dots()
        .env_remove("DOTS_ROOT")
        .env("MISE_CONFIG_DIR", "dotlink/..")
        .arg("root")
        .output()
        .unwrap();
    assert_eq!(successful(from_mise), expected);

    let status = f
        .dots()
        .env("DOTS_ROOT", "dotlink/../missing/deep")
        .arg("root")
        .output()
        .unwrap();
    assert_eq!(status.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&status.stderr).contains("missing/deep"));
    successful(
        f.dots()
            .env("DOTS_ROOT", "dotlink/../missing/deep")
            .arg("--version")
            .output()
            .unwrap(),
    );
    assert!(!root.join("missing").exists());
}

#[test]
fn new_configuration_command_keeps_literal_argv_cwd_env_and_status() {
    let f = Fixture::new();
    f.write("config.toml", "deliberately malformed [");
    f.script(
        "config/dots/commands/example",
        r#"
printf '[%s]\n' "$@"
printf '%s\n' "$PWD" "$DOTS_ROOT" "$DOTS_PROFILE"
test -z "${MISE_ENV+x}" && test -z "${DOTS_HOME+x}" || exit 91
printf 'child diagnostic\n' >&2
dots --version
exit 29
"#,
    );
    let caller = f.base.join("caller");
    fs::create_dir(&caller).unwrap();
    let raw = OsString::from_vec(b"raw-\xff".to_vec());
    let output = f
        .dots()
        .current_dir(&caller)
        .arg("example")
        .args(["--help", "two words", "", "*", "--"])
        .arg(raw)
        .env("DOTS_MISE_BIN", "/does/not/exist")
        .env("DOTS_PROFILE", "supplied")
        .env("DOTS_HOME", f.base.join("old"))
        .env("MISE_ENV", "server")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(29));
    assert_eq!(output.stderr, b"child diagnostic\n");
    let start = b"[--help]\n[two words]\n[]\n[*]\n[--]\n[raw-\xff]\n";
    assert!(output.stdout.starts_with(start), "{:?}", output.stdout);
    let rest = String::from_utf8_lossy(&output.stdout[start.len()..]);
    assert!(rest.starts_with(&format!(
        "{}\n{}\nsupplied\ndots ",
        caller.display(),
        f.config.display()
    )));
}

#[test]
fn new_configuration_help_lists_real_executables_and_builtins_win() {
    let f = Fixture::new();
    for name in [
        "root",
        "update",
        "uninstall",
        ".hidden",
        "with space",
        "internal",
    ] {
        f.script(&format!("config/dots/commands/{name}"), "echo unexpected");
    }
    f.script("config/dots/commands/example", r#"touch "$DOTS_ROOT/ran""#);
    f.write("dots/commands/nonexec", "unusable");
    let help = String::from_utf8(successful(f.dots().arg("help").output().unwrap())).unwrap();
    let section = help.split_once("Configuration commands").unwrap().1;
    assert!(section.contains("example [ARGS...]"));
    for excluded in [
        "root",
        "update",
        "uninstall",
        ".hidden",
        "with space",
        "internal",
        "nonexec",
    ] {
        assert!(!section.contains(&format!("{excluded} [ARGS...]")));
    }
    assert_eq!(
        successful(f.dots().arg("root").output().unwrap()),
        format!("{}\n", f.config.display()).as_bytes()
    );
    let output = f.dots().arg("nonexec").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("use dots help"));
    fs::remove_file(f.config.join("config.toml")).unwrap();
    let output = f.dots().arg("example").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("config.toml not found"));
    assert!(!f.config.join("ran").exists());
    let help = String::from_utf8(successful(f.dots().arg("help").output().unwrap())).unwrap();
    assert!(!help.contains("example [ARGS...]"));
}

#[test]
fn new_mise_override_and_program_arguments_are_literal() {
    let f = Fixture::new();
    let fake = f.script(
        "fake mise",
        r#"
printf '<%s>\n' "$@"
printf '%s\n' "$PWD" "$MISE_CONFIG_DIR" "$MISE_ENV" "$MISE_AUTO_INSTALL" "$MISE_EXEC_AUTO_INSTALL"
printf 'mise diagnostic\n' >&2
exit 43
"#,
    );
    let output = f
        .dots()
        .args(["mise", "--help", "--", "two words", "*", ""])
        .arg(OsString::from_vec(b"\xff".to_vec()))
        .env("DOTS_MISE_BIN", fake)
        .env("SETUP_PROFILE", "laptop")
        .env("SETUP_MACHINE_ID", "example")
        .env("MISE_ENV", "server")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(43));
    assert_eq!(output.stderr, b"mise diagnostic\n");
    let prefix = b"<--help>\n<-->\n<two words>\n<*>\n<>\n<\xff>\n";
    assert!(output.stdout.starts_with(prefix));
    assert_eq!(
        &output.stdout[prefix.len()..],
        format!(
            "{}\n{}\nlaptop,host-example\n0\n0\n",
            f.config.display(),
            f.config.display()
        )
        .as_bytes()
    );
    let bad = f
        .dots()
        .args(["mise", "--help"])
        .env("DOTS_MISE_BIN", "")
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("DOTS_MISE_BIN is not executable"));
}

#[test]
fn new_running_dots_survives_an_upgrade_during_a_configuration_command() {
    let f = Fixture::new();
    let install = f
        .base
        .join("mise/installs/github-ezracerpac-dots/v1/bin/dots");
    fs::create_dir_all(install.parent().unwrap()).unwrap();
    stage_executable(dots_bin(), &install).unwrap();
    f.script(
        "config/dots/commands/replace",
        r#"
set -eu
rm "$1"
mkdir -p "${1%/*}"
printf '#!/bin/sh\nprintf "new version\\n"\n' > "$1"
chmod +x "$1"
command -v dots
dots --version
"#,
    );
    let output = successful(
        f.command(&install)
            .args([OsStr::new("replace"), install.as_os_str()])
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", install.parent().unwrap().display()),
            )
            .output()
            .unwrap(),
    );
    let output = std::str::from_utf8(&output).unwrap();
    let (pinned, version) = output.split_once('\n').unwrap();
    assert!(version.starts_with(&format!("dots {} ", env!("CARGO_PKG_VERSION"))));
    let parent = Path::new(pinned).parent().unwrap();
    assert!(parent.starts_with(&f.base));
    assert!(!parent.exists());
}

#[test]
fn new_install_method_uses_the_running_binary_location() {
    let f = Fixture::new();
    for (relative, kind) in [
        ("brew/Cellar/dots/1.0/bin/dots", "homebrew"),
        ("mise/installs/github-ezracerpac-dots/1.0/bin/dots", "mise"),
        ("cargo/bin/dots", "cargo"),
        ("archive/dots", "other"),
    ] {
        let path = f.base.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        stage_executable(dots_bin(), &path).unwrap();
        let link = f.base.join(format!("launch-{kind}"));
        symlink(&path, &link).unwrap();
        let output = successful(
            f.command(link)
                .arg("--version")
                .env("CARGO_HOME", f.base.join("cargo"))
                .output()
                .unwrap(),
        );
        assert_eq!(
            output,
            format!("dots {} ({kind})\n", env!("CARGO_PKG_VERSION")).as_bytes()
        );
    }
}

#[test]
fn unchanged_codex_only_gets_the_shared_skill() {
    let f = Fixture::new();
    fs::create_dir(f.home.join(".codex")).unwrap();
    successful(f.refresh_skill());
    let link = f.home.join(".agents/skills/dots");
    assert_dots_skill_link(&link);
    assert!(!f.home.join(".claude").exists());
}

fn assert_dots_skill_link(link: &Path) {
    assert!(link.is_symlink(), "{} is not a symlink", link.display());
    let skill = fs::read_to_string(link.join("SKILL.md")).unwrap();
    assert!(skill.lines().any(|line| line == "name: dots"));
}

#[test]
fn new_missing_codex_root_inside_the_shared_skills_tree_stays_absent() {
    let f = Fixture::new();
    fs::create_dir(f.home.join(".agents")).unwrap();
    let codex = f.home.join(".agents/skills");
    successful(
        f.dots()
            .env("CODEX_HOME", &codex)
            .env_remove("DOTS_SKILL_LINKS")
            .arg("--version")
            .output()
            .unwrap(),
    );
    assert!(!codex.exists());
    assert!(codex.join("dots").symlink_metadata().is_err());
}

#[test]
fn unchanged_skill_link_updates_legacy_ownership_but_leaves_user_replacements() {
    let f = Fixture::new();
    let skills = f.home.join(".claude/skills");
    let link = skills.join("dots");
    let old = f.base.join("old-install/skills/dots");
    fs::create_dir_all(&skills).unwrap();
    fs::create_dir_all(f.skill_state()).unwrap();
    symlink(&old, &link).unwrap();
    fs::write(
        f.skill_state().join("skill-links"),
        format!("{}\t{}\n", link.display(), old.display()),
    )
    .unwrap();
    successful(f.refresh_skill());
    assert_dots_skill_link(&link);
    assert_ne!(fs::read_link(&link).unwrap(), old);
    fs::remove_file(&link).unwrap();
    symlink(&old, &link).unwrap();
    let first = f.refresh_skill();
    assert!(first.status.success(), "{first:?}");
    assert!(String::from_utf8_lossy(&first.stderr).contains("left "));
    let second = f.refresh_skill();
    assert!(second.status.success(), "{second:?}");
    assert!(second.stderr.is_empty(), "{second:?}");
    assert_eq!(fs::read_link(link).unwrap(), old);
}

#[test]
fn unchanged_skill_link_stays_inside_the_agent_directory() {
    let f = Fixture::new();
    fs::create_dir(f.home.join(".claude")).unwrap();
    let outside = f.base.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, f.home.join(".claude/skills")).unwrap();
    let output = f.refresh_skill();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("leads outside"));
    assert!(outside.join("dots").symlink_metadata().is_err());
}

#[test]
fn unchanged_completion_loader_leaves_skill_links_alone() {
    let f = Fixture::new();
    fs::create_dir(f.home.join(".claude")).unwrap();
    // Check side effects while the Rust completion loader is still a stub.
    f.dots()
        .env_remove("DOTS_SKILL_LINKS")
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    assert!(!f.home.join(".claude/skills").exists());
    assert!(!f.base.join("xdg-state").exists());
}

#[test]
fn rust_skill_extraction_refreshes_the_embedded_copy() {
    let f = Fixture::new();
    fs::create_dir(f.home.join(".codex")).unwrap();
    successful(f.refresh_skill());
    let link = f.home.join(".agents/skills/dots");
    let source = f.skill_state().join("skills/dots");
    assert_eq!(fs::read_link(&link).unwrap(), source);
    let skill = source.join("SKILL.md");
    assert_eq!(fs::read(link.join("SKILL.md")).unwrap(), SKILL);
    fs::write(&skill, b"old contents").unwrap();
    fs::remove_file(&link).unwrap();
    symlink(f.base.join("user-skill"), link).unwrap();
    successful(f.refresh_skill());
    assert_eq!(fs::read(skill).unwrap(), SKILL);
}

#[test]
fn rust_skill_extraction_skips_private_completion_and_opt_out() {
    let f = Fixture::new();
    fs::create_dir(f.home.join(".claude")).unwrap();
    f.dots()
        .env_remove("DOTS_SKILL_LINKS")
        .arg("__complete")
        .output()
        .unwrap();
    assert!(!f.skill_state().exists());
    successful(f.dots().arg("--version").output().unwrap());
    assert!(!f.skill_state().exists());
}

#[test]
fn new_mise_exec_uses_the_configured_dots() {
    let f = Fixture::new();
    let shadow = f.script("tool/bin/dots", "echo 'mise-selected dots'");
    f.write(
        "config.toml",
        &format!(
            "{BASE}\n[tools]\nnode = \"path:{}\"\n",
            shadow.parent().unwrap().parent().unwrap().display()
        ),
    );
    let output = f
        .dots()
        .args(["mise", "exec", "--", "dots"])
        .output()
        .unwrap();
    assert_eq!(successful(output), b"mise-selected dots\n");
}

#[test]
fn rust_owned_launch_restores_pinned_dots_after_mise_preserving_exec_contract() {
    use std::process::Stdio;

    let f = Fixture::new();
    let shadow = f.script("tool/bin/dots", "echo 'mise-selected dots'");
    f.write(
        "config.toml",
        &format!(
            "{BASE}\n[tools]\nnode = \"path:{}\"\n[env]\nFROM_MISE = \"resolved\"\nREMOVED = false\n",
            shadow.parent().unwrap().parent().unwrap().display()
        ),
    );
    let executable = dots_bin();
    let pinned_snapshot = f.base.join("pinned-executable");
    f.script(
        "config/tests/run",
        r#"
set -eu
printf '[%s]\n' "$@"
pinned=$(command -v dots)
cat "$pinned" > "$FIXTURE_PINNED_SNAPSHOT"
printf '%s\n' "$PWD" "$FROM_MISE" "${REMOVED-unset}" "$pinned"
cat
cat <&9
printf 'hook stderr\n' >&2
exit 39
"#,
    );
    let fd = f.base.join("fd");
    fs::write(&fd, "inherited fd\n").unwrap();
    let input = f.base.join("input");
    fs::write(&input, "inherited stdin\n").unwrap();
    let launcher = f.script(
        "launch",
        r#"
exec 9<"$1"
shift
exec "$@"
"#,
    );
    let output = f
        .command(launcher)
        .current_dir(&f.config)
        .env("REMOVED", "from caller")
        .env("FIXTURE_PINNED_SNAPSHOT", &pinned_snapshot)
        .args([&fd, &executable])
        .args(["test", "--help", "one two", ""])
        .arg(OsString::from_vec(vec![0xff]))
        .stdin(Stdio::from(fs::File::open(input).unwrap()))
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(39),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stderr, b"hook stderr\n");
    assert!(
        fs::read(pinned_snapshot).unwrap() == fs::read(executable).unwrap(),
        "the pinned executable differs from the running dots binary"
    );
    let prefix = b"[--help]\n[one two]\n[]\n[\xff]\n";
    assert_eq!(&output.stdout[..prefix.len()], prefix);
    let tail = std::str::from_utf8(&output.stdout[prefix.len()..]).unwrap();
    let lines: Vec<_> = tail.lines().collect();
    let [cwd, resolved, removed, pinned, stdin, fd] = lines.as_slice() else {
        panic!("unexpected hook output: {tail:?}");
    };
    assert_eq!(Path::new(cwd), f.config);
    assert_eq!((*resolved, *removed), ("resolved", "unset"));
    assert_eq!((*stdin, *fd), ("inherited stdin", "inherited fd"));
    let pinned = Path::new(pinned);
    assert!(pinned.starts_with(&f.base), "pinned path: {pinned:?}");
    assert_eq!(pinned.file_name(), Some(OsStr::new("dots")));
    assert!(
        pinned
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .as_bytes()
            .starts_with(b"dots-run-"),
        "pinned path: {pinned:?}"
    );
}
