#[path = "p07/isolation.rs"]
mod isolation;
mod support;

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use isolation::{FixtureFile, IsolatedWorld, executable_on_path, fixture_editor, fixture_file};
use nix::libc;
use support::FixtureCommand;

const FIXTURE_GITHUB: &str = "https://github.com/dots-fixture/source";
const TEXT: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
const PRIVATE_MARKER: &str = "FIXTURE_PRIVATE_LINE";

fn finish(command: &mut FixtureCommand<'_>) -> Output {
    command
        .output()
        .unwrap_or_else(|error| panic!("{:?}: {error}", **command))
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

fn success(command: &mut FixtureCommand<'_>) -> String {
    let output = finish(command);
    assert!(output.status.success(), "{:?}: {output:?}", **command);
    stdout(&output).trim().to_owned()
}

fn put(path: &Path, content: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn script(path: &Path, body: &str) {
    put(path, format!("#!/bin/sh\nset -eu\n{body}\n"));
    chmod(path, 0o700);
}

fn sh(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn has(text: &str, expected: &str) {
    assert!(
        text.to_ascii_lowercase()
            .contains(&expected.to_ascii_lowercase()),
        "expected {expected:?} in {text}"
    );
}

fn absent(text: &str, unwanted: &str) {
    assert!(!text.contains(unwanted), "found {unwanted:?} in {text}");
}

#[test]
fn native_mise_adapter_is_private_after_nested_environment_reset() {
    let world = IsolatedWorld::new();
    let fixture = world.root().parent().unwrap();
    let boundary = fixture.parent().unwrap();
    let nested = fixture.join("native-call");
    fs::create_dir(&nested).unwrap();
    let native = fixture.join("observe-native-env");
    let marker = fixture.join("native-started");
    script(&native, &format!(": > {}\nexec /usr/bin/env", sh(&marker)));
    let adapter = world.native_mise_adapter(&native);
    let other = IsolatedWorld::new();
    let symlink = fixture.join("redirected-native-path");
    std::os::unix::fs::symlink(other.root(), &symlink).unwrap();
    let reset = fixture.join("reset-native-env");
    let command = world.tool(&reset);
    let mise_keys = command
        .get_envs()
        .filter_map(|(key, _)| key.to_str())
        .filter(|key| key.starts_with("MISE_") || key.starts_with("__MISE_"))
        .collect::<Vec<_>>()
        .join(" ");
    script(
        &reset,
        &format!(
            r#"unset {mise_keys}
export HOME={home}
export MISE_CONFIG_DIR="$HOME" MISE_SYSTEM_CONFIG_DIR="$HOME/system"
export MISE_DATA_DIR="$HOME/data" MISE_STATE_DIR="$HOME/state" MISE_CACHE_DIR="$HOME/cache"
case "$1" in
    override)
        export MISE_SYSTEM_DATA_DIR="$HOME/override-system-data"
        export MISE_SYSTEM_PLUGINS_DIRS="$HOME/override-plugins" ;;
    cwd) cd "$2" ;;
    argv) exec {adapter} -C "$2" --version ;;
    env) export "$2=$3" ;;
esac
exec {adapter} -C "$HOME" --version"#,
            home = sh(&nested),
            adapter = sh(&adapter),
        ),
    );
    for variant in ["fallback", "override"] {
        let output = finish(world.tool(&reset).arg(variant));
        assert!(output.status.success(), "{output:?}");
        let values: std::collections::BTreeMap<_, _> = stdout(&output)
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let system_data = if variant == "fallback" {
            fixture.join("mise-system/data")
        } else {
            nested.join("override-system-data")
        };
        let plugins = if variant == "fallback" {
            system_data.join("plugins")
        } else {
            nested.join("override-plugins")
        };
        for (key, expected) in [
            ("HOME", nested.clone()),
            ("MISE_CONFIG_DIR", nested.clone()),
            ("MISE_SYSTEM_CONFIG_DIR", nested.join("system")),
            ("MISE_SYSTEM_CONFIG_FILE", nested.join("system/config.toml")),
            ("MISE_DATA_DIR", nested.join("data")),
            ("MISE_STATE_DIR", nested.join("state")),
            ("MISE_CACHE_DIR", nested.join("cache")),
            ("MISE_INSTALLS_DIR", nested.join("data/installs")),
            ("MISE_SYSTEM_DATA_DIR", system_data.clone()),
            ("MISE_SYSTEM_INSTALLS_DIR", system_data.join("installs")),
            ("MISE_SYSTEM_SHIMS_DIR", system_data.join("shims")),
            ("MISE_SYSTEM_PLUGINS_DIRS", plugins),
        ] {
            assert_eq!(
                values.get(key).copied(),
                expected.to_str(),
                "{key} ({variant})"
            );
        }
        assert_eq!(values.get("MISE_OFFLINE"), Some(&"1"));
        assert_eq!(values.get("MISE_AUTO_INSTALL"), Some(&"0"));
        let git_ceiling = std::env::join_paths([fixture, boundary]).unwrap();
        assert_eq!(
            values.get("GIT_CEILING_DIRECTORIES").copied(),
            git_ceiling.to_str()
        );
        fs::remove_file(&marker).unwrap();
    }
    for path in [other.root(), &symlink, boundary] {
        for input in ["cwd", "argv"] {
            let output = finish(world.tool(&reset).arg(input).arg(path));
            assert!(!output.status.success(), "{output:?}");
            has(
                &String::from_utf8_lossy(&output.stderr),
                "fixture boundary refused",
            );
            assert!(!marker.exists());
        }
        for key in [
            "MISE_CONFIG_DIR",
            "MISE_SYSTEM_DATA_DIR",
            "MISE_SYSTEM_PLUGINS_DIRS",
            "MISE_INSTALLS_DIR",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
        ] {
            let output = finish(world.tool(&reset).args(["env", key]).arg(path));
            assert!(!output.status.success(), "{output:?}");
            has(&String::from_utf8_lossy(&output.stderr), key);
            assert!(!marker.exists());
        }
    }
    let output = finish(
        world
            .tool(&reset)
            .args(["env", "GIT_CEILING_DIRECTORIES"])
            .arg(other.root()),
    );
    assert!(!output.status.success(), "{output:?}");
    has(
        &String::from_utf8_lossy(&output.stderr),
        "GIT_CEILING_DIRECTORIES",
    );
    assert!(!marker.exists());
    let reader = world.fixture_reader();
    let local_body = fixture.join("body.md");
    put(&local_body, "fixture body\n");
    let read_body = finish(world.tool(&reader).arg(&local_body));
    assert!(read_body.status.success(), "{read_body:?}");
    assert_eq!(stdout(&read_body), "fixture body\n");
    for outside_body in [
        other.root().join("config.toml"),
        symlink.join("config.toml"),
    ] {
        let output = finish(world.tool(&reader).arg(outside_body));
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
}

struct Repo {
    world: IsolatedWorld,
    scratch: PathBuf,
    a: PathBuf,
    b: PathBuf,
    origin: PathBuf,
    tools: PathBuf,
    mise: PathBuf,
    mise_real: Option<PathBuf>,
    gitleaks_real: Option<PathBuf>,
    oracle: bool,
    config: String,
}

impl Repo {
    fn new() -> Self {
        let world = IsolatedWorld::new();
        let a = world.root().to_path_buf();
        let scratch = a.parent().unwrap().to_path_buf();
        let b = scratch.join("peer");
        let origin = world.bare_remote("source.git");
        let tools = scratch.join("source-tools");
        let mise = tools.join("mise-fixture");
        fs::create_dir_all(&tools).unwrap();
        fs::create_dir_all(scratch.join("mise-system")).unwrap();
        fs::create_dir_all(scratch.join("tmp")).unwrap();
        let mise_real = executable_on_path(&world.git(), "mise")
            .map(|native| world.native_mise_adapter(&native));
        let gitleaks_real = executable_on_path(&world.git(), "gitleaks");
        let dots = world.dots();
        let mut prefix = [0; 2];
        fs::File::open(dots.get_program())
            .unwrap()
            .read_exact(&mut prefix)
            .unwrap();
        let oracle = prefix == *b"#!";
        let oracle_tools = oracle.then_some(["node", "python3"]).into_iter().flatten();
        for tool in ["git", "jq", "age", "age-keygen"]
            .into_iter()
            .chain(oracle_tools)
        {
            if let Some(binary) = executable_on_path(&world.git(), tool) {
                symlink(binary, tools.join(tool)).unwrap();
            }
        }
        let file_url = format!("file://{}", origin.display());
        let (setting, remote) = if oracle {
            ("dots-fixture/source", FIXTURE_GITHUB)
        } else {
            (file_url.as_str(), file_url.as_str())
        };
        let config = format!(
            "[vars]\ndots_profiles = \"laptop selected\"\ndots_source_repo = {setting:?}\n"
        );
        let fixture = Self {
            world,
            scratch,
            a,
            b,
            origin,
            tools,
            mise,
            mise_real,
            gitleaks_real,
            oracle,
            config,
        };
        for item in fs::read_dir(&fixture.a).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                fs::remove_dir_all(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
        fixture.git(&fixture.a, &["init", "-q", "--initial-branch=main"]);
        fixture.seed_identity(&fixture.a);
        if oracle {
            fixture.git(
                &fixture.a,
                &["config", &format!("url.{file_url}.insteadOf"), remote],
            );
        }
        fixture.git(&fixture.a, &["remote", "add", "origin", remote]);
        fixture.write(&fixture.a, "config.toml", &fixture.config);
        fixture.write(&fixture.a, "text", TEXT);
        fixture.write(&fixture.a, ".gitignore",
            "/config.local.toml\n/miserc.toml\n/config.host-*.toml\n/mise.lock\n/mise.*.lock\n/.setup-state/\n/locks/\n");
        fixture.commit(&fixture.a, "Initial");
        fixture.git(&fixture.a, &["push", "-q", "origin", "main"]);
        fixture.git(&fixture.a, &["switch", "-qc", "dev"]);
        fixture.git(&fixture.a, &["push", "-q", "origin", "dev"]);
        fixture.git(
            &fixture.scratch,
            &[
                "clone",
                "-q",
                "--branch",
                "dev",
                &file_url,
                fixture.b.to_str().unwrap(),
            ],
        );
        fixture.seed_identity(&fixture.b);
        if oracle {
            fixture.git(
                &fixture.b,
                &["config", &format!("url.{file_url}.insteadOf"), remote],
            );
            fixture.git(&fixture.b, &["remote", "set-url", "origin", remote]);
        }
        script(&fixture.mise, MISE_FIXTURE);
        symlink(&fixture.mise, fixture.tools.join("mise")).unwrap();
        script(&fixture.tools.join("gitleaks"), GITLEAKS_FIXTURE);
        fixture
    }

    fn path(&self) -> std::ffi::OsString {
        std::env::join_paths([
            self.tools.as_path(),
            Path::new("/usr/bin"),
            Path::new("/bin"),
        ])
        .unwrap()
    }

    fn scanner_fixture_path(&self) -> std::ffi::OsString {
        let tools: &[&str] = if self.oracle {
            &["bash", "dirname", "grep", "sed", "tar"]
        } else {
            &["grep"]
        };
        for &tool in tools {
            let path = self.tools.join(tool);
            if !path.exists() {
                let executable = executable_on_path(&self.world.git(), tool)
                    .unwrap_or_else(|| panic!("fixture requires {tool}"));
                symlink(executable, path).unwrap();
            }
        }
        std::env::join_paths([&self.tools]).unwrap()
    }

    fn git_command(&self, root: &Path) -> FixtureCommand<'_> {
        let mut command = self.world.git();
        command
            .current_dir(root)
            .env("PATH", self.path())
            .env("TMPDIR", self.scratch.join("tmp"))
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Dots fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Dots fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
        command
    }

    fn git(&self, root: &Path, args: &[&str]) -> String {
        success(self.git_command(root).args(args))
    }

    fn seed_identity(&self, root: &Path) {
        self.git(root, &["config", "user.name", "Dots fixture"]);
        self.git(root, &["config", "user.email", "fixture@example.invalid"]);
    }

    fn write(&self, root: &Path, path: &str, content: impl AsRef<[u8]>) {
        put(&root.join(path), content);
    }

    fn commit(&self, root: &Path, title: &str) -> String {
        self.git(root, &["add", "-A"]);
        self.git(root, &["commit", "-qm", title]);
        self.dev_or_head(root)
    }

    fn dev_or_head(&self, root: &Path) -> String {
        self.git(root, &["rev-parse", "HEAD"])
    }

    fn publish(&self, path: &str, contents: impl AsRef<[u8]>) -> String {
        self.write(&self.b, path, contents);
        let tip = self.commit(&self.b, "Peer edit");
        self.git(&self.b, &["push", "-q", "origin", "HEAD:dev"]);
        tip
    }

    fn reviewed(&self, path: &str, contents: impl AsRef<[u8]>) -> String {
        self.git(&self.b, &["switch", "-qc", "main", "origin/main"]);
        self.write(&self.b, path, contents);
        let tip = self.commit(&self.b, "Reviewed");
        self.git(&self.b, &["push", "-q", "origin", "HEAD:main"]);
        self.git(&self.b, &["switch", "-q", "dev"]);
        tip
    }

    fn tip(&self, root: &Path) -> String {
        self.git(root, &["rev-parse", "refs/heads/dev"])
    }

    fn remote(&self) -> String {
        self.tip(&self.origin)
    }

    fn blob(&self, revision: &str, path: &str) -> String {
        self.git(&self.origin, &["show", &format!("{revision}:{path}")])
    }

    fn tree(&self) -> String {
        self.git(&self.origin, &["ls-tree", "-r", "--name-only", "dev"])
    }

    fn clean(&self, root: &Path) {
        assert_eq!(self.git(root, &["status", "--porcelain"]), "");
    }

    fn check_ancestor(&self, ancestor: &str, tip: &str) {
        self.git(
            &self.origin,
            &["merge-base", "--is-ancestor", ancestor, tip],
        );
    }

    fn origin_offline(&self) -> PathBuf {
        let offline = self.origin.with_extension("offline");
        fs::rename(&self.origin, &offline).unwrap();
        offline
    }

    fn git_dir(&self, root: &Path) -> PathBuf {
        PathBuf::from(self.git(root, &["rev-parse", "--absolute-git-dir"]))
    }

    fn hook(&self, root: &Path, hook: &str, body: &str) {
        script(&self.git_dir(root).join("hooks").join(hook), body);
    }

    fn validation_hook(&self, body: &str) -> PathBuf {
        let hook = self.scratch.join("during-validation");
        script(&hook, body);
        hook
    }

    fn dots(&self, root: &Path, args: &[&str]) -> FixtureCommand<'_> {
        let mut command = self.world.dots();
        command.args(args);
        command
            .current_dir(root)
            .env("PATH", self.path())
            .env("TMPDIR", self.scratch.join("tmp"))
            .env("MISE_SYSTEM_CONFIG_DIR", self.scratch.join("mise-system"))
            .env("MISE_CEILING_PATHS", &self.scratch)
            .env("MISE_TRUSTED_CONFIG_PATHS", root)
            .env("MISE_OFFLINE", "1")
            .env("DOTS_ROOT", root)
            .env("FIXTURE_LIVE_ROOT", root)
            .env("MISE_CONFIG_DIR", root)
            .env("DOTS_MISE_BIN", &self.mise)
            .env("SETUP_PROFILE", "laptop")
            .env("SETUP_MACHINE_ID", "fixture")
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_AUTO_UPDATE", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn sync(&self, root: &Path) -> String {
        self.sync_env(root, &[])
    }

    fn sync_env(&self, root: &Path, env: &[(&str, &str)]) -> String {
        dots_zero(
            self.dots(root, &["--verbose", "sync"])
                .envs(env.iter().copied()),
        )
    }

    fn replay(&self, root: &Path) -> String {
        dots_zero(&mut self.dots(root, &["--plain", "sync", "--last"]))
    }
}

fn dots_zero(command: &mut FixtureCommand<'_>) -> String {
    let output = finish(command);
    assert_eq!(output.status.code(), Some(0), "{:?}: {output:?}", **command);
    format!(
        "{}{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    )
}

const MISE_FIXTURE: &str = r#"
if [ "${1:-}" = --version ]; then
    printf '2026.11.18\n'
    exit 0
fi
if [ "${1:-}" = -C ]; then
    config=$2
    shift 2
else
    config=${DOTS_ROOT:?}
fi
if [ "${1:-}" = -E ]; then shift 2; fi
case "${1:-} ${2:-} ${3:-}" in
  'config get --file')
    case "$5" in
      'dotfiles."~/.fixture-history"')
        exec grep '^"~/.fixture-history" = ' "$4" ;;
      'dotfiles."~/.fixture-history".encrypt')
        exec sed -nE '/^"~\/.fixture-history" = /s/.*encrypt = (true|false).*/\1/p' "$4" ;;
    esac
    name=${5#vars.}
    value=$(sed -n 's/^'"$name"' = "\(.*\)"$/\1/p' "$4")
    [ -n "$value" ] && printf '%s\n' "$value"
    ;;
  'ls --json '|'ls --current --json')
    printf '{}\n'
    ;;
  'bootstrap packages status'|'bootstrap status --json')
    printf '{}\n'
    ;;
  'config ls --json')
    if [ -n "${FIXTURE_VALIDATE:-}" ] && [ "$config" != "$FIXTURE_LIVE_ROOT" ]; then
        if [ -n "${FIXTURE_VALIDATION_PARENT:-}" ]; then
            printf '%s\n' "$PPID" > "$FIXTURE_VALIDATION_PARENT"
        fi
        "$FIXTURE_VALIDATE"
    fi
    if [ "$config" != "${FIXTURE_LIVE_ROOT:-}" ] && grep -q 'INVALID' "$config/config.toml"; then
        printf 'invalid config.toml\n' >&2
        exit 1
    fi
    jq -n --arg path "$config/config.toml" '[{path: $path}]'
    ;;
  'bootstrap dotfiles paths')
    if [ -n "${FIXTURE_DOTFILE_PATHS:-}" ]; then
        printf '%s\n' "$FIXTURE_DOTFILE_PATHS"
    else
        printf '{"entries": [], "invalid": []}\n'
    fi
    ;;
  'bootstrap dotfiles --local')
    [ "${4:-} ${5:-}" = 'paths --json' ]
    printf '{"entries": [], "invalid": []}\n'
    ;;
  'bootstrap dotfiles status')
    printf '{"files": [], "edits": []}\n'
    ;;
  'bootstrap dotfiles save')
    if [ -n "${FIXTURE_SAVE_ATTEMPTED:-}" ]; then touch "$FIXTURE_SAVE_ATTEMPTED"; fi
    if [ -n "${FIXTURE_SAVE_OUTPUT:-}" ]; then "$FIXTURE_SAVE_OUTPUT"; fi
    if [ -n "${FIXTURE_SAVE_FAILURE:-}" ]; then printf '%s\n' "$FIXTURE_SAVE_FAILURE" >&2; exit 1; fi
    if [ -n "${FIXTURE_SAVED:-}" ]; then touch "$FIXTURE_SAVED"; fi
    ;;
  'bootstrap dotfiles apply')
    case " $* " in
      *' --dry-run '*)
        for file in "$config"/config.local.toml "$config"/config.host-*.toml; do
            [ -f "$file" ] || continue
            source=$(sed -n 's/^source = "\(.*\)"$/\1/p' "$file")
            if [ -n "$source" ] && [ ! -e "$config/$source" ]; then
                printf 'sources do not exist: %s\n' "$source" >&2
                exit 1
            fi
        done
        ;;
      *)
        if [ -n "${FIXTURE_APPLY_FAILURE:-}" ]; then printf '%s\n' "$FIXTURE_APPLY_FAILURE" >&2; exit 1; fi
        ;;
    esac
    ;;
  'exec -- '*)
    shift 2
    exec "$@"
    ;;
  'which '*)
    exit 1
    ;;
esac
"#;

