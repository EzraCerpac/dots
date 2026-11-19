mod support;

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Output};
use std::time::{Duration, Instant};

use nix::libc;
use support::{FixtureCommand, World};

const SHELLS: &[&str] = &["bash", "zsh", "fish"];

struct CompletionWorld {
    world: World,
    scratch: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    state: PathBuf,
    path: OsString,
}

impl CompletionWorld {
    fn new() -> Self {
        let world = World::new();
        let scratch = tempfile::tempdir_in(world.base()).unwrap();
        let (path, state) = {
            let dots = world.dots();
            let env = |key: &str| {
                dots.get_envs()
                    .find_map(|(name, value)| (name == key).then_some(value).flatten())
                    .unwrap_or_else(|| panic!("fixture does not set {key}"))
            };
            symlink(dots.get_program(), scratch.path().join("dots")).unwrap();
            let path = std::env::join_paths(
                std::iter::once(scratch.path().to_owned())
                    .chain(std::env::split_paths(env("PATH"))),
            )
            .unwrap();
            (path, PathBuf::from(env("XDG_STATE_HOME")))
        };
        let home = world.home().to_owned();
        let root = world.root().to_owned();
        let exceptions = root.join("dots/exceptions.tsv");
        if exceptions.exists() {
            fs::remove_file(exceptions).unwrap();
        }
        Self {
            world,
            scratch,
            home,
            root,
            state,
            path,
        }
    }

    fn cwd(&self) -> &Path {
        self.scratch.path()
    }

