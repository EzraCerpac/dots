mod support;

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Output;
use std::{env, fs};

use dots_cli::config::Root;
use dots_cli::history::{self, Checkpoint};
use dots_cli::run::{Run, Sink};
use dots_cli::sync::{self, Loaded};
use dots_cli::{Ctx, Env, Flags};
use support::{FixtureCommand, World};

const CONFIG: &str = "[vars]\ndots_profiles = \"laptop\"\n";
const EMPTY: &str = r#"{"entries":[],"invalid":[]}"#;

struct Fixture {
    world: World,
}

impl Fixture {
    fn new() -> Self {
        let world = World::new();
        fs::remove_dir_all(world.root().join("dots")).unwrap();
        fs::remove_file(world.root().join("config.laptop.toml")).unwrap();
        fs::write(world.root().join("config.toml"), CONFIG).unwrap();
        let bin = world.home().join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("mise"), MISE);
        executable(
            &bin.join("maintenance-mise"),
            include_str!("contract_update_fixtures/mise"),
        );
        // Give the oracle its local runtime without exposing user shims or
        // additional package managers to update.
        let path = env::var_os("PATH").unwrap();
        for tool in ["git", "node", "jq"] {
            if let Some(source) = env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(tool))
                .find(|path| path.is_file())
            {
                symlink(source, bin.join(tool)).unwrap();
            }
        }
        let fixture = Self { world };
        fixture.write("shared", EMPTY);
        fixture.write("local", EMPTY);
        fixture
    }

    fn seed(&self) {
        self.git(&["init", "--initial-branch=dev"]);
        self.git(&["add", "config.toml"]);
        self.git(&["commit", "-m", "Seed fixture"]);
    }

    fn git(&self, args: &[&str]) -> Vec<u8> {
        success(self.world.git().args(args).output().unwrap()).stdout
    }

    fn write(&self, name: &str, contents: &str) {
        fs::write(self.world.home().join(name), contents).unwrap();
    }

    fn isolated(&self, command: &mut FixtureCommand<'_>) {
        let bin = self.world.home().join(".local/bin");
        command
            .env(
                "PATH",
                env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap(),
            )
            .env("DOTS_MISE_BIN", bin.join("mise"))
            .env("SETUP_PROFILE", "laptop")
            .env("SETUP_MACHINE_ID", "fixture")
            .env("MISE_CONFIG_DIR", self.world.root())
            .env("MISE_TRUSTED_CONFIG_PATHS", self.world.root())
            .env("MISE_OFFLINE", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1");
    }

    fn seam(&self, name: &str, expected: &str) {
        let test = std::env::current_exe().unwrap();
        let mut command = self.world.command(test);
        self.isolated(&mut command);
        success(
            command
                .env("DOTS_TEST_SEAM", name)
                .env("DOTS_TEST_EXPECTED", expected)
                .args(["--ignored", "--exact", "decision_child"])
                .output()
                .unwrap(),
        );
    }
}

fn executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn success(output: Output) -> Output {
    assert!(output.status.success(), "{output:?}");
    output
}

#[test]
#[ignore = "guarded subprocess for the public Rust seams"]
fn decision_child() {
    let Ok(seam) = std::env::var("DOTS_TEST_SEAM") else {
        return;
    };
    let env = Env::current().unwrap();
    let ctx = Ctx {
        root: Root::resolve(&env).unwrap(),
        env,
        flags: Flags::default(),
    };
    let actual = match seam.as_str() {
        "checkpoint" => {
            let run = Run::begin(&ctx, Sink::Unreported);
            let stage = run.start("Test checkpoint");
            let result = match history::checkpoint(&run, stage, &ctx) {
                Ok(Checkpoint::NothingTracked) => "empty",
                Err(issue) => {
                    assert!(issue.diagnostic.contains("P-06"), "{issue:?}");
                    "blocked"
                }
                Ok(Checkpoint::Saved) => panic!("checkpoint saved history"),
            };
            let _ = run.end(0);
            result
        }
        "source" => match sync::load(&ctx) {
            Loaded::LocalOnly => "local",
            Loaded::Problem(issue) => {
                assert!(issue.diagnostic.contains("P-08"), "{issue:?}");
                "blocked"
            }
            Loaded::Ready(_) => panic!("shared sync was loaded"),
        },
        _ => panic!("unexpected test seam {seam}"),
    };
    assert_eq!(actual, std::env::var("DOTS_TEST_EXPECTED").unwrap());
}