const GITLEAKS_FIXTURE: &str = r#"
report=''
while [ "$#" -gt 1 ]; do
  case "$1" in
    --report-path) report=$2; shift ;;
    --report-path=*) report=${1#*=} ;;
  esac
  shift
done
if [ -n "$report" ]; then printf '[]\n' > "$report"; fi
"#;

macro_rules! sync_case {
    (active $name:ident, $body:expr) => {
        #[test]
        fn $name() {
            ($body)();
        }
    };
    ($name:ident, $body:expr) => {
        #[test]
        #[ignore = "P-08 sync; run unchanged cases against the v0.4.0 oracle"]
        fn $name() {
            ($body)();
        }
    };
}

sync_case!(
    active unchanged_clean_checkout_has_one_receipt_and_no_changes,
    || {
        let f = Repo::new();
        let before = f.remote();
        let receipt = f.sync(&f.a);
        has(&receipt, "pulled dev +0, main +0");
        has(&receipt, "dots sync:");
        assert_eq!(receipt.lines().count(), 1, "{receipt}");
        assert_eq!(f.tip(&f.a), before);
        assert_eq!(f.remote(), before);
        f.clean(&f.a);
    }
);

sync_case!(active unchanged_offline_commit_is_saved_then_shared, || {
    let f = Repo::new();
    let before = f.remote();
    let offline = f.origin_offline();
    f.write(&f.a, "local", "saved offline\n");
    let receipt = f.sync(&f.a);
    has(&receipt, "offline");
    has(&receipt, if f.oracle { "1 commit" } else { "saved 1 locally" });
    let snapshot = f.tip(&f.a);
    assert_ne!(before, snapshot);
    assert_eq!(
        f.git(&f.a, &["rev-list", "--count", "origin/dev..dev"]),
        "1"
    );
    assert_eq!(f.git(&f.a, &["show", "dev:local"]), "saved offline");
    has(&f.replay(&f.a), "saved locally");
    f.clean(&f.a);
    fs::rename(offline, &f.origin).unwrap();
    has(&f.sync(&f.a), "pushed");
    assert_eq!(f.remote(), snapshot);
    has(&f.replay(&f.a), "shared on dev");
});

sync_case!(active unchanged_reviewed_main_is_shared_and_received, || {
    let f = Repo::new();
    let reviewed = f.reviewed("reviewed", "reviewed change\n");
    has(&f.sync(&f.a), "pushed");
    let replay = f.replay(&f.a);
    has(&replay, "shared on dev");
    has(&replay, "updated in this checkout");
    assert_eq!(replay.matches("reviewed").count(), 2, "{replay}");
    f.check_ancestor(&reviewed, "dev");
    assert_eq!(f.remote(), reviewed);
    assert_eq!(read(&f.a.join("reviewed")), "reviewed change\n");
});

sync_case!(
    active unchanged_missing_dev_reports_files_from_created_branch,
    || {
        let f = Repo::new();
        let before = f.remote();
        f.git(&f.origin, &["update-ref", "-d", "refs/heads/dev"]);
        assert_eq!(f.git(&f.a, &["rev-parse", "origin/dev"]), before);
        f.sync(&f.a);
        let replay = f.replay(&f.a);
        has(&replay, "shared on dev");
        has(&replay, "config.toml");
        has(&replay, "text");
        assert_eq!(f.remote(), before);
    }
);

fn publish_during_validation(f: &Repo, revision: &str) -> PathBuf {
    let guard = f.scratch.join("race-once");
    f.validation_hook(&format!(
        "if [ -e {guard} ]; then exit 0; fi\n: > {guard}\ngit -C {peer} fetch -q {local} {revision}\ngit -C {peer} push -q origin FETCH_HEAD:dev",
        guard = sh(&guard), local = sh(&f.a), peer = sh(&f.b),
    ))
}

sync_case!(
    active unchanged_accepted_push_reports_only_replaced_remote_tip,
    || {
        let f = Repo::new();
        f.write(&f.a, "first-queued", "queued first\n");
        let advanced = f.commit(&f.a, "Queued");
        let hook = publish_during_validation(&f, &advanced);
        f.write(&f.a, "second-queued", "queued second\n");
        let receipt = f.sync_env(&f.a, &[("FIXTURE_VALIDATE", hook.to_str().unwrap())]);
        has(&receipt, "pushed");
        let replay = f.replay(&f.a);
        has(&replay, "second-queued");
        absent(&replay, "first-queued");
        assert_eq!(f.blob("dev", "first-queued"), "queued first");
        assert_eq!(f.blob("dev", "second-queued"), "queued second");
        assert_eq!(f.remote(), f.tip(&f.a));
    }
);

sync_case!(
    active unchanged_concurrent_snapshot_is_not_reported_as_local_push,
    || {
        let f = Repo::new();
        let hook = publish_during_validation(&f, "dev");
        f.write(&f.a, "concurrent", "concurrently shared\n");
        let receipt = f.sync_env(&f.a, &[("FIXTURE_VALIDATE", hook.to_str().unwrap())]);
        has(&receipt, "already shared");
        let replay = f.replay(&f.a);
        absent(&replay, "Shared on dev");
        absent(&replay, "Saved locally");
        assert_eq!(f.remote(), f.tip(&f.a));
        assert_eq!(f.blob("dev", "concurrent"), "concurrently shared");
    }
);

sync_case!(
    active unchanged_concurrent_snapshot_and_reviewed_push_distinguished,
    || {
        let f = Repo::new();
        f.reviewed("actual-share", "reviewed change\n");
        let hook = publish_during_validation(&f, "dev");
        f.write(&f.a, "concurrent", "concurrently shared\n");
        let receipt = f.sync_env(&f.a, &[("FIXTURE_VALIDATE", hook.to_str().unwrap())]);
        has(&receipt, "already shared");
        has(&receipt, "pushed");
        let replay = f.replay(&f.a);
        has(&replay, "actual-share");
        absent(&replay, "concurrent (added)");
        assert_eq!(f.blob("dev", "concurrent"), "concurrently shared");
    }
);

#[test]
fn new_peer_revert_keeps_the_reverted_path_in_saved_locally() {
    let f = Repo::new();
    let published = f.scratch.join("peer-published");
    let hook = f.validation_hook(&format!(
        "if [ -e {published} ]; then exit 0; fi\n\
         git -C {peer} fetch -q {local} dev\n\
         git -C {peer} push -q origin FETCH_HEAD:dev\n\
         git -C {peer} rev-parse FETCH_HEAD > {published}\n\
         git -C {peer} reset -q --hard FETCH_HEAD\n\
         git -C {peer} rm -q -- notes\n\
         git -C {peer} commit -qm 'Revert notes only'\n\
         git -C {peer} push -q origin HEAD:dev",
        published = sh(&published),
        peer = sh(&f.b),
        local = sh(&f.a),
    ));
    f.write(&f.a, "notes", "saved before peer revert\n");
    f.write(&f.a, "peer-kept", "still at the published tip\n");
    let live = f.sync_env(&f.a, &[("FIXTURE_VALIDATE", hook.to_str().unwrap())]);
    let replay = f.replay(&f.a);
    let published = read(&published).trim().to_owned();
    assert_ne!(published, f.remote());
    f.check_ancestor(&published, "dev");
    assert_eq!(f.blob(&published, "notes"), "saved before peer revert");
    assert_eq!(f.blob("dev", "peer-kept"), "still at the published tip");
    assert!(!f.tree().lines().any(|path| path == "notes"));
    assert_eq!(f.tip(&f.a), f.remote());
    for receipt in [live, replay] {
        has(&receipt, "already shared 1 file");
        has(&receipt, "saved 1 locally");
        has(&receipt, "Saved locally\n  notes");
        absent(&receipt, "peer-kept");
        absent(&receipt, "Shared on dev");
    }
}

fn failing_apply_fixture(f: &Repo) -> (PathBuf, PathBuf) {
    f.write(
        &f.a,
        "config.toml",
        format!(
            "{}\n[dotfiles]\n'~/.fixture-apply' = {{ source = 'fixture.tera', mode = 'template' }}\n",
            f.config
        ),
    );
    f.write(&f.a, "fixture.tera", "{{ 'fixture template' }}\n");
    f.commit(&f.a, "Seed native apply fixture");
    f.git(&f.a, &["push", "-q", "origin", "HEAD:dev"]);
    f.git(&f.b, &["pull", "-q", "--ff-only", "origin", "dev"]);

    let real_mise = f.mise_real.as_ref().expect("real mise on fixture PATH");
    let wrapper = f.scratch.join("fail-live-apply");
    let attempted = f.scratch.join("live-apply-attempted");
    script(
        &wrapper,
        &format!(
            r#"config=$DOTS_ROOT
if [ "${{1:-}}" = '-C' ]; then config=$2; fi
case " $* " in
    *' config ls --json '*)
        if [ "$config" != {root} ] && [ -n "${{FIXTURE_VALIDATE:-}}" ]; then
            "$FIXTURE_VALIDATE"
        fi ;;
    *' bootstrap dotfiles apply '*)
        case " $* " in
            *' --dry-run '*) ;;
            *)
                if [ "$config" = {root} ] && [ -n "${{FIXTURE_APPLY_FAILURE:-}}" ]; then
                    : > {attempted}
                    printf '%s\n' "$FIXTURE_APPLY_FAILURE" >&2
                    exit 1
                fi ;;
        esac ;;
esac
exec {real_mise} "$@""#,
            root = sh(&f.a),
            attempted = sh(&attempted),
            real_mise = sh(real_mise),
        ),
    );
    (wrapper, attempted)
}

fn applied_failure(f: &Repo, receipt: &str, attempted: &Path, oracle_message: &str) {
    assert!(
        attempted.is_file(),
        "fixture live apply was not reached: {receipt}"
    );
    if f.oracle {
        has(receipt, oracle_message);
        has(receipt, "dots apply");
    } else {
        has(receipt, "mise dotfile apply failed (exit 1)");
        has(receipt, "rerun dots sync");
        absent(receipt, oracle_message);
    }
}

sync_case!(
    active unchanged_completed_share_checkout_recovery_and_later_apply_failure,
    || {
        let f = Repo::new();
        let (mise, attempted) = failing_apply_fixture(&f);
        let mise = mise.to_str().unwrap();
        f.publish("incoming", "incoming first\n");
        let lock = f.git_dir(&f.a).join("refs/heads/dev.lock");
        let hook = f.validation_hook(&format!("touch {}", sh(&lock)));
        f.write(&f.a, "local", "local snapshot\n");
        let receipt = f.sync_env(
            &f.a,
            &[("DOTS_MISE_BIN", mise), ("FIXTURE_VALIDATE", hook.to_str().unwrap())],
        );
        has(&receipt, "pending");
        assert_ne!(f.tip(&f.a), f.remote());
        has(&f.replay(&f.a), "shared on dev");
        fs::remove_file(lock).unwrap();
        f.git(&f.b, &["pull", "-q", "--ff-only", "origin", "dev"]);
        let latest = f.publish("later", "incoming second\n");
        let receipt = f.sync_env(
            &f.a,
            &[
                ("DOTS_MISE_BIN", mise),
                ("FIXTURE_APPLY_FAILURE", "mise ERROR fixture apply failure"),
            ],
        );
        applied_failure(&f, &receipt, &attempted, "fixture apply failure");
        let replay = f.replay(&f.a);
        has(&replay, "pending checkout");
        has(&replay, "updated in this checkout");
        assert_eq!(f.tip(&f.a), latest);
        assert_eq!(read(&f.a.join("later")), "incoming second\n");
    }
);

sync_case!(active unchanged_git_metadata_is_never_snapshotted, || {
    let f = Repo::new();
    let before = f.remote();
    let metadata = f.a.join("metadata");
    f.git(
        &f.a,
        &["init", "--separate-git-dir", metadata.to_str().unwrap()],
    );
    f.write(&metadata, "private-recovery", PRIVATE_MARKER);
    let receipt = f.sync(&f.a);
    has(&receipt, "git metadata");
    absent(&receipt, PRIVATE_MARKER);
    assert_eq!(f.remote(), before);
    absent(&f.tree(), "metadata");
});

sync_case!(active unchanged_public_edit_push_excludes_private_files, || {
    let f = Repo::new();
    f.write(&f.a, "new", "ordinary public edit\n");
    f.write(&f.a, "config.local.toml", "[env]\nPRIVATE=\"yes\"\n");
    f.write(&f.a, ".setup-state/state.json", "{}\n");
    f.write(&f.a, "locks/tool.lock", "held\n");
    f.write(&f.a, "mise.laptop.lock", "private lockfile\n");
    has(&f.sync(&f.a), "pushed");
    assert_eq!(f.remote(), f.tip(&f.a));
    assert_eq!(f.blob("dev", "new"), "ordinary public edit");
    for private in [
        "config.local.toml",
        ".setup-state",
        "locks/",
        "mise.laptop.lock",
    ] {
        absent(&f.tree(), private);
    }
    f.clean(&f.a);
});

fn tracked_fixture(f: &Repo, encrypted: bool) -> String {
    let declaration = "config.local.toml";
    f.write(
        &f.a,
        declaration,
        format!(
            "[dotfiles]\n\"~/.fixture-history\" = {{ mode = \"track\", encrypt = {encrypted} }}\n"
        ),
    );
    let target = f.world.home().join(".fixture-history");
    put(&target, "fixture native history\n");
    chmod(&target, 0o600);
    serde_json::json!({
        "entries": [{
            "path": "~/.fixture-history",
            "mode": "track",
            "declared_in": f.a.join(declaration)
        }],
        "invalid": []
    })
    .to_string()
}

sync_case!(active unchanged_sync_refuses_unencrypted_history, || {
    let f = Repo::new();
    let before = f.remote();
    let saved = f.scratch.join("saved");
    let marker = saved.to_str().unwrap();
    let encrypted = tracked_fixture(&f, true);
    let allowed = f.sync_env(
        &f.a,
        &[
            ("FIXTURE_SAVED", marker),
            ("FIXTURE_DOTFILE_PATHS", &encrypted),
        ],
    );
    assert!(saved.exists(), "{allowed}");
    fs::remove_file(&saved).unwrap();
    let attempted = f.scratch.join("plaintext-save-attempted");
    let pushed = f.scratch.join("plaintext-push-attempted");
    f.hook(&f.a, "pre-push", &format!("touch {}", sh(&pushed)));
    let plaintext = tracked_fixture(&f, false);
    let receipt = dots_zero(
        f.dots(&f.a, &["--verbose", "sync"])
            .env("FIXTURE_SAVED", marker)
            .env("FIXTURE_DOTFILE_PATHS", plaintext)
            .env("FIXTURE_SAVE_ATTEMPTED", &attempted),
    );
    has(&receipt, "history");
    has(&receipt, "encrypt = true");
    assert!(!saved.exists());
    assert!(!attempted.exists());
    assert!(!pushed.exists());
    assert_eq!(f.remote(), before);
    assert_eq!(f.tip(&f.a), before);
    assert_eq!(
        read(&f.world.home().join(".fixture-history")),
        "fixture native history\n"
    );
});

sync_case!(active unchanged_remote_change_arrives_on_disk, || {
    let f = Repo::new();
    let upstream = f.publish("remote", "from peer\n");
    f.sync(&f.a);
    has(&f.replay(&f.a), "remote");
    assert_eq!(read(&f.a.join("remote")), "from peer\n");
    assert_eq!(f.tip(&f.a), upstream);
    assert_eq!(f.remote(), upstream);
    f.clean(&f.a);
});

sync_case!(active unchanged_independent_hunks_merge_on_both_machines, || {
    let f = Repo::new();
    f.write(&f.a, "text", TEXT.replacen("one", "ONE", 1));
    f.publish("text", TEXT.replace("seven", "SEVEN"));
    f.sync(&f.a);
    f.sync(&f.b);
    let merged = TEXT.replacen("one", "ONE", 1).replace("seven", "SEVEN");
    assert_eq!(read(&f.a.join("text")), merged);
    assert_eq!(read(&f.b.join("text")), merged);
    assert_eq!(f.blob("dev", "text"), merged.trim_end());
    f.clean(&f.a);
    f.clean(&f.b);
});

fn merged_conflict(f: &Repo, receipt: &str) {
    has(receipt, if f.oracle { "conflict" } else { "pushed dev" });
}

sync_case!(
    active unchanged_published_hunk_wins_and_local_hunk_remains_recoverable,
    || {
        let f = Repo::new();
        let local = TEXT.replace("one", "mine").replace("seven", "my tail");
        f.write(&f.a, "text", &local);
        let losing = f.commit(&f.a, "Local conflict");
        let peer = f.publish("text", TEXT.replace("one", "theirs"));
        let receipt = f.sync(&f.a);
        merged_conflict(&f, &receipt);
        assert_eq!(read(&f.a.join("text")), local.replace("mine", "theirs"));
        assert_eq!(f.blob(&losing, "text"), local.trim_end());
        f.check_ancestor(&losing, "dev");
        f.check_ancestor(&peer, "dev");
        f.clean(&f.a);
    }
);

sync_case!(active unchanged_binary_conflict_uses_the_published_file, || {
    let f = Repo::new();
    f.publish("blob", [0, 1, 2]);
    f.sync(&f.a);
    f.write(&f.a, "blob", [0, 9, 9]);
    let losing = f.commit(&f.a, "Local binary");
    f.publish("blob", [0, 7, 7]);
    merged_conflict(&f, &f.sync(&f.a));
    assert_eq!(fs::read(f.a.join("blob")).unwrap(), [0, 7, 7]);
    f.check_ancestor(&losing, "dev");
});

fn file_directory_conflict(published: &str, local: &str) {
    let f = Repo::new();
    f.publish(published, "published\n");
    f.write(&f.a, local, "local\n");
    let local_tip = f.commit(&f.a, "Local conflict");
    merged_conflict(&f, &f.sync(&f.a));
    assert_eq!(read(&f.a.join(published)), "published\n");
    assert_eq!(f.blob(&local_tip, local), "local");
    f.check_ancestor(&local_tip, "dev");
    f.clean(&f.a);
}

sync_case!(active unchanged_published_file_wins_over_local_directory, || {
    file_directory_conflict("collision", "collision/child");
});

sync_case!(active unchanged_published_directory_wins_over_local_file, || {
    file_directory_conflict("collision/child", "collision");
});

sync_case!(
    active unchanged_offline_count_includes_new_snapshot_after_external_ref_move,
    || {
        let f = Repo::new();
        let tip = f.publish("upstream", "updated remotely\n");
        f.git(&f.a, &["fetch", "-q", "origin", "dev"]);
        let task = detached_worktree(&f);
        f.git(&task, &["update-ref", "refs/heads/dev", &tip]);
        f.write(&f.a, "local", "queued offline\n");
        f.origin_offline();
        let receipt = f.sync(&f.a);
        let count = f.git(
            &f.a,
            &["rev-list", "--count", "dev", "^origin/dev", "^origin/main"],
        );
        assert_ne!(count, "0", "{receipt}");
        if f.oracle {
            has(&receipt, &format!("{count} commit"));
        } else {
            has(&receipt, "saved 1 locally");
            has(&receipt, "offline");
            assert_eq!(f.git(&f.a, &["show", "dev:local"]), "queued offline");
            assert_eq!(read(&f.a.join("upstream")), "updated remotely\n");
        }
    }
);

fn one_push_race(f: &Repo, update: &str) {
    f.hook(&f.a, "pre-push", &format!(
        "rm \"$0\"\ncd {}\n{update}\ngit add -A\ngit commit -qm Race\ngit push -q origin HEAD:dev",
        sh(&f.b),
    ));
}

sync_case!(active unchanged_push_race_merges_peer_and_local_edits, || {
    let f = Repo::new();
    one_push_race(&f, "echo raced > raced");
    f.write(&f.a, "local", "local\n");
    has(&f.sync(&f.a), "pushed");
    assert_eq!(f.blob("dev", "raced"), "raced");
    assert_eq!(f.blob("dev", "local"), "local");
    assert_eq!(read(&f.a.join("raced")), "raced\n");
    f.clean(&f.a);
});

fn withdrawn_dev(during_push: bool) {
    let f = Repo::new();
    let withdrawn = f.publish("withdrawn", "peer only\n");
    let remove = format!(
        "git -C {} update-ref -d refs/heads/dev {withdrawn}",
        sh(&f.origin)
    );
    f.write(&f.a, "local", "before deletion\n");
    let receipt = if during_push {
        f.hook(&f.a, "pre-push", &format!("rm \"$0\"\n{remove}"));
        f.sync(&f.a)
    } else {
        let once = sh(&f.scratch.join("withdrawn-during-validation"));
        let hook = f.validation_hook(&format!(
            "if [ -e {once} ]; then exit 0; fi\n{remove}\n: > {once}"
        ));
        f.sync_env(&f.a, &[("FIXTURE_VALIDATE", hook.to_str().unwrap())])
    };
    has(
        &receipt,
        if f.oracle {
            "disappeared"
        } else {
            "published dev or main was removed or rewound"
        },
    );
    assert_eq!(f.git(&f.origin, &["for-each-ref", "refs/heads/dev"]), "");
    assert_eq!(f.git(&f.a, &["show", "dev:local"]), "before deletion");
    assert!(!f.a.join("withdrawn").exists());
    let offline = f.origin_offline();
    has(&f.sync(&f.a), "offline");
    assert!(!f.a.join("withdrawn").exists());
    fs::rename(offline, &f.origin).unwrap();
    has(&f.sync(&f.a), "pushed");
    absent(&f.tree(), "withdrawn");
}

sync_case!(active unchanged_deleted_dev_after_fetch_stays_withdrawn, || {
    withdrawn_dev(false);
});
sync_case!(active unchanged_deleted_dev_during_push_stays_withdrawn, || {
    withdrawn_dev(true);
});

sync_case!(active unchanged_sync_pushes_no_tags_or_main_changes, || {
    let f = Repo::new();
    let main = f.git(&f.origin, &["rev-parse", "main"]);
    f.git(&f.a, &["config", "push.followTags", "true"]);
    f.git(
        &f.origin,
        &["config", "receive.denyNonFastForwards", "true"],
    );
    f.git(&f.a, &["tag", "-am", "Local label", "local-label"]);
    f.write(&f.a, "local", "to dev\n");
    has(&f.sync(&f.a), "pushed");
    assert_eq!(f.remote(), f.tip(&f.a));
    assert_eq!(f.git(&f.origin, &["tag", "--list"]), "");
    assert_eq!(f.git(&f.origin, &["rev-parse", "main"]), main);
});

sync_case!(active unchanged_retry_leaves_withdrawn_history_withdrawn, || {
    let f = Repo::new();
    let initial = f.remote();
    let withdrawn = f.publish("withdrawn", "remote update\n");
    f.write(&f.a, "local", "local edit\n");
    f.hook(
        &f.a,
        "pre-push",
        &format!(
            "rm \"$0\"\ngit -C {} update-ref refs/heads/dev {initial} {withdrawn}",
            sh(&f.origin)
        ),
    );
    has(
        &f.sync(&f.a),
        if f.oracle {
            "history was removed"
        } else {
            "published dev or main was removed or rewound"
        },
    );
    assert_eq!(f.remote(), initial);
    assert_eq!(f.git(&f.a, &["show", "dev:local"]), "local edit");
    assert!(!f.a.join("withdrawn").exists());
    has(&f.sync(&f.a), "pushed");
    absent(&f.tree(), "withdrawn");
});

fn endless_race(f: &Repo, withdraw: bool) {
    let tail = if withdraw {
        format!(
            "git -C {} update-ref refs/heads/dev \"$(git rev-parse HEAD~1)\" \"$(git rev-parse HEAD)\"\nrm {}",
            sh(&f.origin),
            sh(&f.git_dir(&f.a).join("hooks/pre-push"))
        )
    } else {
        "exit 1".to_owned()
    };
    f.hook(&f.a, "pre-push", &format!(
        "cd {}\nfor n in 1 2 3 4 5; do\n  if [ ! -e \"change-$n\" ]; then\n    if [ \"$n\" -ge {} ]; then break; fi\n    echo \"$n\" > \"change-$n\"\n    git add \"change-$n\"\n    git commit -qm \"Advance $n\"\n    git push -q origin HEAD:dev\n    exit 0\n  fi\ndone\n{tail}",
        sh(&f.b), if withdraw { 4 } else { 5 },
    ));
}

sync_case!(
    active unchanged_final_retry_checks_the_tip_used_for_that_merge,
    || {
        let f = Repo::new();
        f.write(&f.a, "local", "local edit\n");
        endless_race(&f, true);
        let receipt = f.sync(&f.a);
        if f.oracle {
            has(&receipt, "history was removed");
        } else {
            has(&receipt, "conditional dev push was rejected");
            assert_eq!(f.blob("dev", "change-2"), "2");
            absent(&f.tree(), "change-3");
        }
        assert!(!f.a.join("change-3").exists());
        assert_eq!(f.git(&f.a, &["show", "dev:local"]), "local edit");
        has(&f.sync(&f.a), "pushed");
        absent(&f.tree(), "change-3");
    }
);

sync_case!(active unchanged_only_repeated_races_report_a_moving_branch, || {
    let f = Repo::new();
    endless_race(&f, false);
    f.write(&f.a, "local", "waiting\n");
    let receipt = f.sync(&f.a);
    if f.oracle {
        has(&receipt, "kept moving");
    } else {
        has(&receipt, "conditional dev push was rejected");
        assert_eq!(f.remote(), f.tip(&f.b));
        for attempt in 1..=4 {
            assert_eq!(
                f.blob("dev", &format!("change-{attempt}")),
                attempt.to_string()
            );
        }
    }
    absent(&f.tree(), "local");
    assert_eq!(f.git(&f.a, &["show", "dev:local"]), "waiting");
});

sync_case!(
    active unchanged_retry_reapplies_main_priority_after_racing_dev,
    || {
        let f = Repo::new();
        let main = f.reviewed("text", TEXT.replace("one", "reviewed"));
        one_push_race(
            &f,
            "printf 'raced\\ntwo\\nthree\\nfour\\nfive\\nsix\\nseven\\n' > text",
        );
        f.write(&f.a, "local", "local\n");
        has(&f.sync(&f.a), "pushed");
        assert_eq!(read(&f.a.join("text")), TEXT.replace("one", "reviewed"));
        f.check_ancestor(&main, "dev");
        f.check_ancestor(&f.tip(&f.b), "dev");
    }
);

sync_case!(
    active unchanged_rejected_push_reports_reason_without_hook_or_patch_content,
    || {
        let f = Repo::new();
        let before = f.remote();
        f.hook(&f.origin, "pre-receive", "cat >/dev/null\necho FIXTURE_PRIVATE_REMOTE\necho FIXTURE_PRIVATE_REMOTE_ERR >&2\nexit 1");
        f.hook(&f.a, "pre-push", "cat >/dev/null\necho FIXTURE_PRIVATE_LOCAL\necho FIXTURE_PRIVATE_LOCAL_ERR >&2\ngit show --format= HEAD >&2");
        f.write(&f.a, "local", PRIVATE_MARKER);
        for args in [&["--plain", "sync"][..], &["--verbose", "sync"][..]] {
            let receipt = dots_zero(&mut f.dots(&f.a, args));
            for output in [receipt, f.replay(&f.a)] {
                if f.oracle {
                    has(&output, "pre-receive hook declined");
                } else {
                    has(&output, "conditional dev push was rejected");
                    has(&output, "remote branch policies");
                }
                absent(&output, "FIXTURE_PRIVATE_");
                absent(&output, "diff --git");
            }
        }
        assert_eq!(f.remote(), before);
        assert_eq!(f.git(&f.a, &["show", "dev:local"]), PRIVATE_MARKER);
    }
);

fn unconfirmed_push_issue(f: &Repo, receipt: &str, expected: &str) {
    let candidate = f.tip(&f.a);
    assert_ne!(candidate, expected);
    for report in [receipt, &f.replay(&f.a)] {
        has(report, "dev push unconfirmed");
        has(report, &format!("candidate {candidate}"));
        has(report, &format!("expected {expected}"));
        has(report, "compare refs/heads/dev");
    }
}