    fn shell(&self, shell: impl AsRef<OsStr>) -> FixtureCommand<'_> {
        let mut command = self.world.command(shell);
        command
            .env("PATH", &self.path)
            .env("ZDOTDIR", &self.home)
            .env("TERM", "dumb")
            .env_remove("DOTS_SKILL_LINKS")
            .current_dir(self.cwd());
        command
    }

    fn static_mise_trap(&self, command: &mut FixtureCommand<'_>) {
        let probe = self.cwd().join("unexpected-mise");
        fixture(&probe, "#!/bin/sh\ntouch \"$PROBE_LOG\"\nexit 1\n", 0o700);
        command
            .env("DOTS_MISE_BIN", &probe)
            .env("PROBE_LOG", self.cwd().join("mise-called"));
    }

    fn candidates(&self, line: &str) -> Vec<String> {
        self.candidates_with(line, |_| {})
    }

    fn candidates_with(
        &self,
        line: &str,
        setup: impl FnOnce(&mut FixtureCommand<'_>),
    ) -> Vec<String> {
        let mut command = self.shell("bash");
        self.static_mise_trap(&mut command);
        setup(&mut command);
        let output = command
            .args([
                "--noprofile",
                "--norc",
                "-c",
                r#"eval "$(dots completions bash)" || exit
               COMP_WORDBREAKS=
               COMP_LINE=$1
               COMP_POINT=${#COMP_LINE}
               _dots_complete 2>/dev/null
               if (( ${#COMPREPLY[@]} )); then printf '%s\0' "${COMPREPLY[@]}"; fi"#,
                "completion-test",
                line,
            ])
            .output()
            .expect("Bash must be installed");
        assert_success(&output);
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| String::from_utf8(field.to_vec()).unwrap())
            .collect()
    }

    fn expect(&self, line: &str, values: &[&str]) {
        assert_eq!(self.candidates(line), values, "{line:?}");
    }

    fn session(&self, shell: &str, autoload: bool) -> Session {
        let mut command = self.shell(shell);
        command
            .env("DOTS_TEST_BINARY", self.world.dots().get_program())
            .env("DOTS_TEST_PATH_LOG", self.cwd().join("new-path-calls"));
        self.static_mise_trap(&mut command);
        let (flags, setup) = match shell {
            "bash" => (
                &["--noprofile", "--norc", "-i"][..],
                r#"eval "$(dots completions bash)""#,
            ),
            "zsh" if autoload => {
                let emitted = self
                    .world
                    .dots()
                    .args(["completions", "zsh"])
                    .output()
                    .unwrap();
                assert_success(&emitted);
                fs::write(self.cwd().join("_dots"), emitted.stdout).unwrap();
                (
                    &["-f", "-i"][..],
                    "fpath=($PWD $fpath); autoload -Uz compinit; compinit -D -i",
                )
            }
            "zsh" => (
                &["-f", "-i"][..],
                "autoload -Uz compinit; compinit -D -i && source <(dots completions zsh)",
            ),
            "fish" => (&["--no-config", "-i"][..], "dots completions fish | source"),
            _ => unreachable!(),
        };
        command.args(flags);
        let mut session = Session::start(command, shell);
        let record = if shell == "fish" {
            r#"function dots; printf '\n%s%s' TEST_ ARGS; for arg in $argv; printf '\0%s' "$arg"; end; printf '\0%s%s\n' END_ ARGS; end"#
        } else {
            r#"dots() { printf '\n%s%s' TEST_ ARGS; for arg; do printf '\0%s' "$arg"; done; printf '\0%s%s\n' END_ ARGS; }"#
        };
        session.send(format!("{setup} && {record}; printf '%s%s\\n' READY_ DOTS\n").as_bytes());
        session.wait_for(b"READY_DOTS");
        session
    }
}

fn fixture(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn unchanged_completion_emission_without_setup() {
    let world = CompletionWorld::new();
    fs::create_dir_all(world.home.join(".agents")).unwrap();
    for shell in SHELLS {
        let mut command = world.world.dots();
        world.static_mise_trap(&mut command);
        let output = command
            .env("DOTS_ROOT", world.cwd().join("no-config"))
            .env_remove("DOTS_SKILL_LINKS")
            .args(["completions", shell])
            .output()
            .unwrap();
        assert_success(&output);
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.is_empty());
    }
    for args in [
        &["completions"][..],
        &["completions", "ksh"],
        &["completions", "bash", "extra"],
    ] {
        let output = world.world.dots().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
    assert!(!world.home.join(".agents/skills/dots").exists());
    assert!(!world.cwd().join("mise-called").exists());
}

#[test]
fn unchanged_completion_grammar() {
    let world = CompletionWorld::new();
    fixture(
        &world.root.join("config.toml"),
        "[vars]\ndots_profiles = '''server laptop'''\n",
        0o600,
    );
    for (name, mode) in [
        ("hello", 0o700),
        ("hidden", 0o600),
        ("bad name", 0o700),
        ("add", 0o700),
    ] {
        fixture(
            &world.root.join("dots/commands").join(name),
            "#!/bin/sh\ntouch \"$HOME/config-command-ran\"\n",
            mode,
        );
    }
    for (line, values) in [
        ("dots he", &["hello", "help"][..]),
        ("dots --plain he", &["help"]),
        ("dots hidden", &[]),
        ("dots hello --", &[]),
        ("dots up", &["up", "update"]),
        ("dots --plain update --ver", &[]),
        ("dots up --last --m", &[]),
        ("dots sync --l", &["--last"]),
        ("dots completions ", &["bash", "fish", "zsh"]),
        ("dots reports u", &["uninstall", "up", "update"]),
        ("dots test shell ", &["node", "python"]),
        ("dots add --profile ", &["laptop", "server"]),
        ("dots add --profile server --p", &[]),
        ("dots add npm:prettier ca", &["cargo:"]),
        ("dots add --asset ", &[]),
        ("dots uninstall --profile=la", &["--profile=laptop"]),
        ("dots bootstrap --config-dir ", &[]),
        ("dots bootstrap --restore laptop --on", &[]),
        ("dots bootstrap --only=tools --rest", &[]),
        (
            "dots bootstrap --only=tools,dot",
            &["--only=tools,dotfiles"],
        ),
        ("dots bundle ch", &["check"]),
        (
            "dots bundle create --recipient age1 --rec",
            &["--recipient"],
        ),
        ("dots --verbose --plain ", &[]),
    ] {
        world.expect(line, values);
    }
    assert!(!world.home.join("config-command-ran").exists());
    assert!(!world.cwd().join("mise-called").exists());
}

#[test]
fn new_completion_contract_grammar() {
    let world = CompletionWorld::new();
    fixture(
        &world.root.join("config.toml"),
        "[vars]\ndots_profiles = 'server laptop'\n",
        0o600,
    );
    fixture(
        &world.root.join("dots/commands/root"),
        "#!/bin/sh\nexit 0\n",
        0o700,
    );
    fs::create_dir(world.cwd().join("--bogus")).unwrap();
    for (line, values) in [
        ("dots ro", &["root"][..]),
        ("dots atuin", &[]),
        ("dots publi", &[]),
        ("dots profile --profile se", &["server"]),
        ("dots reports pl", &["plan"]),
        ("dots reports rep", &[]),
        ("dots brew admin", &["admin-available"]),
        ("dots --plain --ver", &[]),
        ("dots bootstrap -Elap", &["-Elaptop"]),
        ("dots bootstrap -E=server,lap", &["-E=server,laptop"]),
        (
            "dots bootstrap --skip=tools --sk",
            &["--skip", "--skip-dirty"],
        ),
        ("dots bootstrap --only=tools comp", &[]),
        ("dots apply -n --dry", &[]),
        ("dots apply -ny --yes", &[]),
        ("dots apply -- --", &[]),
        ("dots bundle create --from-live", &[]),
        ("dots mise --h", &[]),
        ("dots add --asset glob ", &["https://github.com/"]),
        ("dots add --asset=glob npm", &[]),
        ("dots add --asset glob https://github.com/owner/tool ", &[]),
        ("dots add --as", &["--asset"]),
        ("dots add https://github.com/owner/tool --as", &["--asset"]),
        ("dots add apt:curl --as", &[]),
        (
            "dots add https://github.com/owner/tool https://github.com/owner/other --as",
            &[],
        ),
        ("dots init --bo", &[]),
        ("dots init -- --bo", &["--bogus"]),
        ("dots init ./--bo", &["./--bogus"]),
    ] {
        world.expect(line, values);
    }
    for flag in ["-h", "--help"] {
        let output = world
            .world
            .dots()
            .args(["completions", flag])
            .output()
            .unwrap();
        assert_success(&output);
        assert!(String::from_utf8_lossy(&output.stdout).contains("completions"));
    }
    assert!(!world.cwd().join("mise-called").exists());
}

#[test]
fn new_completion_uses_the_released_menu_name() {
    let world = CompletionWorld::new();
    world.expect("dots men", &[]);
    world.expect("dots --no-w", &[]);
    world.expect("dots up --no-w", &[]);
    world.expect("dots up --l", &["--last"]);
    fixture(
        &world.root.join("dots/commands/menu"),
        "#!/bin/sh\ntouch \"$HOME/config-command-ran\"\n",
        0o700,
    );
    world.expect("dots men", &["menu"]);
    world.expect("dots menu --", &[]);
    world.expect("dots --plain men", &[]);
    world.expect("dots reports pl", &["plan"]);
    assert!(!world.home.join("config-command-ran").exists());
    assert!(!world.cwd().join("mise-called").exists());
}

#[test]
fn new_completion_skips_unavailable_inputs() {
    let world = CompletionWorld::new();
    fixture(&world.root.join("config.toml"), "[vars\n", 0o600);
    world.expect("dots add --profile ", &[]);
    world.expect("dots --pl", &["--plain"]);
    assert_eq!(
        world.candidates_with("dots compl", |command| {
            let absent = world.cwd().join("no-config");
            command
                .env("MISE_CONFIG_DIR", &absent)
                .env("DOTS_ROOT", absent);
        }),
        ["completions"]
    );
    assert!(!world.cwd().join("mise-called").exists());
}

#[test]
fn unchanged_completion_scoped_removal() {
    let world = CompletionWorld::new();
    fixture(
        &world.root.join("config.toml"),
        r#"
[vars]
dots_profiles = "server laptop"
[tools]
fd = "latest"
"npm:@scope/foo" = [{version = "1"}, {version = "2"}]
"bad\nname" = "latest"
[bootstrap.packages]
"brew:homebrew/core/fd" = "latest"
"unsupported:unknown" = "latest"
"#,
        0o600,
    );
    fixture(
        &world.root.join("config.server.toml"),
        "[tools]\nserver = 'latest'\n",
        0o600,
    );
    fixture(
        &world.cwd().join("-draft=prod/config.toml"),
        "[tools]\nchosen = 'latest'\n",
        0o600,
    );
    fixture(
        &world.cwd().join("--path=chosen.toml"),
        "[tools]\nchosen = 'latest'\n",
        0o600,
    );
    fixture(
        &world.cwd().join("broken.toml"),
        "[tools]\nbroken = {\n",
        0o600,
    );
    for (line, values) in [
        ("dots remove --base fd", &["brew:fd", "mise:fd"][..]),
        ("dots remove --base npm:", &["npm:@scope/foo"]),
        ("dots remove --base mise:fd fd", &["brew:fd"]),
        ("dots remove --base -- bad", &[]),
        ("dots uninstall --profile=server ", &["server"]),
        ("dots remove --profile ../server ", &[]),
        ("dots remove --base --profile server ", &[]),
        ("dots remove --base unsupported", &[]),
        ("dots remove --path -draft=prod/config.toml ", &["chosen"]),
        ("dots remove --path --path=chosen.toml ", &["chosen"]),
        ("dots remove --path=broken.toml ", &[]),
    ] {
        world.expect(line, values);
    }
    fixture(
        &world.root.join("dots/exceptions.tsv"),
        "server\tmanual\thold=brew:fd\n",
        0o600,
    );
    world.expect("dots remove --base fd", &["mise:fd"]);
    assert!(!world.cwd().join("mise-called").exists());
}

#[test]
fn unchanged_completion_active_removal_uses_a_safe_probe() {
    let world = CompletionWorld::new();
    let root = &world.root;
    fixture(
        &root.join("config.toml"),
        "[vars]\ndots_profiles = 'server'\n[tools]\nnode = '22'\n",
        0o600,
    );
    fixture(
        &root.join("config.local.toml"),
        "[tools]\nlocal-tool = 'latest'\n",
        0o600,
    );
    fixture(
        &root.join("config.inactive.toml"),
        "[tools]\ninactive = 'latest'\n",
        0o600,
    );
    fixture(
        &world.cwd().join("external.toml"),
        "[tools]\nexternal = 'latest'\n",
        0o600,
    );
    symlink(
        world.cwd().join("external.toml"),
        root.join("external.toml"),
    )
    .unwrap();
    let probe = world.cwd().join("mise-probe");
    let probe_log = world.cwd().join("probe-called");
    fixture(
        &probe,
        r#"#!/bin/sh
test "${1-}" != --fixture-ready || exit 0
case " $* " in *" config ls --json "*) ;; *) exit 10 ;; esac
test -z "${MISE_ENV-}" && test "${MISE_LOG_FILE+set}" = "" &&
test "$MISE_CONFIG_DIR" = "$PROBE_ROOT" &&
test "$MISE_OFFLINE:$MISE_SAFE:$MISE_NO_ENV:$MISE_NO_HOOKS:$MISE_AUTO_INSTALL:$MISE_EXEC_AUTO_INSTALL:$MISE_AUTO_UPDATE" = "1:1:1:1:0:0:0" &&
test "$MISE_STATE_DIR" != "$PROBE_ROOT" && test "$MISE_CACHE_DIR" != "$PROBE_ROOT" || exit 11
printf 'called' > "$PROBE_LOG"
printf '%s\n' "$PROBE_OUTPUT"
"#,
        0o700,
    );
    // A freshly created executable can take longer than the probe's deadline
    // to launch on macOS. Warm the fixture before measuring the actual query.
    let ready = world.shell(&probe).arg("--fixture-ready").output().unwrap();
    assert_success(&ready);
    let listed: Vec<_> = ["config.toml", "config.local.toml", "external.toml"]
        .into_iter()
        .map(|name| serde_json::json!({"path": root.join(name)}))
        .collect();
    let setup = |command: &mut FixtureCommand<'_>| {
        command
            .env("DOTS_MISE_BIN", &probe)
            .env("MISE_ENV", "inactive")
            .env("MISE_LOG_FILE", root.join("unexpected-mise.log"))
            .env("MISE_STATE_DIR", root)
            .env("MISE_CACHE_DIR", root)
            .env("PROBE_ROOT", root)
            .env("PROBE_LOG", &probe_log)
            .env("PROBE_OUTPUT", serde_json::to_string(&listed).unwrap());
    };
    assert_eq!(
        world.candidates_with("dots remove --d", setup),
        ["--dry-run"]
    );
    assert!(!probe_log.exists());
    let active = world.candidates_with("dots remove ", setup);
    assert_eq!(fs::read_to_string(&probe_log).unwrap(), "called");
    assert_eq!(active, ["local-tool", "node"]);
    world.expect("dots remove no", &[]);
    fixture(
        &world.state.join("dots/github-apps/48666178824fdf0b3270751a27679be440bf0aa41e9779424e6de7ed8083cccf.json"),
        r#"{"repo":"owner/standalone","path":"/Applications/Dots Completion Fixture Standalone.app","bundle_id":"test.example"}"#,
        0o600,
    );
    assert_eq!(
        world.candidates_with("dots remove stand", setup),
        ["standalone"]
    );
    world.expect("dots remove --base stand", &[]);
}

#[test]
fn unchanged_completion_native_shells() {
    let world = CompletionWorld::new();
    fs::create_dir_all(world.home.join(".agents")).unwrap();
    fixture(
        &world.root.join("config.toml"),
        "[vars]\ndots_profiles = 'server'\n[tools]\nripgrep = 'latest'\n'npm:@scope/foo' = 'latest'\n",
        0o600,
    );
    for name in [
        "path file",
        "env=prod/config.toml",
        "-draft=prod/config.toml",
        "config\tlocal.toml",
    ] {
        fixture(&world.cwd().join(name), "", 0o600);
    }
    fs::create_dir(world.cwd().join("path folder")).unwrap();
    fixture(
        &world.cwd().join("later-bin/dots"),
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$DOTS_TEST_PATH_LOG\"\nexec \"$DOTS_TEST_BINARY\" \"$@\"\n",
        0o700,
    );
    let calls = || {
        fs::read_to_string(world.cwd().join("new-path-calls")).map_or(0, |log| log.lines().count())
    };
    for shell in SHELLS {
        let mut session = world.session(shell, false);
        session.expect(b"dots ad\t", &["add"]);
        session.expect(b"dots add brew-c\tcode", &["add", "brew-cask:code"]);
        session.expect(
            b"dots remove --base npm:\t",
            &["remove", "--base", "npm:@scope/foo"],
        );
        session.expect(
            b"dots remove --base \"\" rip\t",
            &["remove", "--base", "", "ripgrep"],
        );
        session.expect(
            b"dots remove --path path\\ fi\t",
            &["remove", "--path", "path file"],
        );
        session.expect(b"dots init pa\tchild", &["init", "path folder/child"]);
        session.expect(
            b"dots remove --path=env=prod/con\t",
            &["remove", "--path=env=prod/config.toml"],
        );
        session.expect(
            b"dots remove --path -draft=prod/con\t",
            &["remove", "--path", "-draft=prod/config.toml"],
        );
        let mut keys = b"dots remove --base rip --dry-run".to_vec();
        keys.extend(std::iter::repeat_n(2, " --dry-run".len()));
        keys.push(b'\t');
        session.expect(&keys, &["remove", "--base", "ripgrep", "--dry-run"]);
        if *shell != "fish" {
            session.expect(
                b"dots remove --path config\t",
                &["remove", "--path", "config\tlocal.toml"],
            );
        }
        let previous_calls = calls();
        session.send(if *shell == "fish" {
            b"set -gx PATH \"$PWD/later-bin\" $PATH\n"
        } else {
            b"export PATH=\"$PWD/later-bin:$PATH\"\n"
        });
        session.expect(b"dots unins\t", &["uninstall"]);
        assert!(
            calls() > previous_calls,
            "{shell} kept using its startup dots"
        );
    }
    world.session("zsh", true).expect(b"dots ad\t", &["add"]);
    assert!(!world.cwd().join("mise-called").exists());
    assert!(!world.home.join(".agents/skills/dots").exists());
    assert!(!world.state.join("dots/skills/dots").exists());
}

fn home_spelling(shells: &[&str]) {
    let world = CompletionWorld::new();
    fixture(
        &world.home.join("chosen.toml"),
        "[tools]\nripgrep = 'latest'\n",
        0o600,
    );
    fixture(
        &world.cwd().join("~/chosen.toml"),
        "[tools]\nripple = 'latest'\n",
        0o600,
    );
    let home_path = world.home.join("chosen.toml");
    for shell in shells {
        let mut session = world.session(shell, false);
        session.expect(
            b"dots remove --path ~/chosen.toml rip\t",
            &["remove", "--path", home_path.to_str().unwrap(), "ripgrep"],
        );
        session.expect(
            b"dots remove --path '~/chosen.toml' rip\t",
            &["remove", "--path", "~/chosen.toml", "ripple"],
        );
        session.expect(
            b"dots remove --path \\~/chosen.toml rip\t",
            &["remove", "--path", "~/chosen.toml", "ripple"],
        );
    }
    assert!(!world.cwd().join("mise-called").exists());
}

#[test]
fn unchanged_completion_bash_zsh_prior_home_spelling() {
    home_spelling(&["bash", "zsh"]);
}

#[test]
fn new_completion_fish_prior_home_spelling() {
    home_spelling(&["fish"]);
}

#[test]
fn new_completion_fish_continuation_preserves_prior_home_spelling() {
    let world = CompletionWorld::new();
    fixture(
        &world.home.join("chosen.toml"),
        "[tools]\nripgrep = 'latest'\n",
        0o600,
    );
    fixture(
        &world.cwd().join("~/chosen.toml"),
        "[tools]\nripple = 'latest'\n",
        0o600,
    );
    let output = world
        .shell("fish")
        .args([
            "--no-config",
            "-c",
            r#"dots completions fish | source
               complete -C "$argv[1]""#,
            "dots remove --path \\\n~/chosen.toml rip",
        ])
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(output.stdout, b"ripgrep\tmise tool\n");
}

#[test]
fn new_completion_zsh_matches_the_suffix_inside_a_word() {
    let world = CompletionWorld::new();
    let mut zsh = world.session("zsh", false);
    zsh.send(b"setopt COMPLETE_IN_WORD\n");
    let mut keys = b"dots bootstrap --prune-dotfiles".to_vec();
    keys.extend(std::iter::repeat_n(2, "une-dotfiles".len()));
    keys.extend_from_slice(b"\tmore");
    zsh.expect(&keys, &["bootstrap", "--prune-dotfiles", "more"]);
}

#[test]
fn new_completion_bash_keeps_npm_scope_after_a_word_break() {
    let world = CompletionWorld::new();
    fixture(
        &world.root.join("config.toml"),
        "[tools]\n'npm:@scope/foo' = 'latest'\n",
        0o600,
    );
    world.session("bash", false).expect(
        b"dots remove --base npm:@scope/f\t",
        &["remove", "--base", "npm:@scope/foo"],
    );
}

#[test]
fn new_completion_bash_decodes_ansi_c_paths() {
    let world = CompletionWorld::new();
    fixture(
        &world.cwd().join("config\tlocal.toml"),
        "[tools]\nripgrep = 'latest'\n",
        0o600,
    );
    let control_path = world
        .shell("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            r"printf '%s' $'config\c?local.toml'",
        ])
        .output()
        .unwrap();
    assert_success(&control_path);
    let control_path = std::str::from_utf8(&control_path.stdout).unwrap();
    assert!(matches!(
        control_path,
        "config\x1flocal.toml" | "config\x7flocal.toml"
    ));
    fixture(
        &world.cwd().join(control_path),
        "[tools]\nripgrep = 'latest'\n",
        0o600,
    );
    let mut bash = world.session("bash", false);
    bash.expect(
        b"dots remove --path $'config\\tlocal.toml' rip\t",
        &["remove", "--path", "config\tlocal.toml", "ripgrep"],
    );
    bash.expect(
        b"dots remove --path=$'config\\011local.toml' rip\t",
        &["remove", "--path=config\tlocal.toml", "ripgrep"],
    );
    bash.expect(
        b"dots remove --path $'config\\cIlocal.toml' rip\t",
        &["remove", "--path", "config\tlocal.toml", "ripgrep"],
    );
    bash.expect(
        b"dots remove --path $'config\\c?local.toml' rip\t",
        &["remove", "--path", control_path, "ripgrep"],
    );
}