#[test]
fn new_checkpoint_requires_proven_empty_native_inventory() {
    let f = Fixture::new();
    f.seam("checkpoint", "empty");
    let active = r#"{"entries":[{"mode":"track","path":"~/setting"}],"invalid":[]}"#;
    let invalid = r#"{"entries":[],"invalid":[{"path":"~/setting"}]}"#;
    for (scope, output) in [
        ("shared", active),
        ("shared", r#"{"entries":[{"mode":"unknown"}],"invalid":[]}"#),
        ("shared", r#"{"entries":[]}"#),
        ("shared", "not JSON"),
        (
            "local",
            r#"{"entries":[{"mode":"track-local","path":"~/setting"}],"invalid":[]}"#,
        ),
        ("local", invalid),
        ("local", r#"{"entries":null,"invalid":[]}"#),
    ] {
        f.write(scope, output);
        f.seam("checkpoint", "blocked");
        f.write(scope, EMPTY);
    }
    f.write("local-failure", "");
    f.seam("checkpoint", "blocked");
    fs::remove_file(f.world.home().join("local-failure")).unwrap();
    f.write("shared", invalid);
    let inactive = r#"{"target":"~/setting","mode":"track","reason":"no variant matches this machine: host-other"}"#;
    let active = r#"{"target":"~/setting","mode":"track"}"#;
    for (rows, expected) in [
        (inactive.to_owned(), "empty"),
        (active.to_owned(), "blocked"),
        (format!("{inactive},{active}"), "blocked"),
        (String::new(), "blocked"),
    ] {
        f.write("status", &format!(r#"{{"files":[{rows}]}}"#));
        f.seam("checkpoint", expected);
    }
    f.write("status", &format!(r#"{{"files":[{inactive}]}}"#));
    f.write("status-failure", "");
    f.seam("checkpoint", "blocked");
    let calls = fs::read_to_string(f.world.home().join("history-calls")).unwrap();
    assert!(
        calls
            .lines()
            .all(|line| matches!(line, "shared" | "local" | "status")),
        "{calls}"
    );
    assert!(!f.world.base().join("mise/state/history").exists());
}

#[test]
fn new_sync_load_checks_source_and_checkout_before_skipping() {
    let f = Fixture::new();
    f.seam("source", "blocked");
    f.seed();
    let root = f.world.root();
    let conf = root.join("config.toml");
    let git_conf = root.join(".git/config");
    f.seam("source", "local");
    let revision = f.git(&["show-ref"]);

    for marker in [
        "MERGE_HEAD",
        "dots-dev.json",
        "dots-sync-pending",
        "dots-sync-write.json",
    ] {
        let path = root.join(".git").join(marker);
        fs::write(&path, "incomplete").unwrap();
        f.seam("source", "blocked");
        fs::remove_file(path).unwrap();
    }
    f.git(&["switch", "--detach"]);
    f.seam("source", "blocked");
    f.git(&["switch", "dev"]);
    fs::write(
        &conf,
        format!("{CONFIG}dots_source_repo = 'example/source'\n"),
    )
    .unwrap();
    f.seam("source", "blocked");
    f.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/example/different",
    ]);
    f.seam("source", "blocked");
    f.git(&[
        "remote",
        "set-url",
        "origin",
        "https://github.com/example/source",
    ]);
    f.seam("source", "blocked");
    fs::write(&conf, CONFIG).unwrap();
    f.seam("source", "blocked");
    f.git(&["remote", "remove", "origin"]);
    f.git(&["config", "remote.origin.pushurl", "file:///absent"]);
    f.seam("source", "blocked");
    f.git(&["config", "--remove-section", "remote.origin"]);

    let original = fs::read(&git_conf).unwrap();
    fs::write(&git_conf, "[broken").unwrap();
    f.seam("source", "blocked");
    fs::write(&git_conf, original).unwrap();
    fs::write(&conf, "[broken").unwrap();
    f.seam("source", "blocked");
    fs::write(&conf, CONFIG).unwrap();
    f.seam("source", "local");
    assert_eq!(f.git(&["show-ref"]), revision);
    assert!(!f.world.home().join("history-calls").exists());
}

#[test]
#[ignore = "P-04"]
fn unchanged_up_finishes_maintenance_with_empty_history_and_no_origin() {
    let f = Fixture::new();
    f.seed();
    let hooks = f.world.root().join("dots/hooks/update.d");
    fs::create_dir_all(&hooks).unwrap();
    executable(
        &hooks.join("10-maintenance"),
        "#!/bin/sh\ntouch \"$HOME/maintenance-ran\"\n",
    );
    f.write("untracked-setting", "do not capture or modify\n");
    let refs = f.git(&["show-ref"]);
    let status = f.git(&["status", "--porcelain"]);

    let mut command = f.world.dots();
    f.isolated(&mut command);
    if let Some(mise) = std::env::var_os("DOTS_TEST_NATIVE_MISE") {
        command.env("DOTS_TEST_NATIVE_MISE", mise);
    }
    success(command.args(["--plain", "up"]).output().unwrap());
    assert!(f.world.home().join("maintenance-ran").is_file());
    assert_eq!(f.git(&["show-ref"]), refs);
    assert_eq!(f.git(&["status", "--porcelain"]), status);
    assert!(f.git(&["remote"]).is_empty());
    assert_eq!(
        fs::read_to_string(f.world.home().join("untracked-setting")).unwrap(),
        "do not capture or modify\n"
    );
    let history = f.world.base().join("mise/state/history");
    if history.is_dir() {
        for entry in fs::read_dir(&history).unwrap() {
            let path = entry.unwrap().path();
            if path.join("HEAD").is_file() {
                let output = success(
                    f.world
                        .git()
                        .arg("--git-dir")
                        .arg(path)
                        .args(["count-objects", "-v"])
                        .output()
                        .unwrap(),
                );
                let counts = String::from_utf8(output.stdout).unwrap();
                assert!(counts.lines().any(|line| line == "count: 0"), "{counts}");
                assert!(counts.lines().any(|line| line == "in-pack: 0"), "{counts}");
            }
        }
    }
}

const MISE: &str = r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        -C|--cd) cd -- "$2"; shift 2 ;;
        -E|--env) export MISE_ENV="$2"; shift 2 ;;
        -q|--quiet|--no-config) shift ;;
        *) break ;;
    esac
done
case "$*" in
    'bootstrap dotfiles paths --json') scope=shared ;;
    'bootstrap dotfiles --local paths --json') scope=local ;;
    'bootstrap dotfiles status --json -- '*) scope=status ;;
    'config ls --json')
        if [ -n "${DOTS_TEST_NATIVE_MISE:-}" ]; then
            exec "$DOTS_TEST_NATIVE_MISE" "$@"
        fi
        exec "${0%/*}/maintenance-mise" "$@" ;;
    'bootstrap dotfiles save')
        printf '%s\n' save >> "$HOME/history-calls"
        if [ -n "${DOTS_TEST_NATIVE_MISE:-}" ]; then
            exec "$DOTS_TEST_NATIVE_MISE" "$@"
        fi
        exit 0 ;;
    *) exec "${0%/*}/maintenance-mise" "$@" ;;
esac
printf '%s\n' "$scope" >> "$HOME/history-calls"
if [ -n "${DOTS_TEST_NATIVE_MISE:-}" ]; then
    exec "$DOTS_TEST_NATIVE_MISE" "$@"
fi
cat "$HOME/$scope"
[ ! -f "$HOME/$scope-failure" ]
"#;