sync_case!(
    active unchanged_failed_push_preserves_authentication_diagnostic,
    || {
        let f = Repo::new();
        let before = f.remote();
        f.hook(&f.a, "pre-push", "echo \"fatal: Authentication failed for 'https://fixture.invalid/dots.git/'\" >&2\nexit 1");
        f.write(&f.a, "local", "waiting\n");
        let receipt = f.sync(&f.a);
        if f.oracle {
            has(&receipt, "authentication failed");
        } else {
            unconfirmed_push_issue(&f, &receipt, &before);
        }
        absent(&receipt, "kept moving");
        absent(&receipt, "OFFLINE");
        assert_eq!(f.remote(), before);
    }
);

sync_case!(
    active unchanged_failed_push_reports_reason_and_offline_when_origin_disappears,
    || {
        let f = Repo::new();
        let before = f.remote();
        let offline = f.origin.with_extension("offline");
        f.hook(&f.a, "pre-push", &format!(
        "mv {} {}\necho 'fatal: unable to access origin: Could not resolve host' >&2\nexit 1",
        sh(&f.origin), sh(&offline),
    ));
        f.write(&f.a, "local", "waiting\n");
        let receipt = f.sync(&f.a);
        if f.oracle {
            has(&receipt, "could not resolve host");
            has(&receipt, "offline");
            has(&receipt, "1 commit");
        } else {
            unconfirmed_push_issue(&f, &receipt, &before);
            has(&receipt, "saved 1 locally");
        }
        assert_eq!(f.git(&f.a, &["show", "dev:local"]), "waiting");
        fs::rename(offline, &f.origin).unwrap();
    }
);

sync_case!(
    active unchanged_git_diagnostic_never_discloses_url_credentials,
    || {
        let f = Repo::new();
        let before = f.remote();
        let synthetic = "FIXTURE_CREDENTIAL_CANARY";
        f.hook(&f.a, "pre-push", &format!(
        "echo \"fatal: unable to access 'https://fixture-user:{synthetic}@fixture.invalid/dots.git/': The requested URL returned error: 403\" >&2\nexit 1",
    ));
        f.write(&f.a, "local", "waiting\n");
        let receipt = f.sync(&f.a);
        if !f.oracle {
            unconfirmed_push_issue(&f, &receipt, &before);
        }
        for text in [receipt, f.replay(&f.a)] {
            if f.oracle {
                has(&text, "403");
            }
            absent(&text, synthetic);
            absent(&text, "fixture-user:");
        }
    }
);

sync_case!(active unchanged_hanging_push_times_out, || {
    use std::time::{Duration, Instant};
    let f = Repo::new();
    let before = f.remote();
    let hook_pid = f.scratch.join("hanging-push.pid");
    let lifetime = f.scratch.join("hanging-push.lock");
    let hook = if f.oracle {
        format!("printf '%s\\n' \"$$\" > {}\nexec sleep 68", sh(&hook_pid))
    } else {
        put(&lifetime, "");
        put(&hook_pid, "");
        // The lock follows this hook across exec and is released when it exits.
        // Keep the bounded wait in that same process, with no extra pipe holder.
        format!(
            "exec {boundary} lock-and-sleep {lifetime} {pid}",
            boundary = f.world.boundary(),
            lifetime = sh(&lifetime),
            pid = sh(&hook_pid),
        )
    };
    f.hook(&f.a, "pre-push", &hook);
    f.write(&f.a, "local", "waiting\n");
    let started = Instant::now();
    let receipt = f.sync(&f.a);
    if f.oracle {
        has(&receipt, "timed out");
        has(&receipt, "credential");
    } else {
        unconfirmed_push_issue(&f, &receipt, &before);
        let pid: libc::pid_t = read(&hook_pid).trim().parse().unwrap();
        assert!(pid > 1);
        let lifetime = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lifetime)
            .unwrap();
        lifetime.try_lock().expect("push hook still running");
    }
    assert!(started.elapsed() >= Duration::from_secs(59));
    assert_eq!(f.remote(), before);
});

sync_case!(
    active unchanged_failed_native_apply_names_diagnostic_and_recovery,
    || {
        let f = Repo::new();
        let (mise, attempted) = failing_apply_fixture(&f);
        let incoming = f.publish("remote", "from peer\n");
        let receipt = f.sync_env(
            &f.a,
            &[
                ("DOTS_MISE_BIN", mise.to_str().unwrap()),
                ("FIXTURE_APPLY_FAILURE", "mise ERROR cannot apply fixture file"),
            ],
        );
        applied_failure(&f, &receipt, &attempted, "cannot apply fixture file");
        assert_eq!(f.tip(&f.a), incoming);
        assert_eq!(read(&f.a.join("remote")), "from peer\n");
    }
);

sync_case!(
    active unchanged_failed_native_save_names_diagnostic_and_recovery,
    || {
        let f = Repo::new();
        failed_history_save(&f, None, "mise ERROR cannot read fixture history");
    }
);

fn failed_history_save(f: &Repo, output: Option<&Path>, diagnostic: &str) -> [String; 2] {
    let inventory = tracked_fixture(f, true);
    let before = f.remote();
    let attempted = f.scratch.join("save-attempted");
    let saved = f.scratch.join("saved");
    let receipt = dots_zero(
        f.dots(&f.a, &["sync"])
            .env("FIXTURE_DOTFILE_PATHS", inventory)
            .env("FIXTURE_SAVE_ATTEMPTED", &attempted)
            .env("FIXTURE_SAVED", &saved)
            .env("FIXTURE_SAVE_OUTPUT", output.unwrap_or(Path::new("")))
            .env("FIXTURE_SAVE_FAILURE", diagnostic),
    );
    assert!(attempted.exists(), "{receipt}");
    assert!(!saved.exists(), "{receipt}");
    assert_eq!(f.remote(), before);
    assert_eq!(f.tip(&f.a), before);
    let reports = [receipt, f.replay(&f.a)];
    for text in &reports {
        let lower = text.to_ascii_lowercase();
        assert!(
            lower.lines().any(|line| {
                line.contains("sav")
                    && ["failed", "could not", "cannot", "unable to"]
                        .iter()
                        .any(|reason| line.contains(reason))
            }),
            "missing a clear save failure reason: {text}"
        );
        assert!(
            lower.contains("exit status")
                || lower.contains("exit code")
                || lower.contains("cannot read")
                || lower.contains("fixture diagnostic")
                || lower.contains("mise error"),
            "missing a safe reason for the save failure: {text}"
        );
        absent(&lower, "dots backup");
        assert!(
            lower.lines().any(|line| {
                line.contains("sync") && (line.contains("retry") || line.contains("rerun"))
            }),
            "checkpoint failure needs an actionable sync retry: {text}"
        );
        assert!(
            text.len() < 64 * 1024,
            "sync receipt rendered {} bytes",
            text.len()
        );
    }
    reports
}

sync_case!(active unchanged_long_native_failure_keeps_receipt_readable, || {
    let f = Repo::new();
    let failure = f.scratch.join("long-failure");
    script(
        &failure,
        "awk 'BEGIN { for (i=0; i<20; i++) printf \"fixture diagnostic %d %070000d\\n\", i, 0 }' >&2",
    );
    failed_history_save(&f, Some(&failure), "mise ERROR cannot read fixture history");
});

sync_case!(
    active new_native_history_failure_keeps_backend_secret_out_of_live_and_replay,
    || {
        let f = Repo::new();
        let marker = "SYNTHETIC_HISTORY_PASSWORD_7E341A";
        for report in failed_history_save(&f, None, &format!("mise ERROR {marker}")) {
            absent(&report, marker);
        }
    }
);

const FLAG_SCANNER: &str = r#"
report=''
opts=''
finding_status=1
while [ "$#" -gt 1 ]; do
    case "$1" in
        --report-path) report=$2; shift ;;
        --report-path=*) report=${1#*=} ;;
        --log-opts) opts=$2; shift ;;
        --log-opts=*) opts=${1#*=} ;;
        --exit-code) finding_status=$2; shift ;;
        --exit-code=*) finding_status=${1#*=} ;;
    esac
    shift
done
root=$1
[ -n "$report" ] || exit 2
case "$opts" in
    *' --') ;;
    *) opts="$opts --" ;;
esac
set -f
git -C "$root" log --format='commit %H' -p $opts |
awk '
/^commit / { commit = $2 }
/^\+\+\+ b\// { path = substr($0, 7); if (path == "fine") saw_fine = 1 }
/^@@ / { split($3, start, ","); line = substr(start[1], 2) + 0; next }
/^\+[^+]/ {
    if ($0 ~ /FIXTURE_SCAN_MATCH/) {
        if (found++) printf ","
        printf "{\"File\":\"%s\",\"StartLine\":%d,\"RuleID\":\"fixture-rule\",\"Commit\":\"%s\"}", path, line, commit
    }
    line++
}
/^ / { line++ }
BEGIN { printf "[" }
END {
    print "]"
    if (trace != "") {
        if (found) print "blocked" >> trace
        else if (saw_fine) print "clean fine" >> trace
    }
    exit (found ? finding_status : 0)
}
' trace="${FIXTURE_SCAN_TRACE:-}" finding_status="$finding_status" > "$report"
"#;

sync_case!(
    active unchanged_flagged_candidate_stays_unreferenced_and_safe_file_is_shared,
    || {
        let f = Repo::new();
        script(&f.tools.join("gitleaks"), FLAG_SCANNER);
        let scans = f.scratch.join("candidate-scans");
        let pushed = f.scratch.join("accepted-push");
        f.hook(
            &f.a,
            "pre-push",
            &format!(
                "awk '/^blocked$/ {{ blocked=1 }} /^clean fine$/ && blocked {{ clean=1 }} END {{ exit !clean }}' {}\ntouch {}",
                sh(&scans), sh(&pushed)
            ),
        );
        f.write(&f.a, "scanner-match", "safe line\nFIXTURE_SCAN_MATCH\n");
        f.write(&f.a, "fine", "harmless\n");
        let receipt = f.sync_env(&f.a, &[("FIXTURE_SCAN_TRACE", scans.to_str().unwrap())]);
        assert!(
            pushed.is_file(),
            "push did not observe a clean rescan after the finding"
        );
        has(&receipt, "scanner-match:2");
        absent(&receipt, "FIXTURE_SCAN_MATCH");
        assert_eq!(f.blob("dev", "fine"), "harmless");
        absent(&f.tree(), "scanner-match");
        assert_eq!(f.git(&f.a, &["status", "--porcelain"]), "?? scanner-match");
        assert_eq!(
            f.git(
                &f.a,
                &["log", "--all", "--format=%H", "--", "scanner-match"]
            ),
            ""
        );
    }
);

sync_case!(active unchanged_no_working_scanner_shares_nothing, || {
    let f = Repo::new();
    let before = f.remote();
    // A found-but-broken executable exercises the same fail-closed guarantee
    // even on a host whose normal fixture PATH also has gitleaks installed.
    script(&f.tools.join("gitleaks"), "exit 127");
    f.write(&f.a, "text", "edited\n");
    let receipt = f.sync(&f.a);
    has(&receipt, "gitleaks");
    assert_eq!(f.remote(), before);
    assert_eq!(f.tip(&f.a), before);
    assert_eq!(read(&f.a.join("text")), "edited\n");
    assert_eq!(f.git(&f.a, &["status", "--porcelain"]), "M text");
    script(&f.tools.join("gitleaks"), GITLEAKS_FIXTURE);
    has(&f.sync(&f.a), "pushed");
});

sync_case!(
    active unchanged_flagged_existing_commit_blocks_push_without_rewrite,
    || {
        let f = Repo::new();
        let before = f.remote();
        script(&f.tools.join("gitleaks"), FLAG_SCANNER);
        f.write(&f.a, "scanner-match", "FIXTURE_SCAN_MATCH\n");
        let committed = f.commit(&f.a, "Scanner match");
        let receipt = f.sync(&f.a);
        if f.oracle {
            has(&receipt, "scanner-match:1");
            has(&receipt, &committed[..12]);
        } else {
            has(&receipt, "gitleaks flagged unpublished history");
            has(&receipt, "remove or rotate");
        }
        absent(&receipt, "FIXTURE_SCAN_MATCH");
        assert_eq!(f.remote(), before);
        assert_eq!(f.tip(&f.a), committed);
    }
);

sync_case!(
    active unchanged_private_file_in_unpublished_history_blocks_push,
    || {
        for private in ["config.host-private.toml", "mise.laptop.lock"] {
            let f = Repo::new();
            let before = f.remote();
            f.write(&f.a, private, "private fixture contents\n");
            f.git(&f.a, &["add", "-f", private]);
            f.git(&f.a, &["commit", "-qm", "Private history"]);
            let forbidden = f.tip(&f.a);
            f.git(&f.a, &["rm", "-q", "--", private]);
            f.git(&f.a, &["commit", "-qm", "Remove the private file"]);
            assert!(!f.a.join(private).exists());
            let tip = f.tip(&f.a);
            let receipt = f.sync(&f.a);
            if f.oracle {
                has(&receipt, private);
                has(&receipt, &forbidden[..12]);
            } else {
                has(&receipt, "unpublished history tracked a machine-local file");
                has(&receipt, "review local history");
                absent(&receipt, "private fixture contents");
            }
            assert_eq!(f.remote(), before);
            assert_eq!(f.tip(&f.a), tip);
        }
    }
);

sync_case!(
    active unchanged_real_gitleaks_names_only_rule_path_and_line,
    || {
        let f = Repo::new();
        fs::remove_file(f.tools.join("gitleaks")).unwrap();
        let real = f
            .gitleaks_real
            .as_ref()
            .expect("real gitleaks on fixture PATH");
        symlink(real, f.tools.join("gitleaks")).unwrap();
        let marker = ["ghp", "R8tq3LwVn5Xk2Jc9Hy7Pd4Mb6Fz1Sa0Gu3Ee"].join("_");
        f.write(
            &f.a,
            "nested/token",
            format!("\ngithub_token = {marker:?}\n"),
        );
        let receipt = f.sync(&f.a);
        has(&receipt, "nested/token:2");
        has(&receipt, "github-pat");
        absent(&receipt, &marker);
        absent(&f.tree(), "nested/token");
    }
);

fn rejected_validation(f: &Repo, receipt: &str) {
    if f.oracle {
        has(receipt, "invalid");
    } else {
        has(receipt, "merged configuration failed validation");
        has(receipt, "nothing pushed or applied");
    }
}

sync_case!(
    active unchanged_invalid_merge_is_neither_pushed_nor_applied,
    || {
        let f = Repo::new();
        let incoming = f.publish("remote", "from peer\n");
        f.write(&f.a, "config.toml", format!("{}\n# INVALID\n", f.config));
        let receipt = f.sync(&f.a);
        rejected_validation(&f, &receipt);
        assert_eq!(f.remote(), incoming);
        assert!(!f.a.join("remote").exists());
        assert_ne!(f.tip(&f.a), incoming);
        has(&f.git(&f.a, &["show", "dev:config.toml"]), "INVALID");
        f.clean(&f.a);
        f.write(&f.a, "config.toml", format!("{}\n# fixed\n", f.config));
        has(&f.sync(&f.a), "pushed");
        assert_eq!(read(&f.a.join("remote")), "from peer\n");
        assert_eq!(f.remote(), f.tip(&f.a));
    }
);

sync_case!(
    active unchanged_toml_diagnostic_has_location_without_source_bytes,
    || {
        let f = Repo::new();
        let mise = f.mise_real.as_ref().expect("real mise on fixture PATH");
        f.publish("config.toml", format!("value = {PRIVATE_MARKER:?} stray\n"));
        let parser = f.tools.join("fixture-mise-parser");
        script(
            &parser,
            &format!(
                "case \" $* \" in *' config ls '*) exec {} \"$@\" ;; esac\nexec {} \"$@\"",
                sh(mise),
                sh(&f.mise),
            ),
        );
        for args in [&["--plain", "sync"][..], &["--verbose", "sync"][..]] {
            let receipt = dots_zero(f.dots(&f.a, args).env("DOTS_MISE_BIN", &parser));
            for text in [receipt, f.replay(&f.a)] {
                has(&text, "config.toml");
                has(&text, "line 1");
                absent(&text, PRIVATE_MARKER);
            }
        }
        assert_eq!(read(&f.a.join("config.toml")), f.config);
    }
);

sync_case!(
    active unchanged_edit_during_validation_survives_and_unrelated_work_shares,
    || {
        let f = Repo::new();
        f.publish("text", "incoming remote edit\n");
        f.write(&f.a, "local", "share this\n");
        let hook = f.validation_hook(&format!("echo late > {}", sh(&f.a.join("text"))));
        let receipt = f.sync_env(&f.a, &[("FIXTURE_VALIDATE", hook.to_str().unwrap())]);
        guarded_path_issue(&f, &receipt, "late edit", "text", "text");
        assert_eq!(read(&f.a.join("text")), "late\n");
        assert_eq!(f.blob("dev", "local"), "share this");
        merged_conflict(&f, &f.sync(&f.a));
        assert_eq!(read(&f.a.join("text")), "incoming remote edit\n");
        f.clean(&f.a);
    }
);

fn index_lock_inbound(linked: bool) {
    let f = Repo::new();
    let before = f.tip(&f.a);
    let root = if linked {
        f.git(&f.a, &["switch", "-q", "main"]);
        let root = f.scratch.join("linked");
        f.git(
            &f.a,
            &["worktree", "add", "-q", root.to_str().unwrap(), "dev"],
        );
        root
    } else {
        f.a.clone()
    };
    let primary_index = fs::read(f.a.join(".git/index")).unwrap();
    let incoming = f.publish("remote", "from peer\n");
    let lock = f.git_dir(&root).join("index.lock");
    put(&lock, "");
    let receipt = f.sync(&root);
    has(&receipt, "index");
    if f.oracle {
        has(&receipt, "locked");
    } else {
        has(&receipt, "could not reserve the full Git index");
    }
    assert!(lock.exists());
    assert!(!root.join("remote").exists());
    assert_eq!(f.tip(&root), before);
    fs::remove_file(lock).unwrap();
    f.sync(&root);
    assert_eq!(read(&root.join("remote")), "from peer\n");
    assert_eq!(f.tip(&root), incoming);
    assert_eq!(f.remote(), incoming);
    if linked {
        assert_eq!(fs::read(f.a.join(".git/index")).unwrap(), primary_index);
    }
    f.clean(&root);
}

sync_case!(
    active unchanged_checkout_respects_an_existing_git_index_lock,
    || {
        index_lock_inbound(false);
    }
);
sync_case!(
    active unchanged_linked_worktree_respects_its_own_index_lock,
    || {
        index_lock_inbound(true);
    }
);

fn recorded_checkout_issue(f: &Repo, root: &Path, receipt: &str, before: &str, after: &str) {
    for report in [receipt, &f.replay(root)] {
        has(report, "guarded checkout is pending");
        has(report, &format!("recorded revisions {before} to {after}"));
        has(report, "reconcile");
    }
}

fn ref_lock_recovery(linked: bool) {
    let f = Repo::new();
    let before = f.tip(&f.a);
    let root = if linked {
        f.git(&f.a, &["switch", "-q", "main"]);
        let root = f.scratch.join("linked");
        f.git(
            &f.a,
            &["worktree", "add", "-q", root.to_str().unwrap(), "dev"],
        );
        root
    } else {
        f.a.clone()
    };
    let primary_index = fs::read(f.a.join(".git/index")).unwrap();
    let incoming = f.publish("remote", "from peer\n");
    let lock = f.a.join(".git/refs/heads/dev.lock");
    put(&lock, "fixture owns this ref lock\n");
    let receipt = f.sync(&root);
    has(&receipt, "dev");
    if f.oracle {
        assert!(
            receipt.contains("could not update dev")
                || receipt.to_ascii_lowercase().contains("lock"),
            "sync did not report the held dev ref: {receipt}"
        );
    } else {
        recorded_checkout_issue(&f, &root, &receipt, &before, &incoming);
    }
    assert_eq!(read(&lock), "fixture owns this ref lock\n");
    assert_eq!(f.tip(&root), before);
    assert_eq!(f.remote(), incoming);
    // The oracle installs the incoming tree before its ref update fails.
    // A preflight ref-lock check may instead leave both live files and index intact.
    if root.join("remote").exists() {
        assert_eq!(read(&root.join("remote")), "from peer\n");
        assert_eq!(f.git(&root, &["show", ":remote"]), "from peer");
    } else {
        assert_eq!(f.git(&root, &["diff", "--cached", "--name-only"]), "");
    }
    fs::remove_file(lock).unwrap();
    f.sync(&root);
    assert_eq!(read(&root.join("remote")), "from peer\n");
    assert_eq!(f.tip(&root), incoming);
    assert_eq!(f.remote(), incoming);
    if linked {
        assert_eq!(fs::read(f.a.join(".git/index")).unwrap(), primary_index);
    }
    f.clean(&root);
}

sync_case!(
    active unchanged_ref_lock_interrupted_checkout_retries_accepted_commit,
    || {
        ref_lock_recovery(false);
    }
);
sync_case!(
    active unchanged_linked_worktree_interruption_retries_accepted_commit,
    || {
        ref_lock_recovery(true);
    }
);

fn guarded_path_issue(
    f: &Repo,
    receipt: &str,
    oracle_reason: &str,
    oracle_path: &str,
    incoming: &str,
) {
    if f.oracle {
        has(receipt, oracle_reason);
        has(receipt, oracle_path);
    } else {
        has(receipt, "obstructed or edited");
        has(receipt, incoming);
        has(receipt, "rerun dots sync");
    }
}

fn obstruction(shape: &str) {
    let f = Repo::new();
    let incoming = if shape == "symlink parent" {
        "collision/item"
    } else {
        "collision"
    };
    let blocked = if shape == "ignored child" {
        "collision/private"
    } else {
        "collision"
    };
    if shape == "ignored child" {
        f.publish("collision/public", "tracked child\n");
        f.sync(&f.a);
        fs::remove_dir_all(f.b.join("collision")).unwrap();
    }
    f.write(&f.a, ".git/info/exclude", format!("/{blocked}\n"));
    let path = f.a.join(blocked);
    match shape {
        "symlink parent" => symlink("missing", &path).unwrap(),
        "directory" => fs::create_dir(&path).unwrap(),
        _ => put(&path, "local only\n"),
    }
    f.publish(incoming, "incoming\n");
    f.write(&f.a, "unrelated", "share this\n");
    let receipt = f.sync(&f.a);
    guarded_path_issue(&f, &receipt, "blocked", blocked, incoming);
    assert_eq!(f.blob("dev", "unrelated"), "share this");
    match shape {
        "symlink parent" => assert_eq!(fs::read_link(&path).unwrap(), Path::new("missing")),
        "directory" => assert!(path.is_dir()),
        _ => assert_eq!(read(&path), "local only\n"),
    }
    fs::rename(&path, f.scratch.join("kept")).unwrap();
    f.sync(&f.a);
    assert_eq!(read(&f.a.join(incoming)), "incoming\n");
    f.clean(&f.a);
}

sync_case!(active unchanged_ignored_file_blocks_incoming_checkout, || {
    obstruction("file");
});
sync_case!(active unchanged_ignored_directory_blocks_incoming_checkout, || {
    obstruction("directory");
});
sync_case!(
    active unchanged_ignored_symlink_parent_blocks_incoming_checkout,
    || {
        obstruction("symlink parent");
    }
);
sync_case!(
    active unchanged_ignored_child_blocks_replacement_of_directory,
    || {
        obstruction("ignored child");
    }
);

fn rename_case(f: &Repo, from: &str, to: &str) -> String {
    f.git(&f.b, &["mv", from, "middle"]);
    f.git(&f.b, &["mv", "middle", to]);
    let tip = f.commit(&f.b, "Change spelling");
    f.git(&f.b, &["push", "-q", "origin", "HEAD:dev"]);
    tip
}

fn assert_spelling(f: &Repo, expected: &str) {
    let matching: Vec<_> = fs::read_dir(&f.a)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().eq_ignore_ascii_case("text"))
        .collect();
    assert_eq!(matching, [expected]);
    f.clean(&f.a);
}