#[test]
fn new_completion_preserves_non_utf8_native_path_slots() {
    let world = CompletionWorld::new();
    let query = |words: &[&OsStr]| {
        let output = world
            .world
            .dots()
            .current_dir(world.cwd())
            .args(["__complete", "bash"])
            .args(words)
            .output()
            .unwrap();
        assert_success(&output);
        assert!(output.stderr.is_empty());
        output.stdout
    };
    let remove = OsStr::new("remove");
    let separate = OsStr::new("--path");
    let name = OsStr::from_bytes(b"config\xff.toml");
    let mut inline = OsString::from("--path=");
    inline.push(name);
    assert_eq!(query(&[remove, separate, name]), b":files\n");
    assert_eq!(query(&[remove, &inline]), b":files=\n");
    assert_eq!(query(&[OsStr::new("init"), name]), b":dirs\n");
    assert_eq!(query(&[remove, &inline, OsStr::new("--b")]), b"");
    // Linux accepts non-UTF-8 filenames. On macOS, the protocol still keeps
    // the slot native and recognizes the earlier inline selector as supplied.
    #[cfg(target_os = "linux")]
    {
        fixture(
            &world.cwd().join(name),
            "[tools]\nripgrep = 'latest'\n",
            0o600,
        );
        assert_eq!(
            query(&[remove, &inline, OsStr::new("rip")]),
            b"ripgrep\tmise tool\n"
        );
    }
}