sync_case!(active unchanged_case_only_rename_advances_cleanly, || {
    let f = Repo::new();
    let incoming = rename_case(&f, "text", "Text");
    f.sync(&f.a);
    assert_eq!(f.tip(&f.a), incoming);
    assert_spelling(&f, "Text");
});

sync_case!(
    active unchanged_interrupted_case_renames_keep_accepted_tip_and_contents,
    || {
        let f = Repo::new();
        let lock = f.git_dir(&f.a).join("refs/heads/dev.lock");
        for (from, to, body) in [
            ("text", "Text", TEXT),
            ("Text", "text", "incoming contents\n"),
        ] {
            let before = f.tip(&f.a);
            f.write(&f.b, from, body);
            let incoming = rename_case(&f, from, to);
            put(&lock, "");
            let receipt = f.sync(&f.a);
            if f.oracle {
                has(&receipt, "could not update dev");
            } else {
                recorded_checkout_issue(&f, &f.a, &receipt, &before, &incoming);
            }
            fs::remove_file(&lock).unwrap();
            f.sync(&f.a);
            assert_eq!(f.tip(&f.a), incoming);
            assert_eq!(f.remote(), incoming);
            assert_eq!(read(&f.a.join(to)), body);
            assert_spelling(&f, to);
        }
    }
);

fn reject_snapshot_with_lock(name: &str) {
    let f = Repo::new();
    let before = f.tip(&f.a);
    let lock = f.git_dir(&f.a).join(name);
    f.write(&f.a, "text", "edit remains on disk\n");
    put(&lock, "");
    let receipt = f.sync(&f.a);
    fs::remove_file(lock).unwrap();
    if f.oracle {
        has(&receipt, "not saved");
    } else {
        has(&receipt, "saved 0 locally");
        has(&receipt, "not pushed");
        if name == "index.lock" {
            has(&receipt, "could not reserve the full Git index");
        } else {
            has(&receipt, "guarded checkout is pending");
            absent(&receipt, "recorded revisions");
        }
    }
    assert_eq!(f.tip(&f.a), before);
    assert_eq!(f.remote(), before);
    assert_eq!(read(&f.a.join("text")), "edit remains on disk\n");
    assert_eq!(f.git(&f.a, &["status", "--porcelain"]), "M text");
}

sync_case!(active unchanged_local_snapshot_ref_lock_preserves_edits, || {
    reject_snapshot_with_lock("refs/heads/dev.lock");
});
sync_case!(active unchanged_local_snapshot_index_lock_preserves_edits, || {
    reject_snapshot_with_lock("index.lock");
});

fn detached_worktree(f: &Repo) -> PathBuf {
    let root = f.scratch.join("task");
    f.git(
        &f.a,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            root.to_str().unwrap(),
            "dev",
        ],
    );
    root
}

sync_case!(
    active unchanged_change_published_by_linked_worktree_is_merged,
    || {
        let f = Repo::new();
        let task = detached_worktree(&f);
        f.write(&task, "task-file", "task file\n");
        f.write(&task, "text", format!("{TEXT}eight\n"));
        let tip = f.commit(&task, "Task");
        f.git(&task, &["push", "-q", "origin", "HEAD:dev"]);
        f.write(&f.a, "text", TEXT.replace("one", "ONE"));
        has(&f.sync(&f.a), "pushed");
        f.check_ancestor(&tip, "dev");
        assert_eq!(read(&f.a.join("task-file")), "task file\n");
        assert_eq!(
            read(&f.a.join("text")),
            format!("{}eight\n", TEXT.replace("one", "ONE"))
        );
        f.clean(&f.a);
    }
);

fn external_dev_move(edit: bool) {
    let f = Repo::new();
    let task = detached_worktree(&f);
    f.write(&task, "moved", "dev moved\n");
    let tip = f.commit(&task, "Move dev");
    f.git(&task, &["update-ref", "refs/heads/dev", &tip]);
    if edit {
        f.write(&f.a, "text", "local edit\n");
    }
    f.sync(&f.a);
    f.check_ancestor(&tip, "dev");
    assert_eq!(read(&f.a.join("moved")), "dev moved\n");
    if edit {
        assert_eq!(f.blob("dev", "text"), "local edit");
    }
    f.clean(&f.a);
}

sync_case!(active unchanged_external_dev_move_preserves_local_edits, || {
    external_dev_move(true);
});
sync_case!(active unchanged_external_dev_move_updates_clean_worktree, || {
    external_dev_move(false);
});

sync_case!(
    active unchanged_stale_staged_index_preserves_current_commit_and_unstaged_edit,
    || {
        let f = Repo::new();
        f.write(&f.a, "text", "changed\n");
        f.sync(&f.a);
        let published = f.remote();
        f.git(&f.a, &["checkout", "HEAD~1", "--", "text"]);
        f.write(&f.a, "other", "unstaged edit\n");
        let receipt = f.sync(&f.a);
        if f.oracle {
            assert_eq!(f.blob("dev", "other"), "unstaged edit");
            assert_eq!(read(&f.a.join("text")), "changed\n");
            f.clean(&f.a);
        } else {
            has(&receipt, "could not verify the index against the current dev ref");
            has(&receipt, "rerun dots sync");
            assert_eq!(f.tip(&f.a), published);
            assert_eq!(f.remote(), published);
            assert_eq!(f.blob("dev", "text"), "changed");
            assert_eq!(f.git(&f.a, &["show", ":text"]), TEXT.trim_end());
            assert_eq!(read(&f.a.join("text")), TEXT);
            assert_eq!(read(&f.a.join("other")), "unstaged edit\n");
            assert_eq!(f.git(&f.a, &["status", "--porcelain"]), "M  text\n?? other");
        }
    }
);

fn startup_receipt(
    f: &Repo,
    receipt: &str,
    oracle_diagnostic: &str,
    diagnostic: &str,
    recovery: &str,
) {
    if f.oracle {
        assert_eq!(receipt.lines().count(), 1, "{receipt}");
        has(receipt, oracle_diagnostic);
    } else {
        assert_eq!(
            receipt
                .lines()
                .filter(|line| line.starts_with("dots sync:"))
                .count(),
            1,
            "{receipt}"
        );
        has(receipt, diagnostic);
        has(receipt, recovery);
    }
}

fn wrong_checkout(shape: &str, oracle_diagnostic: &str, diagnostic: &str) {
    let f = Repo::new();
    let before = f.remote();
    match shape {
        "detached" => {
            f.git(&f.a, &["switch", "-q", "--detach"]);
        }
        "main" => {
            f.git(&f.a, &["switch", "-q", "main"]);
            f.write(&f.a, "unpublished-main", "do not move local main\n");
            f.commit(&f.a, "Unpublished main change");
        }
        "jj" => fs::create_dir(f.a.join(".jj")).unwrap(),
        "legacy" => f.write(&f.a, ".git/dots-dev.json", "{}"),
        _ => unreachable!(),
    }
    let check = || {
        let receipt = f.sync(&f.a);
        startup_receipt(&f, &receipt, oracle_diagnostic, diagnostic, "sync guide");
        assert_eq!(f.remote(), before);
    };
    check();
    if shape == "legacy" && !f.oracle {
        // A pending checkout bypasses the first shape check so recovery can
        // recheck the repository under the sync lock.
        let pending = f.a.join(".git/dots-sync-pending");
        fs::create_dir(&pending).unwrap();
        check();
        assert!(pending.is_dir());
    }
}

sync_case!(active unchanged_detached_head_stops_cleanly, || {
    wrong_checkout("detached", "detached", "Git HEAD on a dev branch");
});
sync_case!(active unchanged_main_checkout_stops_cleanly, || {
    wrong_checkout("main", "main", "Git HEAD on a dev branch");
});

#[test]
fn new_sync_selects_dev_from_a_clean_main_clone() {
    let f = Repo::new();
    let dev = f.publish("from-dev", "published on dev\n");
    let clone = f.scratch.join("fresh");
    f.git(
        &f.scratch,
        &[
            "clone",
            "-q",
            "--branch",
            "main",
            &format!("file://{}", f.origin.display()),
            clone.to_str().unwrap(),
        ],
    );
    f.seed_identity(&clone);
    let main = f.dev_or_head(&clone);
    assert_ne!(main, dev);
    f.write(&clone, "untracked", "keep local work\n");
    let refusal = f.sync(&clone);
    has(&refusal, "Git HEAD on a dev branch");
    absent(&refusal, "Switched this checkout from main to dev");
    assert_eq!(f.dev_or_head(&clone), main);
    assert_eq!(read(&clone.join("untracked")), "keep local work\n");
    assert_eq!(f.remote(), dev);
    fs::remove_file(clone.join("untracked")).unwrap();

    f.git(&clone, &["branch", "dev", "main"]);
    has(&f.sync(&clone), "Git HEAD on a dev branch");
    assert_eq!(f.dev_or_head(&clone), main);
    assert_eq!(f.tip(&clone), main);
    f.git(&clone, &["branch", "-d", "dev"]);
    let receipt = f.sync(&clone);
    has(&receipt, "dev up to date");
    assert_eq!(
        receipt
            .matches("Switched this checkout from main to dev")
            .count(),
        1
    );
    has(&f.replay(&clone), "Switched this checkout from main to dev");
    absent(&f.sync(&clone), "Switched this checkout from main to dev");
    assert_eq!(f.git(&clone, &["branch", "--show-current"]), "dev");
    assert_eq!(f.tip(&clone), dev);
    assert_eq!(f.git(&clone, &["rev-parse", "main"]), main);
    assert_eq!(read(&clone.join("from-dev")), "published on dev\n");
    f.clean(&clone);
}

#[test]
fn new_sync_keeps_main_when_dev_removes_the_saved_profile() {
    let f = Repo::new();
    f.git(&f.a, &["switch", "main"]);
    f.git(&f.a, &["branch", "-d", "dev"]);
    let main = f.dev_or_head(&f.a);
    let invalid = f.publish(
        "config.toml",
        f.config.replace("laptop selected", "selected"),
    );
    f.git(&f.a, &["fetch", "origin"]);
    let local = "[env]\nSETUP_PROFILE = 'laptop'\nSETUP_MACHINE_ID = 'fixture'\n";
    f.write(&f.a, "config.local.toml", local);
    f.clean(&f.a);
    let receipt = dots_zero(
        f.dots(&f.a, &["--plain", "sync"])
            .env_remove("SETUP_PROFILE"),
    );
    assert_eq!(
        f.git(&f.a, &["branch", "--show-current"]),
        "main",
        "{receipt}"
    );
    assert_eq!(f.dev_or_head(&f.a), main);
    assert_eq!(read(&f.a.join("config.toml")), f.config);
    assert_eq!(read(&f.a.join("config.local.toml")), local);
    assert_eq!(f.remote(), invalid);
    f.clean(&f.a);
    has(&receipt, "configuration failed validation");
    absent(&receipt, "Switched this checkout from main to dev");
}

#[test]
fn new_sync_keeps_main_when_dev_names_another_origin() {
    let f = Repo::new();
    let other = f.world.bare_remote("other-source.git");
    f.git(&f.a, &["switch", "main"]);
    f.git(&f.a, &["branch", "-d", "dev"]);
    let main = f.dev_or_head(&f.a);
    let dev = f.publish(
        "config.toml",
        f.config.replace(
            &format!("file://{}", f.origin.display()),
            &format!("file://{}", other.display()),
        ),
    );
    f.git(&f.a, &["fetch", "origin"]);
    let transport = deny_transport(&f);
    let receipt = f.sync(&f.a);
    has(&receipt, "origin is missing, unsafe, or differs");
    absent(&receipt, "Switched this checkout from main to dev");
    assert_eq!(f.git(&f.a, &["branch", "--show-current"]), "main");
    assert_eq!(f.dev_or_head(&f.a), main);
    assert_eq!(read(&f.a.join("config.toml")), f.config);
    assert!(!transport.exists());
    assert_eq!(f.remote(), dev);
    assert!(f.git(&other, &["for-each-ref"]).is_empty());
}

#[test]
fn new_sync_creates_dev_when_origin_only_has_main() {
    let f = Repo::new();
    f.git(&f.origin, &["update-ref", "-d", "refs/heads/dev"]);
    f.git(&f.a, &["switch", "main"]);
    f.git(&f.a, &["branch", "-d", "dev"]);
    f.git(&f.a, &["update-ref", "-d", "refs/remotes/origin/dev"]);
    let main = f.dev_or_head(&f.a);
    let concurrent = f.git(
        &f.a,
        &[
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            "HEAD",
            "-m",
            "Concurrent main commit",
        ],
    );
    let marker = sh(&f.scratch.join("main-changed-during-validation"));
    let hook = f.validation_hook(&format!(
        "if [ ! -e {marker} ]; then\n  git -C {} update-ref refs/heads/main {concurrent} {main}\n  touch {marker}\nfi",
        sh(&f.a)
    ));
    let stopped = f.sync_env(&f.a, &[("FIXTURE_VALIDATE", hook.to_str().unwrap())]);
    has(&stopped, "Source checkout changed before switching to dev");
    absent(&stopped, "Switched this checkout from main to dev");
    assert_eq!(f.git(&f.a, &["branch", "--show-current"]), "main");
    assert_eq!(f.dev_or_head(&f.a), concurrent);
    assert_eq!(f.git(&f.a, &["branch", "--list", "dev"]), "");
    f.clean(&f.a);
    f.git(&f.a, &["update-ref", "refs/heads/main", &main, &concurrent]);
    let receipt = f.sync(&f.a);
    has(&receipt, "pushed dev");
    assert_eq!(
        receipt
            .matches("Switched this checkout from main to dev")
            .count(),
        1
    );
    assert_eq!(f.git(&f.a, &["branch", "--show-current"]), "dev");
    assert_eq!(f.tip(&f.a), f.git(&f.origin, &["rev-parse", "main"]));
    assert_eq!(f.remote(), f.tip(&f.a));
}
sync_case!(active unchanged_jj_checkout_stops_cleanly, || {
    wrong_checkout("jj", ".jj", "JJ workspace");
});
sync_case!(active unchanged_pre_port_sync_state_stops_cleanly, || {
    wrong_checkout(
        "legacy",
        "legacy",
        "Sync found source state from dots 0.4.0 or earlier in .git/dots-dev.json",
    );
});
sync_case!(active unchanged_wrong_origin_stops_cleanly, || {
    let f = Repo::new();
    let local = f.tip(&f.a);
    let mismatched = f.world.bare_remote("mismatched-origin.git");
    let refs = |root: &Path| f.git(root, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let expected_refs = refs(&f.origin);
    let mismatched_refs = refs(&mismatched);
    let url = format!("file://{}", mismatched.display());
    f.git(&f.a, &["remote", "set-url", "origin", &url]);
    assert_eq!(f.git(&f.a, &["remote", "get-url", "origin"]), url);
    assert_eq!(f.git(&f.a, &["remote", "get-url", "--push", "origin"]), url);
    let attempted = deny_transport(&f);
    let receipt = f.sync(&f.a);
    startup_receipt(&f, &receipt, "origin", "unsafe", "rerun dots sync");
    assert!(!attempted.exists(), "sync tried Git transport: {receipt}");
    assert_eq!(refs(&f.origin), expected_refs);
    assert_eq!(refs(&mismatched), mismatched_refs);
    assert_eq!(f.tip(&f.a), local);
});

sync_case!(
    active unchanged_snapshot_error_exits_zero_and_keeps_worktree_bytes,
    || {
        let f = Repo::new();
        let before = f.tip(&f.a);
        f.git(&f.a, &["config", "--unset", "user.name"]);
        f.git(&f.a, &["config", "--unset", "user.email"]);
        put(
            &f.scratch.join("no-identity.gitconfig"),
            "[user]\nuseConfigOnly = true\n",
        );
        f.git(&f.a, &["config", "user.useConfigOnly", "true"]);
        f.write(&f.a, "text", "needs a commit\n");
        let receipt = dots_zero(
            f.dots(&f.a, &["--plain", "sync"])
                .env("GIT_CONFIG_GLOBAL", f.scratch.join("no-identity.gitconfig"))
                .env_remove("EMAIL")
                .env_remove("GIT_AUTHOR_NAME")
                .env_remove("GIT_AUTHOR_EMAIL")
                .env_remove("GIT_COMMITTER_NAME")
                .env_remove("GIT_COMMITTER_EMAIL"),
        );
        has(&receipt, "identity");
        if !f.oracle {
            has(&receipt, "Could not snapshot local edits: Author identity unknown");
            has(&receipt, "git config --global user.name");
            has(&receipt, "git config --global user.email");
        }
        assert_eq!(f.tip(&f.a), before);
        assert_eq!(f.remote(), before);
        assert_eq!(read(&f.a.join("text")), "needs a commit\n");
    }
);

#[test]
fn new_sync_supplies_missing_commit_identity_without_writing_config() {
    let f = Repo::new();
    let global = f.world.home().join(".gitconfig");
    fs::remove_file(&global).unwrap();
    f.git(&f.a, &["config", "--unset", "user.name"]);
    f.git(&f.a, &["config", "--unset", "user.email"]);
    let shim = f.tools.join("git");
    let real = fs::read_link(&shim).unwrap();
    fs::remove_file(&shim).unwrap();
    script(
        &shim,
        &format!(
            r#"
identity() {{
    if {{ [ -z "$2" ] && [ "${{FIXTURE_CONFIGURED_NAME:-}}" != 1 ]; }} ||
       {{ [ -z "$3" ] && [ -z "${{EMAIL:-}}" ] && [ "${{FIXTURE_CONFIGURED_EMAIL:-}}" != 1 ]; }}; then
        printf '%s identity unknown\nfatal: fixture cannot infer identity\n' "$1" >&2
        exit 128
    fi
}}
case " $* " in
  *" write-tree "*)
    if [ "${{FIXTURE_SNAPSHOT_ERROR:-}}" = 1 ]; then
      case "${{GIT_INDEX_FILE:-}}" in
        */snapshot.index) printf 'fixture index error\nsecond diagnostic line\n' >&2; exit 1 ;;
      esac
    fi ;;
  *" var GIT_AUTHOR_IDENT "*)
    case " $* " in *" -c commit.gpgSign=false "*) ;; *) exit 98 ;; esac
    identity Author "${{GIT_AUTHOR_NAME:-}}" "${{GIT_AUTHOR_EMAIL:-}}" ;;
  *" var GIT_COMMITTER_IDENT "*)
    case " $* " in *" -c commit.gpgSign=false "*) ;; *) exit 98 ;; esac
    identity Committer "${{GIT_COMMITTER_NAME:-}}" "${{GIT_COMMITTER_EMAIL:-}}" ;;
  *" commit-tree "*)
    identity Author "${{GIT_AUTHOR_NAME:-}}" "${{GIT_AUTHOR_EMAIL:-}}"
    identity Committer "${{GIT_COMMITTER_NAME:-}}" "${{GIT_COMMITTER_EMAIL:-}}" ;;
esac
exec {} "$@"
"#,
            sh(&real)
        ),
    );
    let sync = |fail, configured_name, configured_email, email: Option<&str>| {
        let mut sync = f.dots(&f.a, &["--plain", "sync"]);
        sync.env("FIXTURE_SNAPSHOT_ERROR", fail)
            .env("FIXTURE_CONFIGURED_NAME", configured_name)
            .env("FIXTURE_CONFIGURED_EMAIL", configured_email)
            .env_remove("EMAIL")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL");
        if let Some(email) = email {
            sync.env("EMAIL", email);
        }
        dots_zero(&mut sync)
    };
    let before = f.remote();
    f.write(&f.a, "text", "new public bytes\n");
    let failed = sync("1", "0", "0", None);
    has(
        &failed,
        "Could not snapshot local edits: fixture index error",
    );
    absent(&failed, "second diagnostic line");
    assert_eq!(f.remote(), before);
    assert_eq!(f.tip(&f.a), before);

    for (content, setting, email, expected) in [
        ("no identity", None, None, "dots <dots@localhost>"),
        (
            "name only",
            Some(("user.name", "Fixture Name")),
            None,
            "Fixture Name <dots@localhost>",
        ),
        (
            "configured email",
            Some(("user.email", "config@example.invalid")),
            None,
            "dots <config@example.invalid>",
        ),
        (
            "EMAIL only",
            None,
            Some("env@example.invalid"),
            "dots <env@example.invalid>",
        ),
    ] {
        if let Some((key, value)) = setting {
            f.git(&f.a, &["config", key, value]);
        }
        f.write(&f.a, "text", format!("{content}\n"));
        let config = fs::read(f.a.join(".git/config")).unwrap();
        has(
            &sync(
                "0",
                if setting.is_some_and(|(key, _)| key == "user.name") {
                    "1"
                } else {
                    "0"
                },
                if setting.is_some_and(|(key, _)| key == "user.email") {
                    "1"
                } else {
                    "0"
                },
                email,
            ),
            "pushed dev",
        );
        assert_eq!(fs::read(f.a.join(".git/config")).unwrap(), config);
        assert!(!global.exists());
        assert_eq!(
            f.git(
                &f.origin,
                &["log", "-1", "--format=%an <%ae>%n%cn <%ce>", "dev"]
            ),
            format!("{expected}\n{expected}"),
            "{content}"
        );
        assert_eq!(f.blob("dev", "text"), content);
        if let Some((key, _)) = setting {
            f.git(&f.a, &["config", "--unset", key]);
        }
    }
}

struct RunningSync {
    child: Option<std::process::Child>,
    pid: u32,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl RunningSync {
    fn start(command: &mut FixtureCommand<'_>, log: &Path) -> Self {
        let stdout = log.with_extension("stdout");
        let stderr = log.with_extension("stderr");
        // SAFETY: setpgid is fork-safe and keeps the checked fixture paths.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        Self {
            pid: child.id(),
            child: Some(child),
            stdout,
            stderr,
        }
    }

    fn signal(&self, number: libc::c_int, group: bool) {
        let pid = self.pid as libc::pid_t;
        let target = if group { -pid } else { pid };
        // The fixture owns this child and created its process group.
        assert_eq!(
            unsafe { libc::kill(target, number) },
            0,
            "signal {number} to fixture process {target}: {}",
            std::io::Error::last_os_error()
        );
    }

    fn kill_lock_holder(&self, parent: &Path, release: &Path) {
        let pid = read(parent).trim().parse::<libc::pid_t>().unwrap();
        assert!(pid > 1);
        if pid as u32 == self.pid {
            self.signal(libc::SIGKILL, true);
        } else {
            // The packaged oracle places its lock holder behind the Go
            // frontend. The fixture's validation child identifies that PID.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        }
        put(release, "");
    }

    fn exited(&self) -> bool {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: The fixture owns this child. WNOWAIT reserves its PID
            // until any group cleanup has finished.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return unsafe { info.si_pid() } != 0;
            }
            let error = std::io::Error::last_os_error();
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted, "{error}");
        }
    }

    fn wait_until_started(&self, marker: &Path) {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(45);
        while !marker.exists() {
            assert!(
                !self.exited() && Instant::now() < deadline,
                "sync did not reach its fixture child\nstdout: {}\nstderr: {}",
                read(&self.stdout),
                read(&self.stderr),
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn finish(mut self) -> Output {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.exited() {
            assert!(Instant::now() < deadline, "fixture sync did not finish");
            std::thread::sleep(Duration::from_millis(25));
        }
        let status = self.child.take().unwrap().wait().unwrap();
        Output {
            status,
            stdout: fs::read(&self.stdout).unwrap(),
            stderr: fs::read(&self.stderr).unwrap(),
        }
    }
}

impl Drop for RunningSync {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            // The unreaped leader still reserves the fixture's group ID.
            unsafe { libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn block_validation(f: &Repo) -> (PathBuf, PathBuf, PathBuf) {
    let entered = f.scratch.join("entered-validation");
    let release = f.scratch.join("release-validation");
    let hook = f.validation_hook(&format!(
        "touch {entered}\nfor n in $(seq 1 60); do\n  if [ -e {release} ]; then exit 0; fi\n  sleep 1\ndone\nexit 97",
        entered = sh(&entered), release = sh(&release),
    ));
    (hook, entered, release)
}

sync_case!(
    active unchanged_concurrent_sync_skips_and_dead_holder_is_reclaimed,
    || {
        let f = Repo::new();
        let before = f.remote();
        let (hook, entered, release) = block_validation(&f);
        let parent = f.scratch.join("lock-holder-pid");
        f.write(&f.a, "local", "waiting\n");
        let holder = RunningSync::start(
            f.dots(&f.a, &["--plain", "sync"])
                .env("FIXTURE_VALIDATE", &hook)
                .env("FIXTURE_VALIDATION_PARENT", &parent),
            &f.scratch.join("holder"),
        );
        holder.wait_until_started(&entered);
        let receipt = f.sync(&f.a);
        has(&receipt, "another");
        has(&receipt, "running");
        assert_eq!(f.remote(), before);
        holder.kill_lock_holder(&parent, &release);
        assert!(!holder.finish().status.success());
        has(&f.sync(&f.a), "pushed");
        assert_eq!(f.blob("dev", "local"), "waiting");
    }
);

#[test]
fn new_standalone_sync_cancellation_records_replayable_stopped_receipt() {
    let f = Repo::new();
    let previous = f.tip(&f.a);
    let reviewed = f.reviewed("incoming", "from reviewed main\n");
    let entered = f.scratch.join("cancel-child-entered");
    let cleaned = f.scratch.join("cancel-child-cleaned");
    f.hook(&f.a, "pre-push", &format!(
        "cat >/dev/null\ntrap 'touch \"$FIXTURE_CLEANED\"; exit 42' INT TERM HUP\ntouch {entered}\nfor n in $(seq 1 60); do sleep 1; done\nexit 97",
        entered = sh(&entered),
    ));
    let run = RunningSync::start(
        f.dots(&f.a, &["--plain", "sync"])
            .env("FIXTURE_CLEANED", &cleaned),
        &f.scratch.join("cancelled"),
    );
    run.wait_until_started(&entered);
    run.signal(libc::SIGINT, false);
    let output = run.finish();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let receipt = stdout(&output);
    assert_eq!(
        receipt
            .lines()
            .filter(|line| line.starts_with("dots sync:"))
            .count(),
        1,
        "{receipt}"
    );
    has(receipt, "stopped");
    assert!(
        cleaned.exists(),
        "sync exited before its fixture child finished cleanup"
    );
    has(&f.replay(&f.a), "stopped");
    assert_eq!(f.tip(&f.a), previous);
    assert_eq!(f.remote(), previous);
    assert_eq!(f.git(&f.origin, &["rev-parse", "main"]), reviewed);
    assert!(!f.a.join("incoming").exists());
}

const RETIRED_DECLARATION: &str = r#"
[dotfiles.fixture]
source = "templates/fixture.tera"
mode = "template"
variants = [{ os = "macos", target = "~/.fixture-rendered" }, { os = "linux", target = "~/.fixture-rendered" }]
"#;
const KEEP_LOCAL: &str = "# This machine\n[env]\nFIXTURE_PROFILE = \"laptop\"\n";

fn setup_retirement(f: &Repo, listed: bool, local_file: &str, local: &str) -> String {
    let source = "templates/fixture.tera";
    f.publish(source, "{{ 'fixture' }}\n");
    f.sync(&f.a);
    f.write(&f.a, local_file, local);
    chmod(&f.a.join(local_file), 0o600);
    fs::remove_file(f.b.join(source)).unwrap();
    if listed {
        f.write(
            &f.b,
            "dots/retired-declarations",
            format!("dotfiles.fixture\t{source}\n"),
        );
    }
    let incoming = f.commit(&f.b, "Remove template");
    f.git(&f.b, &["push", "-q", "origin", "HEAD:dev"]);
    incoming
}

fn retirement_backup(f: &Repo, receipt: &str, local_path: &str, expected: &str) {
    let root = &f.a;
    let candidates: Vec<_> = if f.oracle {
        receipt
            .split_whitespace()
            .filter(|part| part.contains(local_path))
            .map(|part| {
                part.trim_matches(['(', ')', ',', ';', '`', '"'])
                    .trim_end_matches('.')
            })
            .collect()
    } else {
        receipt
            .lines()
            .filter_map(|line| line.strip_prefix("Recovery copy at "))
            .collect()
    };
    let saved = candidates
        .into_iter()
        .map(|part| root.join(part))
        .filter(|path| path != &root.join(local_path))
        .filter_map(|path| fixture_file(root.parent().unwrap(), &path))
        .filter(|entry| entry.file.metadata().unwrap().mode() & 0o777 == 0o600)
        .any(|entry| std::io::read_to_string(entry.file).is_ok_and(|data| data == expected));
    assert!(
        saved,
        "receipt did not name the private retirement backup: {receipt}"
    );
}

sync_case!(
    active unchanged_matching_local_declaration_is_retired_with_private_backup,
    || {
        let f = Repo::new();
        const LOCAL: &str = "config.local.toml";
        let local = format!("{KEEP_LOCAL}{RETIRED_DECLARATION}");
        let incoming = setup_retirement(&f, true, LOCAL, &local);
        f.write(&f.a, "config.host-laptop.toml", "[env]\nKEEP = \"1\"\n");
        let receipt = f.sync(&f.a);
        has(&receipt, "retired");
        let local_config = fixture_file(&f.scratch, &f.a.join(LOCAL))
            .expect("rewritten local declaration must be a regular fixture file");
        assert_eq!(local_config.file.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::io::read_to_string(local_config.file).unwrap(),
            KEEP_LOCAL
        );
        assert_eq!(
            read(&f.a.join("config.host-laptop.toml")),
            "[env]\nKEEP = \"1\"\n"
        );
        assert_eq!(f.tip(&f.a), incoming);
        assert!(!f.a.join("templates/fixture.tera").exists());
        retirement_backup(&f, &receipt, LOCAL, &local);
        has(
            &f.sync(&f.a),
            if f.oracle { "nothing to push" } else { "dev up to date" },
        );
    }
);

sync_case!(
    active unchanged_unlisted_stale_declaration_fails_validation,
    || {
        let f = Repo::new();
        const LOCAL: &str = "config.local.toml";
        let local = format!("{KEEP_LOCAL}{RETIRED_DECLARATION}");
        setup_retirement(&f, false, LOCAL, &local);
        let before = fs::metadata(f.a.join(LOCAL)).unwrap().modified().unwrap();
        let receipt = f.sync(&f.a);
        rejected_validation(&f, &receipt);
        assert_eq!(read(&f.a.join(LOCAL)), local);
        assert_eq!(
            fs::metadata(f.a.join(LOCAL)).unwrap().modified().unwrap(),
            before
        );
        assert!(f.a.join("templates/fixture.tera").exists());
    }
);

sync_case!(
    active unchanged_selected_local_source_different_from_retirement_row_is_kept,
    || {
        let f = Repo::new();
        const LOCAL: &str = "config.local.toml";
        let local = format!(
            "{KEEP_LOCAL}{}",
            RETIRED_DECLARATION.replace("templates/fixture.tera", "text")
        );
        let incoming = setup_retirement(&f, true, LOCAL, &local);
        f.sync(&f.a);
        assert_eq!(f.tip(&f.a), incoming);
        assert_eq!(read(&f.a.join(LOCAL)), local);
    }
);

sync_case!(
    active unchanged_invalid_incoming_configuration_cannot_retire_local_declaration,
    || {
        let f = Repo::new();
        const LOCAL: &str = "config.local.toml";
        let local = format!("{KEEP_LOCAL}{RETIRED_DECLARATION}");
        let incoming = setup_retirement(&f, true, LOCAL, &local);
        f.write(&f.a, "config.toml", format!("{}# INVALID\n", f.config));
        let receipt = f.sync(&f.a);
        rejected_validation(&f, &receipt);
        assert_eq!(read(&f.a.join(LOCAL)), local);
        assert_eq!(f.remote(), incoming);
        assert!(f.a.join("templates/fixture.tera").exists());
    }
);

sync_case!(active unchanged_retirement_waits_until_files_advance, || {
    let f = Repo::new();
    const LOCAL: &str = "config.host-laptop.toml";
    let local = format!("{KEEP_LOCAL}{RETIRED_DECLARATION}");
    let incoming = setup_retirement(&f, true, LOCAL, &local);
    let lock = f.git_dir(&f.a).join("index.lock");
    put(&lock, "");
    has(
        &f.sync(&f.a),
        if f.oracle { "locked" } else { "could not reserve the full Git index" },
    );
    assert_eq!(read(&f.a.join(LOCAL)), local);
    assert!(f.a.join("templates/fixture.tera").exists());
    fs::remove_file(lock).unwrap();
    has(&f.sync(&f.a), "retired");
    assert_eq!(read(&f.a.join(LOCAL)), KEEP_LOCAL);
    assert_eq!(f.tip(&f.a), incoming);
});

const RETIRE_LATE_EDIT: &str = "[[dotfiles.fixture.variants]]\nos = \"linux\"\n";

sync_case!(
    active unchanged_retirement_preserves_local_file_edited_during_validation,
    || {
        let f = Repo::new();
        const LOCAL: &str = "config.local.toml";
        let local = format!("{KEEP_LOCAL}{RETIRED_DECLARATION}");
        setup_retirement(&f, true, LOCAL, &local);
        let edited = f.scratch.join("local-edit-during-validation");
        let marker = sh(&edited);
        let hook = f.validation_hook(&format!(
            "if [ -e {marker} ]; then exit 0; fi\ncat >> {} <<'LATE'\n{RETIRE_LATE_EDIT}LATE\n: > {marker}",
            sh(&f.a.join(LOCAL))
        ));
        let receipt = f.sync_env(&f.a, &[("FIXTURE_VALIDATE", hook.to_str().unwrap())]);
        assert!(edited.exists(), "fixture edit did not run: {receipt}");
        if f.oracle {
            has(&receipt, "config.local.toml");
            has(&receipt, "changed");
        } else {
            has(&receipt, "not pushed");
            has(&receipt, "rerun dots sync");
        }
        assert_eq!(read(&f.a.join(LOCAL)), format!("{local}{RETIRE_LATE_EDIT}"));
        assert!(f.a.join("templates/fixture.tera").exists());
        let retry = f.sync(&f.a);
        if f.oracle {
            has(&retry, "invalid");
        } else {
            has(&retry, "profile and machine selection could not be read");
            has(&retry, "not pushed");
            assert_eq!(read(&f.a.join(LOCAL)), format!("{local}{RETIRE_LATE_EDIT}"));
            assert!(f.a.join("templates/fixture.tera").exists());
        }
        f.write(&f.a, LOCAL, &local);
        has(&f.sync(&f.a), "retired");
        assert_eq!(read(&f.a.join(LOCAL)), KEEP_LOCAL);
    }
);

sync_case!(
    active unchanged_retirement_reports_local_file_edited_as_dev_advances,
    || {
        let f = Repo::new();
        const LOCAL: &str = "config.local.toml";
        let local = format!("{KEEP_LOCAL}{RETIRED_DECLARATION}");
        let incoming = setup_retirement(&f, true, LOCAL, &local);
        f.hook(&f.a, "reference-transaction", &format!(
        "if [ \"$1\" = committed ] && grep -q ' refs/heads/dev$'; then\ncat >> {} <<'LATE'\n{RETIRE_LATE_EDIT}LATE\nfi",
        sh(&f.a.join(LOCAL)),
    ));
        let receipt = f.sync(&f.a);
        if f.oracle {
            has(&receipt, "config.local.toml");
            has(&receipt, "changed");
        } else {
            has(&receipt, "machine-local files changed after validation");
            has(&receipt, "dotfiles were kept");
        }
        assert_eq!(read(&f.a.join(LOCAL)), format!("{local}{RETIRE_LATE_EDIT}"));
        assert_eq!(f.tip(&f.a), incoming);
        assert!(!f.a.join("templates/fixture.tera").exists());
    }
);

struct Machine<'a> {
    repo: &'a Repo,
    root: &'a Path,
    home: PathBuf,
    state: PathBuf,
    mise: PathBuf,
    hooks: bool,
}

impl<'a> Machine<'a> {
    fn new(repo: &'a Repo, root: &'a Path, name: &str, hooks: bool) -> Self {
        let mise = repo.mise_real.clone().expect("real mise on fixture PATH");
        let state = repo.scratch.join(name);
        let home = state.join("home");
        for directory in [&home, &state.join("system")] {
            fs::create_dir_all(directory).unwrap();
        }
        Self {
            repo,
            root,
            home,
            state,
            mise,
            hooks,
        }
    }

    fn isolate(&self, command: &mut FixtureCommand<'_>) {
        command
            .env("PATH", self.repo.path())
            .env("TMPDIR", self.repo.scratch.join("tmp"))
            .env("MISE_OFFLINE", "1")
            .env("HOME", &self.home)
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("MISE_CONFIG_DIR", self.root)
            .env("MISE_DATA_DIR", self.state.join("data"))
            .env("MISE_STATE_DIR", self.state.join("state"))
            .env("MISE_CACHE_DIR", self.state.join("cache"))
            .env("MISE_SYSTEM_CONFIG_DIR", self.state.join("system"))
            .env("MISE_TRUSTED_CONFIG_PATHS", self.root)
            .env("MISE_CEILING_PATHS", &self.repo.scratch)
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_AUTO_UPDATE", "0")
            .env("MISE_NO_HOOKS", if self.hooks { "0" } else { "1" });
    }

    fn dots(&self, args: &[&str]) -> FixtureCommand<'_> {
        let mut command = self.repo.dots(self.root, args);
        self.isolate(&mut command);
        command.env("DOTS_MISE_BIN", &self.mise);
        command
    }

    fn sync(&self) -> String {
        dots_zero(&mut self.dots(&["--verbose", "sync"]))
    }

    fn replay(&self) -> String {
        dots_zero(&mut self.dots(&["--plain", "sync", "--last"]))
    }

    fn deploy(&self) {
        let mut command = self.repo.world.tool(&self.mise);
        command.current_dir(self.root);
        self.isolate(&mut command);
        success(
            command
                .arg("-C")
                .arg(self.root)
                .args(["bootstrap", "dotfiles", "apply", "--yes"]),
        );
    }

    fn source(&self, path: &str, value: impl AsRef<[u8]>) {
        self.repo.write(self.root, path, value);
    }

    fn target(&self, path: &str, value: impl AsRef<[u8]>) {
        put(&self.home.join(path), value);
    }
}

const COPY_DECLARATION: &str = r#""~/.copied" = { source = "copy.txt", mode = "copy" }"#;
const TEMPLATE_DECLARATION: &str =
    r#""~/.rendered" = { source = "template.tera", mode = "template" }"#;

sync_case!(active new_shared_sync_saves_encrypted_history_before_push, || {
    let f = Repo::new();
    let machine = Machine::new(&f, &f.a, "machine-a", false);
    let key = machine.home.join("fixture-age-key");
    let keygen = || {
        let mut command = f.world.tool(f.tools.join("age-keygen"));
        machine.isolate(&mut command);
        command
    };
    success(keygen().arg("-o").arg(&key));
    let recipient = success(keygen().arg("-y").arg(&key));

    let history_remote = f.world.bare_remote("history.git");
    let history_ref = "HEAD";
    let store = machine.state.join("state/history/repo.git");
    let checkpoint = f.scratch.join("checkpoint-before-push");
    assert!(!store.exists());
    f.hook(
        &f.a,
        "pre-push",
        &format!(
            "git --git-dir {store} rev-parse --verify {history_ref} > {checkpoint}",
            store = sh(&store),
            checkpoint = sh(&checkpoint),
        ),
    );
    machine.source(
        "config.toml",
        format!(
            "{}\n[dotfiles]\n\"~/.fixture-state\" = {{ mode = \"track\", encrypt = true }}\n",
            f.config
        ),
    );
    machine.source(
        "config.local.toml",
        format!(
            "[settings.history]\nsync = \"manual\"\n\n[settings.age]\nidentity_files = [{key:?}]\n\n[history.origin]\nurl = {:?}\nbranch = \"host-fixture\"\n\n[history.encryption]\nrecipients = [{recipient:?}]\n",
            format!("file://{}", history_remote.display()),
        ),
    );
    let content = "private fixture state before any source push";
    machine.target(".fixture-state", content);
    machine.source("public", "public fixture edit\n");
    let receipt = machine.sync();
    assert!(checkpoint.exists(), "{receipt}");
    let git = |args: &[&str]| finish(f.git_command(&f.a).arg("--git-dir").arg(&store).args(args));
    let revision = git(&["rev-parse", "--verify", history_ref]);
    assert!(revision.status.success(), "{revision:?}");
    assert_eq!(read(&checkpoint), stdout(&revision));
    let tree = git(&["ls-tree", "-r", "--name-only", history_ref]);
    assert!(tree.status.success(), "{tree:?}");
    let path = stdout(&tree)
        .lines()
        .find(|path| path.ends_with("/.fixture-state") || *path == ".fixture-state")
        .expect("native checkpoint contains the tracked file");
    let envelope = git(&["show", &format!("{history_ref}:{path}")]);
    assert!(envelope.status.success());
    assert!(envelope.stdout.starts_with(b"mise-encrypted-file-v1\n"));
    let age_header = b"age-encryption.org/v1\n";
    let encrypted = envelope
        .stdout
        .windows(age_header.len())
        .position(|window| window == age_header)
        .expect("native checkpoint contains age ciphertext");
    let payload = f.scratch.join("checkpoint.age");
    put(&payload, &envelope.stdout[encrypted..]);
    let mut age = f.world.tool(f.tools.join("age"));
    machine.isolate(&mut age);
    let decrypted = finish(age.args(["--decrypt", "-i"]).arg(&key).arg(payload));
    assert!(decrypted.status.success());
    machine.target(".fixture-state", "different private fixture state");
    let mut rollback = f.world.tool(&machine.mise);
    machine.isolate(&mut rollback);
    let commit = format!("commit:{}", stdout(&revision).trim());
    success(
        rollback
            .current_dir(&f.a)
            .args(["bootstrap", "dotfiles", "rollback", "--to", &commit])
            .args(["~/.fixture-state", "--yes"]),
    );
    assert_eq!(read(&machine.home.join(".fixture-state")), content);
    assert_eq!(mode(&machine.home.join(".fixture-state")), 0o600);
    assert_eq!(f.git(&history_remote, &["for-each-ref"]), "");
    assert_eq!(f.remote(), f.tip(&f.a));
    assert_eq!(f.blob("dev", "public"), "public fixture edit");
    let public =
        finish(
            f.git_command(&f.origin)
                .args(["cat-file", "--batch-all-objects", "--batch"]),
        );
    assert!(public.status.success());
    assert!(
        !public
            .stdout
            .windows(content.len())
            .any(|bytes| bytes == content.as_bytes()),
        "private tracked payload entered the public source repository"
    );
});

fn copies(f: &Repo, a: &Machine<'_>, b: &Machine<'_>) -> String {
    let config = format!(
        "{}\n[dotfiles]\n{COPY_DECLARATION}\n{TEMPLATE_DECLARATION}\n\"~/.linked\" = {{ source = \"link.txt\", mode = \"symlink\" }}\n",
        f.config
    );
    a.source("config.toml", &config);
    a.source("copy.txt", "initial\n");
    a.source("link.txt", "linked\n");
    a.source("template.tera", "{{ 'rendered' }}\n");
    for machine in [a, b] {
        let receipt = machine.sync();
        let target = machine.home.join(".copied");
        assert!(target.exists(), "{receipt}");
        assert_eq!(read(&target), "initial\n");
    }
    config
}

fn hooks(config: &str, pre: &str, post: &str) -> String {
    // Fixture commands use ASCII and shell quoting. TOML basic strings take
    // the same escaping as Rust's debug representation for these values.
    format!("{config}\n[bootstrap.hooks]\npre-dotfiles = {pre:?}\npost-dotfiles = {post:?}\n")
}

macro_rules! copies_case {
    ($($name:ident)+, $hooks:expr, |$f:ident, $a:ident, $b:ident, $config:ident| $body:block) => {
        sync_case!($($name)+, || {
            let $f = Repo::new();
            let $a = Machine::new(&$f, &$f.a, "machine-a", $hooks);
            let $b = Machine::new(&$f, &$f.b, "machine-b", $hooks);
            let $config = copies(&$f, &$a, &$b);
            $body
        });
    };
}

copies_case!(
    active unchanged_second_copy_edit_is_captured_while_incoming_checkout_stays_blocked,
    false,
    |f, a, _b, _config| {
        f.write(&f.a, ".git/info/exclude", "/obstruction\n");
        f.write(&f.a, "obstruction", "private obstruction\n");
        f.publish("obstruction", "incoming tracked file\n");
        for change in ["first capture\n", "second capture\n"] {
            a.target(".copied", change);
            let receipt = a.sync();
            if f.oracle {
                has(&receipt, "blocked");
            } else {
                has(&receipt, "obstructed or edited");
                has(&receipt, "obstruction");
                has(&receipt, "rerun dots sync");
            }
            assert_eq!(read(&f.a.join("obstruction")), "private obstruction\n");
            assert_eq!(read(&f.a.join("copy.txt")), change);
            assert_eq!(f.blob("dev", "copy.txt"), change.trim_end());
        }
    }
);

copies_case!(
    active unchanged_copy_edited_in_pre_apply_stays_while_unrelated_target_updates,
    true,
    |f, a, b, config| {
        let pre = "printf 'late edit\\n' > \"$HOME/.copied\"";
        b.source("config.toml", hooks(&config, pre, ":"));
        b.source("copy.txt", "incoming version\n");
        b.source("template.tera", "{{ 'incoming template' }}\n");
        let before = f.remote();
        b.sync();
        assert_ne!(f.remote(), before);
        let receipt = a.sync();
        has(&receipt, ".copied");
        assert_eq!(read(&a.home.join(".copied")), "late edit\n");
        assert_eq!(read(&f.a.join("copy.txt")), "incoming version\n");
        assert_eq!(read(&a.home.join(".rendered")), "incoming template\n");
    }
);

fn recovery_files<'a>(
    base: &'a Path,
    receipt: &'a str,
) -> impl Iterator<Item = (PathBuf, FixtureFile)> + 'a {
    receipt
        .lines()
        .flat_map(str::split_whitespace)
        .map(|word| {
            word.trim_matches(['(', ')', ',', ';', '`', '"'])
                .trim_end_matches('.')
        })
        .map(PathBuf::from)
        .filter_map(|path| fixture_file(base, &path).map(|entry| (path, entry)))
}

fn assert_recovery_copy(base: &Path, receipt: &str, expected: &str) {
    let saved = recovery_files(base, receipt)
        .filter(|(_, entry)| entry.parent.metadata().unwrap().mode() & 0o777 == 0o700)
        .any(|(_, entry)| std::io::read_to_string(entry.file).is_ok_and(|data| data == expected));
    assert!(
        saved,
        "sync did not print a private recovery copy with the expected bytes: {receipt}"
    );
}