#[test]
fn new_completion_fish_preserves_native_and_quoted_paths() {
    let world = CompletionWorld::new();
    fixture(&world.cwd().join("config\tlocal.toml"), "", 0o600);
    fixture(&world.cwd().join("env=prod/path file"), "", 0o600);
    let output = world
        .shell("fish")
        .args([
            "--no-config",
            "-c",
            r#"dots completions fish | source
               complete -C 'dots remove --path config'
               complete -C 'dots remove "--path=env=prod/path fi"'"#,
        ])
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(
        output.stdout,
        b"config\tlocal.toml\n--path=env=prod/path file\n"
    );
}

struct Session {
    shell: String,
    child: Child,
    tty: Option<File>,
    buffered: Vec<u8>,
}

impl Session {
    fn start(mut command: FixtureCommand<'_>, shell: &str) -> Self {
        let (master, tty) = terminal().expect("open completion test terminal");
        command
            .stdin(tty.try_clone().unwrap())
            .stdout(tty.try_clone().unwrap())
            .stderr(tty);
        // The child needs a controlling terminal for its real completion widget.
        unsafe {
            command.pre_exec(|| {
                #[cfg(target_os = "macos")]
                let request = libc::c_ulong::from(libc::TIOCSCTTY);
                #[cfg(not(target_os = "macos"))]
                let request = libc::TIOCSCTTY;
                if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, request, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .unwrap_or_else(|error| panic!("{shell} is required: {error}"));
        Self {
            shell: shell.into(),
            child,
            tty: Some(master),
            buffered: Vec::new(),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.tty.as_mut().unwrap().write_all(bytes).unwrap();
    }

    fn wait_for(&mut self, marker: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(start) = self
                .buffered
                .windows(marker.len())
                .position(|bytes| bytes == marker)
            {
                return self.buffered.drain(..start + marker.len()).collect();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let tty = self.tty.as_mut().unwrap();
            let mut ready = libc::pollfd {
                fd: tty.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // poll only observes readiness. The descriptor stays owned by this session.
            let status = unsafe { libc::poll(&mut ready, 1, remaining.as_millis() as libc::c_int) };
            if status < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let mut bytes = [0; 4096];
            if status > 0
                && let Ok(n @ 1..) = tty.read(&mut bytes)
            {
                self.buffered.extend_from_slice(&bytes[..n]);
                continue;
            }
            panic!(
                "{} did not print {:?}\n{}",
                self.shell,
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(&self.buffered),
            );
        }
    }

    fn expect(&mut self, keys: &[u8], expected: &[&str]) {
        self.send(keys);
        self.send(b"\n");
        let output = self.wait_for(b"\0END_ARGS");
        let start = output
            .windows(9)
            .position(|bytes| bytes == b"TEST_ARGS")
            .expect("argument recorder");
        let args: Vec<_> = output[start + 9..output.len() - 9]
            .split(|byte| *byte == 0)
            .skip(1)
            .map(|arg| std::str::from_utf8(arg).unwrap())
            .collect();
        assert_eq!(
            args,
            expected,
            "{} {:?}",
            self.shell,
            String::from_utf8_lossy(keys)
        );
    }
}

fn terminal() -> io::Result<(File, File)> {
    let (mut master, mut slave) = (-1, -1);
    // openpty initializes both owned descriptors without optional termios data.
    unsafe {
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        let pair = (File::from_raw_fd(master), File::from_raw_fd(slave));
        for file in [&pair.0, &pair.1] {
            if libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(pair)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        drop(self.tty.take());
        let _ = self.child.wait();
    }
}