copies_case!(
    active unchanged_prior_copy_stays_writable_through_open_descriptor,
    false,
    |f, a, b, _config| {
        use std::io::{Seek as _, SeekFrom, Write as _};
        b.source("copy.txt", "incoming version\n");
        b.sync();
        let target = a.home.join(".copied");
        let identity = |metadata: fs::Metadata| (metadata.dev(), metadata.ino());
        let original = identity(
            fixture_file(&f.scratch, &target)
                .unwrap()
                .file
                .metadata()
                .unwrap(),
        );
        let mut previous = fixture_editor(&f.scratch, &target).unwrap().file;
        let opened_metadata = previous.metadata().unwrap();
        let original_mode = opened_metadata.mode() & 0o7777;
        let opened = identity(opened_metadata);
        assert_eq!(
            opened, original,
            "descriptor opened a different fixture file"
        );
        let receipt = a.sync();
        let replay = a.replay();
        let after_sync = identity(previous.metadata().unwrap());
        let new_file = fixture_file(&f.scratch, &target).unwrap().file;
        let new_target = identity(new_file.metadata().unwrap());
        // Checkout backups can have the same bytes as the deployed file.
        // The path for an editor's old descriptor must name the same inode.
        let candidates: Vec<_> = [("live", &receipt), ("replay", &replay)]
            .into_iter()
            .flat_map(|(source, text)| {
                recovery_files(&f.scratch, text).map(move |(path, entry)| {
                    (
                        source,
                        identity(entry.file.metadata().unwrap()),
                        path,
                        entry,
                    )
                })
            })
            .collect();
        let evidence = format!(
            "device/inode: target before {original:?}, descriptor opened {opened:?}, \
             descriptor after sync {after_sync:?}, target after sync {new_target:?}; \
             printed paths {candidates:?}\nlive: {receipt}\nreplay: {replay}"
        );
        assert_eq!(after_sync, opened, "{evidence}");
        assert_ne!(new_target, opened, "{evidence}");
        let (_, _, _, mut recovered) = candidates
            .into_iter()
            .find(|(_, file, _, _)| *file == opened)
            .unwrap_or_else(|| panic!("no printed recovery path names the open inode: {evidence}"));
        assert_eq!(recovered.parent.metadata().unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            recovered.file.metadata().unwrap().mode() & 0o7777,
            original_mode,
            "{evidence}"
        );
        assert_eq!(
            std::io::read_to_string(&mut recovered.file).unwrap(),
            "initial\n",
            "{evidence}"
        );
        previous.set_len(0).unwrap();
        previous.seek(SeekFrom::Start(0)).unwrap();
        previous
            .write_all(b"save through old descriptor\n")
            .unwrap();
        assert_eq!(identity(previous.metadata().unwrap()), opened, "{evidence}");
        assert_eq!(
            identity(recovered.file.metadata().unwrap()),
            opened,
            "{evidence}"
        );
        recovered.file.rewind().unwrap();
        assert_eq!(
            std::io::read_to_string(recovered.file).unwrap(),
            "save through old descriptor\n",
            "{evidence}"
        );
        assert_eq!(
            std::io::read_to_string(new_file).unwrap(),
            "incoming version\n",
            "{evidence}"
        );
    }
);

fn guarded_hooks(only_copy: bool) {
    let f = Repo::new();
    let a = Machine::new(&f, &f.a, "machine-a", true);
    let b = Machine::new(&f, &f.b, "machine-b", true);
    let mixed = copies(&f, &a, &b);
    let config = if only_copy {
        format!("{}\n[dotfiles]\n{COPY_DECLARATION}\n", f.config)
    } else {
        mixed
    };
    let pre = "cat \"$HOME/.copied\" > \"$HOME/pre\"; if test -e \"$HOME/edit\"; then echo 'late edit' > \"$HOME/.copied\"; fi; test ! -e \"$HOME/fail\"";
    let post = "cat \"$HOME/.copied\" > \"$HOME/post\"";
    a.source("config.toml", hooks(&config, pre, post));
    a.source("copy.txt", "hook-visible copy\n");
    a.target("fail", "");
    has(&a.sync(), "pre-dotfiles");
    assert_eq!(read(&a.home.join(".copied")), "initial\n");
    assert!(!a.home.join("post").exists());
    fs::remove_file(a.home.join("fail")).unwrap();
    a.sync();
    assert_eq!(read(&a.home.join("pre")), "initial\n");
    assert_eq!(read(&a.home.join("post")), "hook-visible copy\n");
    a.source("copy.txt", "next source\n");
    a.target("edit", "");
    has(&a.sync(), ".copied");
    assert_eq!(read(&a.home.join(".copied")), "late edit\n");
    assert_eq!(read(&a.home.join("post")), "late edit\n");
    assert_eq!(read(&a.root.join("copy.txt")), "next source\n");
}

sync_case!(
    active unchanged_guarded_copy_only_runs_between_real_mise_hooks,
    || {
        guarded_hooks(true);
    }
);
sync_case!(
    active unchanged_mixed_guarded_copies_run_between_real_mise_hooks,
    || {
        guarded_hooks(false);
    }
);

copies_case!(
    active unchanged_real_hook_failure_preserves_phase_and_exit_without_private_output,
    true,
    |_f, a, _b, config| {
        let private = "echo FIXTURE_PRIVATE_HOOK; printf '%s\\n' FIXTURE_PRIVATE_HOOK_ERR 'diff --git a/a b/a' '+FIXTURE_PRIVATE_HOOK_PATCH' >&2; exit 7";
        a.source("config.toml", hooks(&config, private, ":"));
        for (args, verbose, quiet) in [
            (&["--plain", "sync"][..], "0", "0"),
            (&["--verbose", "sync"][..], "1", "0"),
            (&["--plain", "sync"][..], "0", "1"),
        ] {
            let receipt = dots_zero(
                a.dots(args)
                    .env("MISE_VERBOSE", verbose)
                    .env("MISE_QUIET", quiet),
            );
            let replay = a.replay();
            for text in [receipt, replay] {
                if quiet == "0" {
                    has(&text, "pre-dotfiles");
                }
                assert!(
                    ["exit code 7", "exit status 7", "exit 7"]
                        .iter()
                        .any(|status| text.contains(status)),
                    "hook exit status is missing: {text}"
                );
                absent(&text, "FIXTURE_PRIVATE_HOOK");
                absent(&text, "diff --git");
            }
        }
    }
);

copies_case!(
    active unchanged_generated_sources_permissions_and_create_if_missing_apply_the_same_run,
    true,
    |f, a, _b, config| {
        let config = config.replace(
            COPY_DECLARATION,
            &COPY_DECLARATION.replace("mode = \"copy\"", "mode = \"copy\", permissions = \"0644\""),
        );
        let config = format!(
            "{config}\"~/.pending-copy\" = {{ source = \"pending.txt\", mode = \"copy\" }}\n\"~/.kept-copy\" = {{ source = \"kept.txt\", mode = \"copy\", create_if_missing = true }}\n"
        );
        a.source("config.toml", &config);
        a.source("pending.txt", "initial\n");
        a.source("kept.txt", "initial\n");
        a.sync();
        let pre = r#"pwd -P > "$HOME/hook-cwd"
printf '%s\n' "$DOTS_ROOT" > "$HOME/hook-root"
git rev-parse --git-path index > "$HOME/hook-index"
git ls-files --error-unmatch -- copy.txt > "$HOME/hook-tracked" || exit 1
for file in copy.txt pending.txt kept.txt; do echo 'generated by pre-hook' > "$file"; done
sed 's/permissions = "0644"/permissions = "0600"/' config.toml > new.toml && mv new.toml config.toml"#;
        let post =
            "cat \"$HOME/.copied\" \"$HOME/.pending-copy\" \"$HOME/.kept-copy\" > \"$HOME/post\"";
        a.source("config.toml", hooks(&config, pre, post));
        a.source("pending.txt", "planned before hook\n");
        a.sync();
        assert_eq!(Path::new(read(&a.home.join("hook-cwd")).trim()), a.root);
        assert_eq!(Path::new(read(&a.home.join("hook-root")).trim()), a.root);
        assert_eq!(
            a.root.join(read(&a.home.join("hook-index")).trim()),
            f.git_dir(a.root).join("index")
        );
        assert_eq!(read(&a.home.join("hook-tracked")), "copy.txt\n");
        assert_eq!(
            read(&a.home.join("post")),
            "generated by pre-hook\ngenerated by pre-hook\ninitial\n"
        );
        assert_eq!(f.blob("dev", "kept.txt"), "initial");
        assert_eq!(read(&a.root.join("kept.txt")), "generated by pre-hook\n");
        let target = a.home.join(".copied");
        let before = fs::metadata(&target).unwrap();
        assert_eq!(mode(&target), 0o600);
        let expected = f.scratch.join("kept-scan-expected");
        let observed = f.scratch.join("kept-scan-candidate");
        let scanned = f.scratch.join("kept-scanned-clean");
        put(&expected, "generated by pre-hook\n");
        let reader = f.world.fixture_reader();
        let compare_bytes = format!("{} compare", f.world.boundary());
        script(
            &f.tools.join("gitleaks"),
            &format!(
                r#"set -f
for source do :; done
case "$source" in {world}/*) ;; *) exit 88 ;; esac
case "$1" in
  git)
    revision=
    for arg do
      case "$arg" in --log-opts=*)
        for token in ${{arg#*=}}; do
          case "$token" in -*|^*) continue ;; esac
          revision=${{token##*..}}
          revision=${{revision%%'^{{commit}}'}}
          break
        done ;;
      esac
    done
    case "$revision" in *[!0-9a-f]*|'') exit 88 ;; esac
    git -C "$source" cat-file blob "$revision:kept.txt" > {observed} || exit 88 ;;
  dir)
    {reader} "$source/kept.txt" > {observed} || exit 88 ;;
  *) exit 88 ;;
esac
if {compare_bytes} {expected} {observed}; then cp {observed} {scanned}; fi
{GITLEAKS_FIXTURE}"#,
                world = sh(&f.scratch),
                reader = sh(&reader),
                expected = sh(&expected),
                observed = sh(&observed),
                scanned = sh(&scanned),
            ),
        );
        f.hook(
            &f.a,
            "pre-push",
            &format!("{compare_bytes} {} {} || exit 88", sh(&expected), sh(&scanned)),
        );
        a.sync();
        let pushed = finish(
            f.git_command(&f.origin)
                .args(["cat-file", "blob", "dev:kept.txt"]),
        );
        assert!(pushed.status.success(), "{pushed:?}");
        assert_eq!(fs::read(&scanned).unwrap(), b"generated by pre-hook\n");
        assert_eq!(pushed.stdout, b"generated by pre-hook\n");
        fs::remove_file(f.git_dir(&f.a).join("hooks/pre-push")).unwrap();
        script(&f.tools.join("gitleaks"), GITLEAKS_FIXTURE);
        let after = fs::metadata(&target).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        a.target(".kept-copy", "customized\n");
        a.sync();
        assert_eq!(f.blob("dev", "kept.txt"), "generated by pre-hook");
        assert_eq!(
            read(&a.home.join("post")),
            "generated by pre-hook\ngenerated by pre-hook\ncustomized\n"
        );
    }
);

copies_case!(
    active unchanged_template_copy_template_dependency_order_and_edited_target_guard,
    true,
    |_f, a, _b, config| {
        let config = format!(
            "{config}\"~/.dependent\" = {{ source = \"~/.rendered\", mode = \"copy\", depends = [\"~/.rendered\"] }}\n\"~/.consumer\" = {{ source = \"consumer.tera\", mode = \"template\", depends = [\"~/.dependent\"] }}\n"
        );
        a.source(
            "consumer.tera",
            format!(
                "{{{{ read_file(path={:?}) }}}}",
                a.home.join(".dependent").to_str().unwrap()
            ),
        );
        a.source(
            "config.toml",
            hooks(
                &config,
                ":",
                "cat \"$HOME/.dependent\" \"$HOME/.consumer\" > \"$HOME/post\"",
            ),
        );
        a.deploy();
        a.source("template.tera", "{{ 'sync update' }}\n");
        let receipt = a.sync();
        let handoffs = [
            read(&a.home.join(".rendered")),
            read(&a.home.join(".dependent")),
            read(&a.home.join(".consumer")),
        ];
        assert_eq!(
            read(&a.home.join("post")),
            "sync update\nsync update\n",
            "rendered, dependent, consumer: {handoffs:?}\n{receipt}"
        );
        a.target(".dependent", "local edit\n");
        a.source("template.tera", "{{ 'next template' }}\n");
        let receipt = a.sync();
        has(&receipt, ".consumer");
        has(&receipt, "dependency");
        assert_eq!(read(&a.home.join(".rendered")), "next template\n");
        assert_eq!(read(&a.home.join(".dependent")), "local edit\n");
        assert_eq!(read(&a.home.join(".consumer")), "sync update\n");
    }
);

copies_case!(
    active unchanged_directory_copy_preserves_edited_restricted_unmanaged_and_excluded_leaves,
    true,
    |f, a, _b, config| {
        let config = format!(
            "{config}\"~/.tree\" = {{ source = \"dir\", mode = \"copy\", exclude = [\"skip\"], dot_prefix = true }}\n"
        );
        a.source("config.toml", &config);
        for name in [
            "nested/clean",
            "nested/late",
            "dirty",
            "restricted",
            "dot-prefixed",
            "skip",
        ] {
            a.source(&format!("dir/{name}"), "initial\n");
        }
        chmod(&a.root.join("dir/restricted"), 0o644);
        f.commit(&f.a, "Declare directory copy");
        f.git(&f.a, &["push", "-q", "origin", "HEAD:dev"]);
        a.deploy();
        assert!(a.home.join(".tree/.prefixed").exists());
        a.source("dir/dot-prefixed", "first sync\n");
        a.sync();
        assert_eq!(read(&a.home.join(".tree/.prefixed")), "first sync\n");
        a.target(".tree/dirty", "local edit\n");
        a.target(".tree/unmanaged", "keep unmanaged\n");
        let restricted = a.home.join(".tree/restricted");
        assert_eq!(mode(&restricted), 0o644);
        chmod(&restricted, 0o600);
        for name in [
            "nested/clean",
            "nested/late",
            "dirty",
            "restricted",
            "new",
            "skip",
        ] {
            a.source(&format!("dir/{name}"), "source updated\n");
        }
        a.source(
            "config.toml",
            hooks(
                &config,
                "echo 'late editor save' > \"$HOME/.tree/nested/late\"",
                ":",
            ),
        );
        has(&a.sync(), ".tree/dirty");
        for (name, content) in [
            ("nested/clean", "source updated\n"),
            ("nested/late", "late editor save\n"),
            ("new", "source updated\n"),
            ("dirty", "local edit\n"),
            ("restricted", "initial\n"),
            ("unmanaged", "keep unmanaged\n"),
        ] {
            assert_eq!(read(&a.home.join(".tree").join(name)), content, "{name}");
        }
        assert_eq!(mode(&a.home.join(".tree/restricted")), 0o600);
        assert!(!a.home.join(".tree/skip").exists());
        has(&a.sync(), ".tree/dirty");
    }
);

copies_case!(
    active unchanged_dependent_stays_unchanged_when_all_guarded_copies_block,
    false,
    |_f, a, _b, config| {
        a.source(
            "config.toml",
            config.replace(
                "source = \"template.tera\", mode = \"template\"",
                "source = \"template.tera\", mode = \"template\", depends = [\"~/.copied\"]",
            ),
        );
        a.source(
            "template.tera",
            format!(
                "{{{{ read_file(path={:?}) }}}}",
                a.home.join(".copied").to_str().unwrap()
            ),
        );
        a.deploy();
        a.source("copy.txt", "incoming\n");
        a.target(".copied", "local\n");
        has(&a.sync(), "dependency");
        assert_eq!(read(&a.home.join(".copied")), "local\n");
        assert_eq!(read(&a.home.join(".rendered")), "initial\n");
    }
);

copies_case!(
    active unchanged_excluded_directory_leaf_can_be_an_independent_template,
    false,
    |f, a, _b, config| {
        let config = format!(
            "{config}\"~/.tree\" = {{ source = \"dir\", mode = \"copy\", exclude = [\"custom\"] }}\n\"~/.tree/custom\" = {{ source = \"child.tera\", mode = \"template\" }}\n"
        );
        a.source("config.toml", &config);
        a.source("dir/general", "copied\n");
        a.source("dir/custom", "excluded\n");
        a.source("child.tera", "{{ 'native child' }}\n");
        a.deploy();
        a.source("child.tera", "{{ 'sync child' }}\n");
        a.sync();
        assert_eq!(read(&a.home.join(".tree/custom")), "sync child\n");
        assert_eq!(read(&a.home.join(".tree/general")), "copied\n");
        let published = f.remote();
        a.source(
            "config.toml",
            config.replace(", exclude = [\"custom\"]", ""),
        );
        let receipt = a.sync();
        if f.oracle {
            has(&receipt, "invalid");
        } else {
            has(&receipt, "could not inspect deployed copies");
            has(&receipt, "not pushed");
            assert_eq!(f.remote(), published);
        }
        assert_eq!(read(&a.home.join(".tree/custom")), "sync child\n");
    }
);

copies_case!(
    active unchanged_former_directory_leaf_is_protected_until_safe_takeover,
    true,
    |_f, a, _b, config| {
        let directory = "\"~/.tree\" = { source = \"dir\", mode = \"copy\" }\n";
        a.source("config.toml", format!("{config}{directory}"));
        a.source("dir/leaf", "deployed\n");
        a.source("dir/sibling", "initial\n");
        a.sync();
        a.target(".tree/leaf", "local edit\n");
        let excluded = format!(
            "{config}{}",
            directory.replace("mode = \"copy\"", "mode = \"copy\", exclude = [\"leaf\"]")
        );
        a.source(
            "config.toml",
            format!("{excluded}\"~/.tree/leaf\" = {{ mode = \"absent\" }}\n"),
        );
        a.source("dir/sibling", "independent update\n");
        has(&a.sync(), "former directory copy");
        assert_eq!(read(&a.home.join(".tree/leaf")), "local edit\n");
        assert_eq!(read(&a.home.join(".tree/sibling")), "independent update\n");
        fs::rename(a.home.join(".tree/leaf"), a.home.join("recovered")).unwrap();
        let template = format!(
            "{excluded}\"~/.tree/leaf\" = {{ mode = \"template\", source = \"takeover.tera\" }}\n"
        );
        let pre =
            "if test -e \"$HOME/recreate\"; then echo 'late recreation' > \"$HOME/.tree/leaf\"; fi";
        a.source("config.toml", hooks(&template, pre, ":"));
        a.source("takeover.tera", "{{ 'first template' }}\n");
        a.target("recreate", "");
        has(&a.sync(), "former directory copy");
        assert_eq!(read(&a.home.join(".tree/leaf")), "late recreation\n");
        fs::rename(a.home.join(".tree/leaf"), a.home.join("recovered-late")).unwrap();
        fs::remove_file(a.home.join("recreate")).unwrap();
        for text in ["first template", "second template"] {
            a.source("takeover.tera", format!("{{{{ {text:?} }}}}\n"));
            a.sync();
            assert_eq!(read(&a.home.join(".tree/leaf")), format!("{text}\n"));
        }
        assert_eq!(read(&a.home.join("recovered")), "local edit\n");
        assert_eq!(read(&a.home.join("recovered-late")), "late recreation\n");
    }
);

copies_case!(
    active unchanged_selected_rendered_directory_target_is_used,
    false,
    |f, a, _b, config| {
        a.source("miserc.toml", "env = [\"selected\", \"host-fixture\"]\n");
        chmod(&a.root.join("miserc.toml"), 0o600);
        a.source("config.toml", format!("{config}\"~/.declaration-key\" = {{ source = \"./dir\", mode = \"copy\", exclude = [\"skip\"], variants = [{{ profile = \"selected\", target = \"{{{{ env.HOME }}}}/.variant-tree\" }}] }}\n"));
        a.source(
            "config.local.toml",
            "[env]\nSETUP_PROFILE = \"selected\"\nSETUP_MACHINE_ID = \"fixture\"\n",
        );
        chmod(&a.root.join("config.local.toml"), 0o600);
        a.source("dir/leaf", "native variant\n");
        a.source("dir/skip", "excluded\n");
        a.deploy();
        assert_eq!(read(&a.home.join(".variant-tree/leaf")), "native variant\n");
        assert!(!a.home.join(".declaration-key").exists());
        f.commit(&f.a, "Declare directory variant");
        f.git(&f.a, &["push", "-q", "origin", "HEAD:dev"]);
        a.source("dir/leaf", "sync variant\n");
        dots_zero(
            a.dots(&["--plain", "sync"])
                .env("SETUP_PROFILE", "selected"),
        );
        assert_eq!(read(&a.home.join(".variant-tree/leaf")), "sync variant\n");
        assert!(!a.home.join(".variant-tree/skip").exists());
    }
);

copies_case!(
    active unchanged_directory_copy_uses_pre_hook_source_and_keeps_late_target_edit,
    true,
    |_f, a, _b, config| {
        let config = format!("{config}\"~/.tree\" = {{ source = \"dir\", mode = \"copy\" }}\n");
        let pre = "cat \"$HOME/generated\" > dir/leaf; if test -e \"$HOME/edit\"; then echo 'late target edit' > \"$HOME/.tree/leaf\"; fi";
        let post = "cat \"$HOME/.tree/leaf\" > \"$HOME/post\"";
        a.source("config.toml", hooks(&config, pre, post));
        a.source("dir/leaf", "initial\n");
        a.target("generated", "native generated\n");
        a.deploy();
        assert_eq!(read(&a.home.join("post")), "native generated\n");
        a.target("generated", "sync generated\n");
        a.sync();
        assert_eq!(read(&a.home.join("post")), "sync generated\n");
        let leaf = a.home.join(".tree/leaf");
        let before = fs::metadata(&leaf).unwrap();
        a.sync();
        let after = fs::metadata(&leaf).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        a.target("generated", "next source\n");
        a.target("edit", "");
        has(&a.sync(), ".tree/leaf");
        assert_eq!(read(&leaf), "late target edit\n");
        assert_eq!(read(&a.root.join("dir/leaf")), "next source\n");
        assert_eq!(read(&a.home.join("post")), "late target edit\n");
    }
);

copies_case!(
    active new_declared_directory_copy_updates_matching_existing_leaf_after_pre_hook,
    true,
    |f, a, _b, config| {
        let config = format!("{config}\"~/.tree\" = {{ source = \"dir\", mode = \"copy\" }}\n");
        let pre = r#"cat "$HOME/generated" > dir/leaf
if [ -e "$HOME/.tree/replacement" ]; then
    mv -f "$HOME/.tree/replacement" "$HOME/.tree/leaf"
fi"#;
        let post = "cat \"$HOME/.tree/leaf\" > \"$HOME/post\"";
        a.source("config.toml", hooks(&config, pre, post));
        a.source("dir/leaf", "native generated\n");
        a.target(".tree/leaf", "native generated\n");
        let source = a.root.join("dir/leaf");
        let deployed = a.home.join(".tree/leaf");
        assert_eq!(read(&source), read(&deployed));
        assert_eq!(mode(&source), mode(&deployed));
        assert!(f.git(a.root, &["ls-tree", "-r", "HEAD", "--", "dir/leaf"]).is_empty());

        a.target("generated", "sync generated\n");
        let receipt = a.sync();
        assert_eq!(read(&source), "sync generated\n");
        assert_eq!(read(&deployed), "sync generated\n");
        assert_eq!(read(&a.home.join("post")), "sync generated\n");
        assert!(
            !receipt.lines().any(|line| {
                line.contains(".tree/leaf") && line.to_ascii_lowercase().contains("kept")
            }),
            "{receipt}"
        );

        // The validated pre-hook renames this new inode over the observed leaf.
        let replacement = a.home.join(".tree/replacement");
        let original_inode = fs::metadata(&deployed).unwrap().ino();
        let deployed_mode = mode(&deployed);
        a.target(".tree/replacement", "sync generated\n");
        let replacement_inode = fs::metadata(&replacement).unwrap().ino();
        assert_ne!(original_inode, replacement_inode);
        assert_eq!(mode(&replacement), deployed_mode);
        assert_eq!(read(&replacement), read(&deployed));

        a.target("generated", "next source\n");
        let receipt = a.sync();
        assert_eq!(read(&source), "next source\n");
        assert!(!replacement.exists(), "pre-hook must rename the replacement");
        assert_eq!(fs::metadata(&deployed).unwrap().ino(), replacement_inode);
        assert_eq!(read(&deployed), "sync generated\n");
        assert_eq!(mode(&deployed), deployed_mode);
        assert_eq!(read(&a.home.join("post")), "sync generated\n");
        assert!(
            receipt.lines().any(|line| {
                line.contains(".tree/leaf") && line.to_ascii_lowercase().contains("kept")
            }),
            "{receipt}"
        );
    }
);

copies_case!(
    active unchanged_adopted_directory_copy_preserves_deployed_permissions,
    false,
    |f, a, _b, config| {
        a.source(
            "config.toml",
            format!("{config}\"~/.tree\" = {{ source = \"dir\", mode = \"copy\" }}\n"),
        );
        for leaf in ["mode-kept", "mode-edited"] {
            a.source(&format!("dir/{leaf}"), "initial\n");
            chmod(&a.root.join("dir").join(leaf), 0o600);
        }
        f.commit(&f.a, "Seed regular leaves with restrictive modes");
        f.git(&f.a, &["push", "-q", "origin", "HEAD:dev"]);
        a.deploy();
        let kept = a.home.join(".tree/mode-kept");
        let edited = a.home.join(".tree/mode-edited");
        assert_eq!(mode(&kept), 0o600);
        chmod(&edited, 0o644);
        for leaf in ["mode-kept", "mode-edited"] {
            a.source(&format!("dir/{leaf}"), "source updated\n");
        }
        has(&a.sync(), ".tree/mode-edited");
        assert_eq!(read(&kept), "source updated\n");
        assert_eq!(mode(&kept), 0o600);
        assert_eq!(read(&edited), "initial\n");
        assert_eq!(mode(&edited), 0o644);
    }
);

copies_case!(
    active unchanged_reload_commands_run_once_after_copy_and_template_and_before_post,
    true,
    |f, a, _b, config| {
        let trace = "echo pre >> \"$HOME/events\"";
        let post = "echo post >> \"$HOME/events\"";
        a.source("config.toml", hooks(&config, trace, post));
        let first_reload = r#"if [ ! -e "$HOME/first-reload" ]; then
    { printf 'copy='; cat "$HOME/.copied"; printf 'template='; cat "$HOME/.rendered"; } > "$HOME/first-reload"
fi
cat "$HOME/.copied" >> "$HOME/events""#;
        let mut reload = "[history.reload]\n".to_owned();
        for (glob, command) in [
            (
                "~/.copied".to_owned(),
                first_reload,
            ),
            ("~/.rendered".to_owned(), "echo native >> \"$HOME/events\""),
            (
                "~/.{copied,rendered}".to_owned(),
                "echo both >> \"$HOME/events\"",
            ),
            (
                a.home.join(".copied").to_string_lossy().into_owned(),
                first_reload,
            ),
        ] {
            reload.push_str(&format!("{glob:?} = {command:?}\n"));
        }
        a.source("config.local.toml", &reload);
        a.source("copy.txt", "native copy\n");
        a.source("template.tera", "{{ 'native template' }}\n");
        if !f.oracle {
            // Record the intermediate source in Git and the guarded copy state.
            // Recreate the private targets through native apply to observe its reloads.
            has(&a.sync(), "pushed");
            assert_eq!(read(&a.home.join(".copied")), "native copy\n");
            assert_eq!(read(&a.home.join(".rendered")), "native template\n");
            fs::remove_file(a.home.join(".copied")).unwrap();
            fs::remove_file(a.home.join(".rendered")).unwrap();
        }
        a.target("events", "");
        a.deploy();
        assert_eq!(
            read(&a.home.join("events")),
            "pre\nnative copy\nnative\nboth\npost\n"
        );
        fs::remove_file(a.home.join("first-reload")).unwrap();
        a.source("copy.txt", "sync copy\n");
        a.source("template.tera", "{{ 'sync template' }}\n");
        a.target("events", "");
        let receipt = a.sync();
        assert_recovery_copy(
            &f.scratch,
            &format!("{receipt}{}", a.replay()),
            "native copy\n",
        );
        assert_eq!(
            read(&a.home.join("events")),
            "pre\nsync copy\nnative\nboth\npost\n"
        );
        assert_eq!(
            read(&a.home.join("first-reload")),
            "copy=sync copy\ntemplate=sync template\n"
        );
        a.target("events", "");
        a.sync();
        assert_eq!(read(&a.home.join("events")), "pre\npost\n");
        a.source("copy.txt", "incoming copy\n");
        a.target(".copied", "local copy\n");
        a.target("events", "");
        has(&a.sync(), ".copied");
        assert_eq!(read(&a.home.join(".copied")), "local copy\n");
        assert_eq!(read(&a.home.join("events")), "pre\npost\n");
    }
);

copies_case!(
    active unchanged_copy_edits_converge_across_two_machines,
    false,
    |f, a, b, _config| {
        for content in ["first edit\n", "second edit\n"] {
            a.target(".copied", content);
            has(&a.sync(), "pushed");
            b.sync();
            for machine in [&a, &b] {
                assert_eq!(read(&machine.root.join("copy.txt")), content);
                assert_eq!(read(&machine.home.join(".copied")), content);
                assert_eq!(read(&machine.home.join(".rendered")), "rendered\n");
                assert_eq!(
                    fs::read_link(machine.home.join(".linked")).unwrap(),
                    machine.root.join("link.txt")
                );
                f.clean(machine.root);
            }
            assert_eq!(f.tip(a.root), f.tip(b.root));
            assert_eq!(f.tip(a.root), f.remote());
        }
    }
);

copies_case!(
    active unchanged_dirty_copy_survives_incoming_source_and_retry,
    false,
    |f, a, b, _config| {
        const DIRTY: &str = "my dirty copy\n";
        const INCOMING: &str = "incoming edit\n";
        let bare_copy = |revision: &str| {
            let object = format!("{revision}:copy.txt");
            let output = finish(f.git_command(&f.origin).args(["cat-file", "blob", &object]));
            assert!(output.status.success(), "{object}: {output:?}");
            output.stdout
        };
        a.target(".copied", DIRTY);
        b.source("copy.txt", INCOMING);
        b.source("template.tera", "{{ 'incoming template' }}\n");
        b.sync();
        let published = f.remote();
        assert_eq!(bare_copy(&published), INCOMING.as_bytes());

        let receipt = a.sync();
        has(&receipt, ".copied");
        let message = receipt.to_ascii_lowercase();
        let lines: Vec<_> = message.lines().collect();
        assert!(
            lines.iter().enumerate().any(|(index, line)| {
                line.contains("conflict")
                    && line.contains("copy.txt")
                    && lines[index..].iter().take(3).any(|action| {
                        (action.contains("git show")
                            || ((action.contains("recover")
                                || action.contains("compare")
                                || action.contains("reconcile"))
                                && (action.contains("version") || action.contains("parent"))))
                            && (action.contains("sync") || action.contains(":copy.txt"))
                    })
            }),
            "missing conflict-specific recovery for copy.txt: {receipt}"
        );

        let merged = f.remote();
        let parents = f.git(&f.origin, &["show", "-s", "--format=%P", &merged]);
        let parents: Vec<_> = parents.split_whitespace().collect();
        assert_eq!(
            parents.len(),
            2,
            "{merged} has parents {parents:?}\n{receipt}"
        );
        assert!(
            parents.contains(&published.as_str()),
            "missing incoming parent: {parents:?}"
        );
        let losing = parents.into_iter().find(|oid| *oid != published).unwrap();
        assert_eq!(bare_copy(losing), DIRTY.as_bytes());
        assert_eq!(bare_copy(&merged), INCOMING.as_bytes());
        // Path-limited Git history can omit the losing side of a merge.
        let ancestry = f.git(&f.origin, &["rev-list", "dev"]);
        assert!(ancestry.lines().any(|oid| oid == losing), "{receipt}");
        assert!(ancestry.lines().any(|oid| oid == published), "{receipt}");

        for line in receipt
            .lines()
            .filter(|line| line.contains("git show") && line.contains("copy.txt"))
        {
            let reported = line
                .split_whitespace()
                .filter_map(|word| {
                    word.trim_matches(['(', ')', '\'', '"', '`', ',', ';'])
                        .strip_suffix(":copy.txt")
                })
                .find(|oid| {
                    (7..=64).contains(&oid.len())
                        && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .unwrap_or_else(|| panic!("missing recovery revision in {line}"));
            let resolved = f.git(
                &f.origin,
                &["rev-parse", "--verify", &format!("{reported}^{{commit}}")],
            );
            assert_eq!(resolved, losing, "{line}");
            assert_eq!(bare_copy(&resolved), DIRTY.as_bytes());
        }

        assert_eq!(read(&f.a.join("copy.txt")), INCOMING);
        assert_eq!(read(&a.home.join(".copied")), DIRTY);
        f.clean(&f.a);
        let retry = a.sync();
        absent(&retry.to_ascii_lowercase(), "conflict");
        assert_eq!(f.remote(), merged, "{retry}");
        assert_eq!(f.tip(&f.a), merged, "{retry}");
        assert_eq!(read(&f.a.join("copy.txt")), INCOMING);
        assert_eq!(read(&a.home.join(".copied")), DIRTY);
        assert_eq!(read(&a.home.join(".rendered")), "incoming template\n");
        f.clean(&f.a);
    }
);

copies_case!(
    active unchanged_diverged_copy_and_source_preserve_both_versions,
    false,
    |f, a, _b, _config| {
        a.target(".copied", "local target\n");
        a.source("copy.txt", "local source\n");
        a.source("unrelated", "share this\n");
        let receipt = a.sync();
        has(&receipt, ".copied");
        has(
            &receipt,
            if f.oracle {
                "also changed"
            } else {
                "both the copy and source changed"
            },
        );
        assert_eq!(read(&a.home.join(".copied")), "local target\n");
        assert_eq!(f.blob("dev", "copy.txt"), "local source");
        assert_eq!(f.blob("dev", "unrelated"), "share this");
    }
);

fn incoming_copy_replacement(declaration: &str, deployed: Option<&str>) {
    let f = Repo::new();
    let a = Machine::new(&f, &f.a, "machine-a", false);
    let b = Machine::new(&f, &f.b, "machine-b", false);
    let config = copies(&f, &a, &b);
    a.target(".copied", "keep my copy\n");
    a.source("copy.txt", "keep my source\n");
    b.source("template.tera", "{{ 'updated template' }}\n");
    b.source("new.txt", "initial\n");
    b.source("config.toml", config.replace(COPY_DECLARATION, declaration));
    b.sync();
    has(&a.sync(), ".copied");
    let shared = f.remote();
    has(&a.sync(), ".copied");
    assert_eq!(f.remote(), shared);
    assert_eq!(read(&a.home.join(".copied")), "keep my copy\n");
    assert_eq!(f.blob("dev", "copy.txt"), "keep my source");
    fs::rename(a.home.join(".copied"), a.home.join("saved")).unwrap();
    a.sync();
    if let Some(content) = deployed {
        assert_eq!(read(&a.home.join(".copied")), content);
    } else {
        assert!(!a.home.join(".copied").exists());
    }
    assert_eq!(read(&a.home.join("saved")), "keep my copy\n");
}

const ABSENT_COPY: &str = r#""~/.copied" = { mode = "absent" }"#;
const TEMPLATE_COPY: &str = r#""~/.copied" = { mode = "template", source = "template.tera" }"#;
const REPLACEMENT_COPY: &str = r#""~/.copied" = { mode = "copy", source = "new.txt" }"#;

sync_case!(active unchanged_incoming_absent_does_not_remove_dirty_copy, || {
    incoming_copy_replacement(ABSENT_COPY, None);
});
sync_case!(
    active unchanged_incoming_template_does_not_replace_dirty_copy,
    || {
        incoming_copy_replacement(TEMPLATE_COPY, Some("updated template\n"));
    }
);
sync_case!(
    active unchanged_incoming_different_source_does_not_capture_dirty_copy,
    || {
        incoming_copy_replacement(REPLACEMENT_COPY, Some("initial\n"));
    }
);

fn former_copy_edit(interrupted: bool) {
    let f = Repo::new();
    let a = Machine::new(&f, &f.a, "machine-a", false);
    let b = Machine::new(&f, &f.b, "machine-b", false);
    let config = copies(&f, &a, &b).replace(COPY_DECLARATION, ABSENT_COPY);
    if interrupted {
        b.source("config.toml", &config);
        b.sync();
        let lock = f.git_dir(&f.a).join("refs/heads/dev.lock");
        put(&lock, "");
        has(&a.sync(), "pending");
        fs::remove_file(lock).unwrap();
    } else {
        a.source("config.toml", &config);
    }
    a.target(".copied", "keep my copy\n");
    let published = f.remote();
    has(&a.sync(), ".copied");
    assert_eq!(read(&a.home.join(".copied")), "keep my copy\n");
    if interrupted {
        assert_eq!(f.remote(), published);
    }
    f.clean(&f.a);
}

sync_case!(
    active unchanged_locally_changed_replacement_checks_former_copy,
    || {
        former_copy_edit(false);
    }
);
sync_case!(
    active unchanged_interrupted_incoming_replacement_checks_former_copy,
    || {
        former_copy_edit(true);
    }
);

fn former_copy_after_declaration_removal(remembered: bool) {
    let f = Repo::new();
    let a = Machine::new(&f, &f.a, "machine-a", false);
    let b = Machine::new(&f, &f.b, "machine-b", false);
    let config = if remembered {
        let config = copies(&f, &a, &b);
        a.source("config.toml", config.replace(COPY_DECLARATION, ""));
        a.sync();
        config
    } else {
        let config = format!("{}\n[dotfiles]\n{COPY_DECLARATION}\n", f.config);
        a.source("config.toml", &config);
        a.source("copy.txt", "initial\n");
        f.commit(&f.a, "Native copy declaration");
        f.git(&f.a, &["push", "-q", "origin", "HEAD:dev"]);
        a.deploy();
        config
    };
    assert_eq!(read(&a.home.join(".copied")), "initial\n");
    a.target(".copied", "keep my copy\n");
    a.source("config.toml", config.replace(COPY_DECLARATION, ABSENT_COPY));
    for _ in 0..2 {
        has(&a.sync(), ".copied");
        assert_eq!(read(&a.home.join(".copied")), "keep my copy\n");
    }
    f.clean(&f.a);
}

sync_case!(
    active unchanged_removed_declaration_protects_dirty_copy_from_later_absent,
    || {
        former_copy_after_declaration_removal(true);
    }
);
sync_case!(
    active unchanged_native_mise_only_state_protects_dirty_copy_from_later_absent,
    || {
        former_copy_after_declaration_removal(false);
    }
);

copies_case!(
    active unchanged_old_bytes_redeployed_by_mise_do_not_revive_stale_copy_base,
    false,
    |_f, a, _b, config| {
        fs::remove_file(a.home.join(".copied")).unwrap();
        a.deploy();
        a.target(".copied", "captured once\n");
        has(&a.sync(), "pushed");
        for content in ["another deployment\n", "initial\n"] {
            a.source("copy.txt", content);
            a.deploy();
            assert_eq!(read(&a.home.join(".copied")), content);
        }
        a.target(".copied", "captured once\n");
        a.source("config.toml", config.replace(COPY_DECLARATION, ABSENT_COPY));
        has(&a.sync(), ".copied");
        assert_eq!(read(&a.home.join(".copied")), "captured once\n");
    }
);

copies_case!(
    active unchanged_clean_copy_to_template_restores_ordinary_template_updates,
    false,
    |_f, a, _b, config| {
        a.source(
            "config.toml",
            config.replace(COPY_DECLARATION, TEMPLATE_COPY),
        );
        a.sync();
        assert_eq!(read(&a.home.join(".copied")), "rendered\n");
        a.target(".copied", "template output edited by hand\n");
        a.source("template.tera", "{{ 'next template' }}\n");
        a.sync();
        assert_eq!(read(&a.home.join(".copied")), "next template\n");
    }
);

fn retire_native_mise(listed: bool) {
    let f = Repo::new();
    let a = Machine::new(&f, &f.a, "machine-a", false);
    const LOCAL: &str = "config.local.toml";
    let local = format!("{KEEP_LOCAL}{RETIRED_DECLARATION}");
    let source = "templates/fixture.tera";
    f.publish(source, "{{ 'fixture' }}\n");
    a.sync();
    a.source(LOCAL, &local);
    a.deploy();
    assert_eq!(read(&a.home.join(".fixture-rendered")), "fixture\n");
    fs::remove_file(f.b.join(source)).unwrap();
    if listed {
        f.write(
            &f.b,
            "dots/retired-declarations",
            format!("dotfiles.fixture\t{source}\n"),
        );
    }
    let tip = f.commit(&f.b, "Remove template");
    f.git(&f.b, &["push", "-q", "origin", "HEAD:dev"]);
    let receipt = a.sync();
    if listed {
        has(&receipt, "retired");
        assert_eq!(read(&f.a.join(LOCAL)), KEEP_LOCAL);
        assert_eq!(f.tip(&f.a), tip);
    } else {
        rejected_validation(&f, &receipt);
        if f.oracle {
            has(&receipt, source);
        }
        assert_eq!(read(&f.a.join(LOCAL)), local);
        assert!(f.a.join(source).exists());
    }
    assert_eq!(read(&a.home.join(".fixture-rendered")), "fixture\n");
}

sync_case!(
    active unchanged_real_mise_retires_matching_declaration_and_keeps_rendered_file,
    || {
        retire_native_mise(true);
    }
);
sync_case!(
    active unchanged_real_mise_rejects_unlisted_retirement_and_keeps_rendered_file,
    || {
        retire_native_mise(false);
    }
);

sync_case!(active new_local_only_sync_skips_all_stages, || {
    let f = Repo::new();
    let before = f.tip(&f.a);
    f.git(&f.a, &["remote", "remove", "origin"]);
    f.write(
        &f.a,
        "config.toml",
        "[vars]\ndots_profiles = \"laptop selected\"\n",
    );
    f.write(&f.a, "public", "keep local\n");
    let scanner = f.scratch.join("scanner-ran");
    script(
        &f.tools.join("gitleaks"),
        &format!("touch {}\nexit 1", sh(&scanner)),
    );
    let saved = f.scratch.join("history-save-ran");
    let receipt = f.sync_env(&f.a, &[("FIXTURE_SAVED", saved.to_str().unwrap())]);
    has(&receipt, "remote");
    has(&receipt, "add");
    assert_eq!(receipt.lines().count(), 1, "{receipt}");
    assert!(!scanner.exists() && !saved.exists());
    assert_eq!(f.tip(&f.a), before);
    assert_eq!(read(&f.a.join("public")), "keep local\n");
});

sync_case!(active new_empty_reachable_origin_creates_only_dev, || {
    let f = Repo::new();
    f.git(&f.origin, &["update-ref", "-d", "refs/heads/dev"]);
    f.git(&f.origin, &["update-ref", "-d", "refs/heads/main"]);
    f.write(&f.a, "public", "share\n");
    let receipt = f.sync(&f.a);
    absent(&receipt.to_ascii_lowercase(), "offline");
    assert_eq!(f.tip(&f.a), f.remote());
    assert_eq!(f.blob("dev", "public"), "share");
    assert_eq!(
        f.git(
            &f.origin,
            &["for-each-ref", "--format=%(refname)", "refs/heads"]
        ),
        "refs/heads/dev"
    );
});

fn deny_transport(f: &Repo) -> PathBuf {
    let git = f.tools.join("git");
    let real = fs::read_link(&git).unwrap();
    let attempted = f.scratch.join("unexpected-transport");
    fs::remove_file(&git).unwrap();
    script(
        &git,
        &format!(
            "case \" $* \" in *' ls-remote --get-url '*) exec {} \"$@\" ;; esac\nfor arg do\n  case \"$arg\" in fetch|push|ls-remote|clone|pull) printf '%s\\n' \"$arg\" >> {}; exit 88 ;; esac\ndone\nexec {} \"$@\"",
            sh(&real),
            sh(&attempted),
            sh(&real)
        ),
    );
    attempted
}

fn rejected_remote(origin: &str) {
    let f = Repo::new();
    let before = f.remote();
    f.write(
        &f.a,
        "config.toml",
        "[vars]\ndots_profiles = \"laptop selected\"\n",
    );
    f.git(&f.a, &["remote", "set-url", "origin", origin]);
    let attempted = deny_transport(&f);
    let receipt = f.sync(&f.a);
    assert!(
        !attempted.exists(),
        "sync attempted transport before validating origin"
    );
    has(&receipt, "origin");
    absent(&receipt, "FIXTURE_REMOTE_CREDENTIAL");
    absent(&f.replay(&f.a), "FIXTURE_REMOTE_CREDENTIAL");
    assert_eq!(f.remote(), before);
    assert_eq!(f.tip(&f.a), before);
}

sync_case!(
    active new_credential_bearing_origin_is_refused_before_transport,
    || {
        let url = "https://fixture-user:FIXTURE_REMOTE_CREDENTIAL@fixture.invalid/dots";
        rejected_remote(url);
    }
);

sync_case!(active new_remote_helper_origin_is_refused_before_transport, || {
    let url = "ext::false";
    rejected_remote(url);
});

sync_case!(
    active new_effective_push_rewrite_is_refused_before_transport,
    || {
        let f = Repo::new();
        let raw = format!("file://{}", f.origin.display());
        let outside = f.world.bare_remote("unexpected.git");
        f.git(
            &f.a,
            &[
                "config",
                &format!("url.file://{}.pushInsteadOf", outside.display()),
                &raw,
            ],
        );
        let before = f.remote();
        f.write(&f.a, "public", "keep this local\n");
        let attempted = deny_transport(&f);
        has(&f.sync(&f.a), "origin");
        assert!(
            !attempted.exists(),
            "sync attempted transport after a mismatched rewrite"
        );
        assert_eq!(f.remote(), before);
        assert_eq!(f.tip(&f.a), before);
        assert_eq!(
            f.git(
                &outside,
                &["for-each-ref", "--format=%(refname)", "refs/heads"]
            ),
            ""
        );
        assert_eq!(read(&f.a.join("public")), "keep this local\n");
    }
);

sync_case!(
    active new_matching_explicit_file_url_syncs_through_local_origin,
    || {
        let f = Repo::new();
        let remote = format!("file://{}", f.origin.display());
        f.write(
            &f.a,
            "config.toml",
            format!("[vars]\ndots_profiles = \"laptop selected\"\ndots_source_repo = {remote:?}\n"),
        );
        f.git(&f.a, &["remote", "set-url", "origin", &remote]);
        f.write(&f.a, "public", "share through a literal local origin\n");
        f.sync(&f.a);
        assert_eq!(f.remote(), f.tip(&f.a));
        assert_eq!(
            f.blob("dev", "public"),
            "share through a literal local origin"
        );
    }
);

sync_case!(
    active new_configured_remote_without_origin_reports_missing_and_keeps_edits,
    || {
        let f = Repo::new();
        let before = f.tip(&f.a);
        f.git(&f.a, &["remote", "remove", "origin"]);
        f.write(&f.a, "local", "do not reset\n");
        let receipt = f.sync(&f.a);
        has(&receipt, "origin");
        has(&receipt, "missing");
        assert_eq!(f.tip(&f.a), before);
        assert_eq!(f.remote(), before);
        assert_eq!(read(&f.a.join("local")), "do not reset\n");
    }
);

sync_case!(
    active new_sync_missing_configuration_still_exits_zero_with_receipt,
    || {
        let f = Repo::new();
        let before = f.remote();
        fs::remove_file(f.a.join("config.toml")).unwrap();
        let receipt = f.sync(&f.a);
        has(&receipt, "config");
        assert_eq!(f.remote(), before);
        assert_eq!(f.tip(&f.a), before);
    }
);

sync_case!(active new_sync_missing_mise_still_exits_zero_with_receipt, || {
    let f = Repo::new();
    let before = f.remote();
    fs::remove_file(f.tools.join("mise")).unwrap();
    let receipt = dots_zero(
        f.dots(&f.a, &["--plain", "sync"])
            .env("DOTS_MISE_BIN", f.tools.join("missing-mise")),
    );
    has(&receipt, "mise");
    assert_eq!(f.remote(), before);
    assert_eq!(f.tip(&f.a), before);
});

sync_case!(
    active new_sync_invalid_option_exits_two_without_touching_state,
    || {
        let f = Repo::new();
        let before = f.remote();
        let mut command = f.dots(&f.a, &["sync", "--invalid"]);
        let output = finish(&mut command);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(f.remote(), before);
        assert_eq!(f.tip(&f.a), before);
    }
);

sync_case!(
    active unchanged_up_receives_changed_path_once_and_finishes_source_sync,
    || {
        let f = Repo::new();
        let tip = f.publish("fixture-incoming", "from peer\n");
        let first = dots_zero(&mut f.dots(&f.a, &["--plain", "up"]));
        let first_replay = f.replay(&f.a);
        let second = dots_zero(&mut f.dots(&f.a, &["--plain", "up"]));
        let second_replay = f.replay(&f.a);
        has(&first, "sync configuration");
        has(&second, "sync configuration");
        assert_eq!(
            first_replay.matches("fixture-incoming").count(),
            1,
            "{first_replay}"
        );
        absent(&second_replay, "fixture-incoming");
        assert_eq!(f.tip(&f.a), tip);
        assert_eq!(read(&f.a.join("fixture-incoming")), "from peer\n");
    }
);

sync_case!(
    active unchanged_missing_scanner_blocks_candidate_before_local_snapshot,
    || {
        let f = Repo::new();
        let before = f.remote();
        fs::remove_file(f.tools.join("gitleaks")).unwrap();
        let mut command = f.dots(&f.a, &["--verbose", "sync"]);
        command.env("PATH", f.scanner_fixture_path());
        assert!(executable_on_path(&command, "gitleaks").is_none());
        assert!(executable_on_path(&command, "cmp").is_none());
        if !f.oracle {
            assert!(executable_on_path(&command, "node").is_none());
            assert!(executable_on_path(&command, "python3").is_none());
        }
        f.write(&f.a, "local", "never publish without scanning\n");
        let receipt = dots_zero(&mut command);
        has(&receipt, "gitleaks");
        assert_eq!(f.remote(), before);
        assert_eq!(f.tip(&f.a), before);
        assert_eq!(read(&f.a.join("local")), "never publish without scanning\n");
        script(&f.tools.join("gitleaks"), GITLEAKS_FIXTURE);
        let recovery = dots_zero(&mut command);
        assert_ne!(f.remote(), before, "{recovery}");
        assert_eq!(f.blob("dev", "local"), "never publish without scanning");
    }
);

#[test]
fn new_missing_configured_scanner_suggests_up_but_broken_scanner_does_not() {
    let f = Repo::new();
    let before = f.remote();
    // Model mise's passive tool metadata, including the missing installation.
    script(
        &f.mise,
        &format!(
            "if [ \"$*\" = 'tool --json gitleaks' ]; then\n  printf '%s\\n' '{{\"config_source\":{{}}}}'\n  exit 0\nfi\n{MISE_FIXTURE}"
        ),
    );
    f.write(
        &f.a,
        "config.toml",
        format!("{}\n[tools]\ngitleaks = 'latest'\n", f.config),
    );
    f.write(&f.a, "text", "still local\n");
    let scanner = f.tools.join("gitleaks");
    fs::remove_file(&scanner).unwrap();
    let mut sync = f.dots(&f.a, &["--plain", "sync"]);
    sync.env("PATH", f.scanner_fixture_path());
    for with_shim in [false, true] {
        if with_shim {
            symlink(&f.mise, &scanner).unwrap();
        }
        let receipt = dots_zero(&mut sync);
        has(&receipt, "gitleaks not found");
        has(&receipt, "Run dots up");
        assert_eq!(f.remote(), before);
        assert_eq!(f.tip(&f.a), before);
    }
    fs::remove_file(&scanner).unwrap();
    script(&scanner, "exit 127");
    let broken = dots_zero(&mut sync);
    has(&broken, "the gitleaks scan failed");
    absent(&broken, "dots up");
    assert_eq!(f.remote(), before);
}

sync_case!(
    active unchanged_partial_checkout_filter_failure_retries_without_republishing,
    || {
        let f = Repo::new();
        let before = f.tip(&f.a);
        f.write(&f.b, ".gitattributes", "z-filtered filter=fixture\n");
        f.write(&f.b, " a-first", "incoming first\n");
        let published = f.publish("z-filtered", "incoming last\n");
        f.git(&f.a, &["config", "filter.fixture.required", "true"]);
        f.git(&f.a, &["config", "filter.fixture.clean", "cat"]);
        f.git(
            &f.a,
            &[
                "config",
                "filter.fixture.smudge",
                "if [ \"${FIXTURE_LIVE_CHECKOUT:-}\" = 1 ]; then exit 9; fi; cat",
            ],
        );
        let shim = f.tools.join("git");
        let real = fs::read_link(&shim).unwrap();
        fs::remove_file(&shim).unwrap();
        // The Git proxy makes the filter fail during live checkout only. Git
        // remains responsible for the partial write, and the CLI must recover.
        script(
            &shim,
            &format!(
                "case \" $* \" in *' read-tree '*' -u '*|*' checkout-index '*) export FIXTURE_LIVE_CHECKOUT=1 ;; esac\nexec {} \"$@\"",
                sh(&real),
            ),
        );
        has(&f.sync(&f.a), "pending");
        assert_eq!(f.tip(&f.a), before);
        assert_eq!(read(&f.a.join(" a-first")), "incoming first\n");
        assert!(!f.a.join("z-filtered").exists());
        f.git(&f.a, &["config", "filter.fixture.smudge", "cat"]);
        f.sync(&f.a);
        assert_eq!(f.tip(&f.a), published);
        assert_eq!(f.remote(), published);
        assert_eq!(read(&f.a.join("z-filtered")), "incoming last\n");
        f.clean(&f.a);
    }
);

copies_case!(
    active unchanged_first_takeover_child_preserves_competing_file_and_blocks_dependents,
    false,
    |f, a, _b, config| {
        let original = format!(
            "{config}\"~/.tree\" = {{ source = \"dir\", mode = \"copy\" }}\n\"~/.consumer\" = {{ mode = \"template\", source = \"consumer.tera\" }}\n"
        );
        a.source("config.toml", original);
        a.source("dir/leaf", "original copy\n");
        a.source("consumer.tera", "{{ 'previous consumer' }}\n");
        a.sync();
        let target = a.home.join(".tree/leaf");
        fs::rename(&target, a.home.join("saved-leaf")).unwrap();
        let next = format!(
            "{config}\"~/.tree\" = {{ source = \"dir\", mode = \"copy\", exclude = [\"leaf\"] }}\n\"~/.consumer\" = {{ mode = \"template\", source = \"consumer.tera\", depends = [\"~/.tree/leaf\"] }}\n\"~/.tree/leaf\" = {{ mode = \"template\", source = \"takeover.tera\", permissions = \"0600\" }}\n"
        );
        a.source("config.toml", &next);
        a.source("takeover.tera", "{{ 'incoming template' }}\n");
        a.source("consumer.tera", "{{ 'new consumer' }}\n");
        a.source("config.local.toml", "[history.reload]\n\"~/.tree/leaf\" = 'cat \"$HOME/.tree/leaf\" >> \"$HOME/reloads\"'\n");
        let marker = f.scratch.join("takeover-child");
        let wrapper = f.tools.join("mise-takeover");
        let reader = f.world.fixture_reader();
        script(
            &wrapper,
            &format!(
                r#"case " $* " in
  *' --dry-run '*) ;;
  *' bootstrap dotfiles apply '*)
    if [ ! -e {marker} ]; then
      for argument do
        case "$argument" in
          */dots-dotfile-*/rendered)
            # The guarded renderer uses this staged destination for every
            # template. Select only the configuration for the takeover.
            [ "$PWD" = "${{argument%/rendered}}/config" ] || continue
            definition=$({reader} config.toml) || exit 88
            printf '%s\n' "$definition" | grep -Fq 'takeover.tera' || continue ;;
          '~/.tree/leaf'|{target}|*/copy-handoff-*/output) ;;
          *) continue ;;
        esac
        printf 'competing edit\n' > {target}
        touch {marker}
        break
      done
    fi ;;
esac
exec {mise} "$@""#,
                marker = sh(&marker),
                reader = sh(&reader),
                target = sh(&target),
                mise = sh(&a.mise)
            ),
        );
        let receipt = dots_zero(
            a.dots(&["--verbose", "sync"])
                .env("DOTS_MISE_BIN", &wrapper),
        );
        assert!(
            marker.exists(),
            "native apply did not reach the competing-writer seam: {receipt}"
        );
        has(&receipt, ".tree/leaf");
        has(&receipt, "dependency");
        assert_eq!(read(&target), "competing edit\n");
        assert_eq!(read(&a.home.join(".consumer")), "previous consumer\n");
        assert!(!a.home.join("reloads").exists());
        has(&a.sync(), "former directory copy");
        assert_eq!(read(&a.home.join("saved-leaf")), "original copy\n");
        fs::rename(&target, a.home.join("saved-competing")).unwrap();
        a.sync();
        assert_eq!(read(&target), "incoming template\n");
        assert_eq!(mode(&target), 0o600);
        assert_eq!(read(&a.home.join(".consumer")), "new consumer\n");
        assert_eq!(read(&a.home.join("reloads")), "incoming template\n");
        assert_eq!(read(&a.home.join("saved-competing")), "competing edit\n");
    }
);

sync_case!(
    active unchanged_two_contenders_reclaim_a_stale_lock_only_once,
    || {
        use std::time::{Duration, Instant};
        let f = Repo::new();
        let before = f.remote();
        let release = f.scratch.join("release-contenders");
        let marker = f.scratch.join("contender");
        let hook = f.validation_hook(&format!(
        "touch {marker}.${{FIXTURE_RUN:?}}\nfor n in $(seq 1 60); do\n  if [ -e {release} ] || [ -e {release}.${{FIXTURE_RUN:?}} ]; then exit 0; fi\n  sleep 1\ndone\nexit 97",
        marker = sh(&marker), release = sh(&release),
    ));
        f.write(&f.a, "local", "waiting\n");
        let parent = f.scratch.join("lock-holder-pid");
        let start = |name: &str| {
            let mut command = f.dots(&f.a, &["--verbose", "sync"]);
            command
                .env("FIXTURE_VALIDATE", &hook)
                .env("FIXTURE_RUN", name);
            if name == "dead" {
                command.env("FIXTURE_VALIDATION_PARENT", &parent);
            }
            RunningSync::start(&mut command, &f.scratch.join(format!("run-{name}")))
        };
        let dead = start("dead");
        dead.wait_until_started(&marker.with_extension("dead"));
        dead.kill_lock_holder(&parent, &release.with_extension("dead"));
        assert!(!dead.finish().status.success());
        let a = start("a");
        let b = start("b");
        let deadline = Instant::now() + Duration::from_secs(45);
        let a_marker = marker.with_extension("a");
        let b_marker = marker.with_extension("b");
        let (winner, loser) = loop {
            let a_finished = a.exited();
            let b_finished = b.exited();
            assert!(
                !(a_marker.exists() && b_marker.exists()),
                "both CLI processes entered the locked stage"
            );
            assert!(
                Instant::now() < deadline,
                "the stale lock was not reclaimed"
            );
            if a_marker.exists() && b_finished {
                break (a, b);
            }
            if b_marker.exists() && a_finished {
                break (b, a);
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let skipped = loser.finish();
        assert_eq!(skipped.status.code(), Some(0), "{skipped:?}");
        has(stdout(&skipped), "another");
        assert_eq!(f.remote(), before);
        has(&f.sync(&f.a), "another");
        put(&release, "");
        let accepted = winner.finish();
        assert_eq!(accepted.status.code(), Some(0), "{accepted:?}");
        has(stdout(&accepted), "pushed");
        assert_eq!(f.blob("dev", "local"), "waiting");
        f.clean(&f.a);
    }
);

copies_case!(
    active unchanged_copy_capture_keeps_source_edit_observed_during_native_inventory,
    false,
    |f, a, _b, _config| {
        a.target(".copied", "deployed edit\n");
        a.source("unrelated", "share this too\n");
        let source = f.a.join("copy.txt");
        let marker = f.scratch.join("late-source");
        let wrapper = f.tools.join("mise-source-edit");
        script(
            &wrapper,
            &format!(
                "case \" $* \" in\n  *' bootstrap dotfiles status '*)\n    if [ ! -e {marker} ]; then\n      {mise} \"$@\"\n      echo 'late source edit' > {source}\n      touch {marker}\n      exit 0\n    fi ;;\nesac\nexec {mise} \"$@\"",
                marker = sh(&marker),
                source = sh(&source),
                mise = sh(&a.mise),
            ),
        );
        let receipt = dots_zero(
            a.dots(&["--verbose", "sync"])
                .env("DOTS_MISE_BIN", &wrapper),
        );
        assert!(marker.exists(), "{receipt}");
        assert_eq!(read(&source), "late source edit\n");
        assert_eq!(read(&a.home.join(".copied")), "deployed edit\n");
        assert_eq!(f.blob("dev", "copy.txt"), "late source edit");
        assert_eq!(f.blob("dev", "unrelated"), "share this too");
    }
);

copies_case!(
    active unchanged_symlink_copy_source_keeps_a_target_created_in_pre_hook,
    true,
    |f, a, b, config| {
        fs::remove_file(a.home.join(".copied")).unwrap();
        fs::rename(f.b.join("copy.txt"), f.b.join("contents.txt")).unwrap();
        symlink("contents.txt", f.b.join("copy.txt")).unwrap();
        b.source("contents.txt", "incoming symlink source\n");
        let pre = "if [ -e \"$HOME/create-target\" ]; then echo 'late new target' > \"$HOME/.copied\"; fi";
        b.source("config.toml", hooks(&config, pre, ":"));
        b.sync();
        a.target("create-target", "");
        let receipt = a.sync();
        has(&receipt, ".copied");
        assert_eq!(read(&a.home.join(".copied")), "late new target\n");
        assert_eq!(read(&f.a.join("copy.txt")), "incoming symlink source\n");
        assert_eq!(
            fs::read_link(f.a.join("copy.txt")).unwrap(),
            Path::new("contents.txt")
        );
    }
);

#[test]
fn new_file_adoption_below_home_shares_and_preserves_source_guard() {
    let world = IsolatedWorld::new();
    let root = world.home().join(".config/mise");
    fs::create_dir_all(&root).unwrap();
    let real_mise = executable_on_path(&world.git(), "mise").expect("real mise on fixture PATH");
    let real_gitleaks =
        executable_on_path(&world.git(), "gitleaks").expect("real gitleaks on fixture PATH");
    let mise = world.native_mise_adapter(&real_mise);
    let tools = world.root().with_file_name("adoption-tools");
    fs::create_dir(&tools).unwrap();
    symlink(real_gitleaks, tools.join("gitleaks")).unwrap();
    symlink(
        executable_on_path(&world.git(), "git").unwrap(),
        tools.join("git"),
    )
    .unwrap();
    let path =
        std::env::join_paths([tools.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let dots = || {
        let mut cmd = world.dots();
        cmd.current_dir(&root)
            .env("DOTS_ROOT", &root)
            .env("MISE_CONFIG_DIR", &root)
            .env("MISE_TRUSTED_CONFIG_PATHS", &root)
            .env("DOTS_MISE_BIN", &mise)
            .env("PATH", &path);
        cmd
    };
    let git = |cwd: &Path, args: &[&str]| success(world.git().current_dir(cwd).args(args));
    dots_zero(dots().arg("init").arg(&root));
    assert_eq!(git(&root, &["branch", "--show-current"]), "dev");
    let remote = world.bare_remote("adoption.git");
    git(&root, &["config", "user.name", "Dots fixture"]);
    git(&root, &["config", "user.email", "fixture@example.invalid"]);
    git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            &format!("file://{}", remote.display()),
        ],
    );
    git(&root, &["push", "origin", "main", "dev"]);
    let target = world.home().join(".config/example/settings.toml");
    let source_key = "dotfiles/.config/example/settings.toml";
    let source = root.join(source_key);
    put(&target, "[settings]\nname = 'adopted-synthetic-file'\n");
    dots_zero(dots().arg("add").arg(&target));
    assert_eq!(fs::read_link(&target).unwrap(), source);
    assert_eq!(read(&source), read(&target));

    let sync = || dots_zero(dots().args(["--plain", "sync"]));
    let first = sync();
    assert_eq!(
        git(
            &remote,
            &[
                "show",
                "refs/heads/dev:dotfiles/.config/example/settings.toml"
            ]
        ),
        "[settings]\nname = 'adopted-synthetic-file'"
    );
    assert_eq!(git(&root, &["status", "--porcelain=v1"]), "");
    let revision = git(&remote, &["rev-parse", "refs/heads/dev"]);
    let repeat = sync();
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/dev"]), revision);
    let selector_issue = "Kept a dotfile whose validated selector changed";
    absent(&first, selector_issue);
    absent(&repeat, selector_issue);
    let declaring = ["config.default.toml", "config.toml"]
        .into_iter()
        .find(|name| {
            fs::read_to_string(root.join(name)).is_ok_and(|text| text.contains(source_key))
        })
        .expect("actual declaration written by dots add");
    let cfg = root.join("config.toml");
    let before = read(&cfg);
    let hook = format!(
        "\n[bootstrap.hooks]\npre-dotfiles = '''\ncp \"$HOME/control-config.toml\" {}\nprintf 'changed\\n' > \"$HOME/control-hook-ran\"\n'''\n",
        sh(&root.join(declaring))
    );
    put(&cfg, format!("{before}{hook}"));
    put(
        &root.join("dotfiles/control.toml"),
        "[settings]\nname = 'rejected-synthetic-source'\n",
    );
    let approved = read(&root.join(declaring));
    let alternate = approved.replace(source_key, "dotfiles/control.toml");
    assert_ne!(approved, alternate);
    put(&world.home().join("control-config.toml"), &alternate);
    let control = sync();
    has(&control, selector_issue);
    assert_eq!(read(&world.home().join("control-hook-ran")), "changed\n");
    assert_eq!(read(&root.join(declaring)), alternate);
    assert_eq!(
        git(&remote, &["show", &format!("refs/heads/dev:{declaring}")]),
        approved.trim()
    );
    assert_eq!(fs::read_link(&target).unwrap(), source);
}

#[test]
fn new_flagged_adopted_file_stays_local_with_its_declaration() {
    let mut f = Repo::new();
    f.mise = f.mise_real.clone().expect("real mise on fixture PATH");
    script(&f.tools.join("gitleaks"), FLAG_SCANNER);
    let held = f.world.home().join("withheld.txt");
    let allowed = f.world.home().join("allowed.txt");
    put(&held, "FIXTURE_SCAN_MATCH\n");
    put(&allowed, "allowed content\n");
    dots_zero(f.dots(&f.a, &["add"]).arg(&held).arg(&allowed));
    let declaring = ["config.laptop.toml", "config.toml"]
        .into_iter()
        .find(|name| {
            fs::read_to_string(f.a.join(name))
                .is_ok_and(|text| text.contains("dotfiles/withheld.txt"))
        })
        .expect("dots add wrote a declaration for the flagged file");
    let config = f.a.join(declaring);
    let local = format!("{}\n# unrelated config edit\n", read(&config));
    put(&config, &local);
    f.write(&f.a, "text", "unrelated edit\n");

    let receipt = f.sync(&f.a);
    has(&receipt, "pushed dev");
    has(&receipt, "gitleaks flagged");
    has(&receipt, "dotfiles/withheld.txt:1");
    absent(&receipt, "FIXTURE_SCAN_MATCH");
    let replay = f.replay(&f.a);
    has(&replay, "dotfiles/withheld.txt:1");
    absent(&replay, "FIXTURE_SCAN_MATCH");
    let shared = f.blob("dev", declaring);
    has(&shared, "dotfiles/allowed.txt");
    has(&shared, "# unrelated config edit");
    absent(&shared, "withheld.txt");
    absent(&f.tree(), "withheld.txt");
    assert_eq!(f.blob("dev", "dotfiles/allowed.txt"), "allowed content");
    assert_eq!(f.blob("dev", "text"), "unrelated edit");
    assert_eq!(read(&config), local);
    assert_eq!(
        fs::read_link(&held).unwrap(),
        f.a.join("dotfiles/withheld.txt")
    );
    assert_eq!(read(&held), "FIXTURE_SCAN_MATCH\n");

    put(&f.a.join("dotfiles/withheld.txt"), "safe replacement\n");
    has(&f.sync(&f.a), "pushed dev");
    has(&f.blob("dev", declaring), "dotfiles/withheld.txt");
    assert_eq!(f.blob("dev", "dotfiles/withheld.txt"), "safe replacement");
    f.clean(&f.a);
}

#[test]
fn new_flagged_copy_declaration_stays_local_while_other_edits_share() {
    let mut f = Repo::new();
    f.mise = f.mise_real.clone().expect("real mise on fixture PATH");
    script(&f.tools.join("gitleaks"), FLAG_SCANNER);
    let config = format!(
        "{}\n[dotfiles]\n'~/.copy-target' = {{ source = './copy-source', mode = 'copy' }}\n'~/.allowed-copy' = {{ source = 'fine', mode = 'copy' }}\n",
        f.config
    );
    f.write(&f.a, "config.toml", &config);
    f.write(&f.a, "copy-source", "FIXTURE_SCAN_MATCH\n");
    f.write(&f.a, "fine", "allowed content\n");
    let live = f.sync(&f.a);
    has(&live, "pushed dev");
    has(&live, "gitleaks flagged");
    has(&live, "copy-source:1");
    absent(&live, "FIXTURE_SCAN_MATCH");
    assert_eq!(f.blob("dev", "fine"), "allowed content");
    absent(&f.tree(), "copy-source");
    let shared = f.blob("dev", "config.toml");
    absent(&shared, "copy-target");
    has(&shared, "allowed-copy");
    assert_eq!(read(&f.a.join("config.toml")), config);
    assert_eq!(read(&f.a.join("copy-source")), "FIXTURE_SCAN_MATCH\n");
    assert!(!f.world.home().join(".copy-target").exists());
}

#[test]
fn new_flagged_retarget_restores_the_published_declaration() {
    let mut f = Repo::new();
    f.mise = f.mise_real.clone().expect("real mise on fixture PATH");
    script(&f.tools.join("gitleaks"), FLAG_SCANNER);
    let original = format!(
        "{}\n[dotfiles]\n'~/.retarget' = {{ source = 'original', mode = 'copy' }}\n",
        f.config
    );
    f.write(&f.a, "original", "published bytes\n");
    f.write(&f.a, "config.toml", &original);
    let before = f.commit(&f.a, "Publish original declaration");
    f.git(&f.a, &["push", "origin", "dev"]);

    let retarget = format!(
        "{}\n# unrelated edit\n",
        original.replace("source = 'original'", "source = 'held'")
    );
    f.write(&f.a, "config.toml", &retarget);
    f.write(&f.a, "held", "FIXTURE_SCAN_MATCH\n");
    f.write(&f.a, "fine", "ordinary edit\n");
    let receipt = f.sync(&f.a);
    has(&receipt, "held:1");
    absent(&receipt, "FIXTURE_SCAN_MATCH");
    assert!(
        !f.git(&f.origin, &["rev-list", "--objects", "--all"])
            .lines()
            .any(|line| line.ends_with(" held"))
    );
    assert_eq!(read(&f.a.join("held")), "FIXTURE_SCAN_MATCH\n");
    assert_eq!(read(&f.a.join("config.toml")), retarget);
    assert_ne!(f.remote(), before, "{receipt}");
    let shared = f.blob("dev", "config.toml");
    has(&shared, "source = 'original'");
    has(&shared, "# unrelated edit");
    absent(&shared, "source = 'held'");
    assert_eq!(f.blob("dev", "fine"), "ordinary edit");
}

#[test]
fn new_sync_reports_the_runtime_mise_version_error_once() {
    let f = Repo::new();
    script(&f.mise, "printf '2026.11.7\\n'");
    let output = finish(
        f.dots(&f.a, &["mise", "config", "ls"])
            .env_remove("SETUP_PROFILE"),
    );
    assert_eq!(output.status.code(), Some(2));
    let error = std::str::from_utf8(&output.stderr)
        .unwrap()
        .trim()
        .trim_start_matches("dots: ");
    has(error, "2026.11.7");
    has(error, env!("DOTS_MINIMUM_MISE"));
    for branch in ["dev", "main"] {
        f.git(&f.a, &["switch", branch]);
        let live = f.sync(&f.a);
        assert_eq!(live.matches(error).count(), 1, "{live}");
        assert_eq!(f.replay(&f.a).matches(error).count(), 1);
        absent(&live, "Switched this checkout from main to dev");
        assert_eq!(f.git(&f.a, &["branch", "--show-current"]), branch);
    }
}
