mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};
use support::World;
use toml_edit::DocumentMut;

const TRACKED: &str = "~/.config/app/state";
const TRACKED_RELATIVE: &str = ".config/app/state";
const DECLARATIONS: &str = "config.host-fx.toml";
const HOST: &str = "host-fx";
const EMPTY: &str = r#"{"entries":[],"invalid":[]}"#;
const REPO: &str = "dots-test-fixture/fixture-state";
const HTTPS: &str = "https://github.com/dots-test-fixture/fixture-state.git";
const MACHINE_KEY: &str = "age1tcfkaaftjeax8f0c3hsxcwrhl6nefap2df9v9qvmxy6zjnft8pvqt89uap";
const RECOVERY_KEY: &str = "age19nzy4e29vw3s5qmsupjklz4ze6ycnzdw6w2xf0d9wzh88pdyg42qvvucmv";

struct History {
    world: World,
    bin: PathBuf,
    log: PathBuf,
    origin: String,
}

impl History {
    fn new() -> Self {
        let world = World::new();
        let base = world.root().parent().unwrap();
        let bin = base.join("bin");
        let log = base.join("mise.log");
        write(&log, "", 0o600);
        write(
            &bin.join("mise"),
            include_str!("fixtures/contract-history-mise.sh"),
            0o755,
        );
        write(
            &bin.join("age-keygen"),
            include_str!("fixtures/contract-history-age-keygen.sh"),
            0o755,
        );
        // The native history module uses age to validate recipients. The
        // enrollment scenarios check which public recipients were persisted.
        write(&bin.join("age"), "#!/bin/sh\nexit 0\n", 0o755);
        write(
            &bin.join("git"),
            r#"#!/bin/sh
PATH="$CONTRACT_HISTORY_REAL_PATH"
export PATH
if [ "$#" -eq 8 ] && [ "$1" = "--git-dir" ] && [ "$3" = "ls-remote" ] &&
   [ "$4" = "--exit-code" ] && [ "$5" = "--refs" ] && [ "$6" = "--" ] &&
   [ "$7" = "$CONTRACT_HISTORY_ORIGIN" ] && [ "$8" = "refs/heads/$CONTRACT_HISTORY_BRANCH" ]; then
    exec git ls-remote --exit-code --refs -- "file://$CONTRACT_HISTORY_ACK_REMOTE" "$8"
fi
exec git "$@"
"#,
            0o755,
        );
        fs::write(world.root().join("config.laptop.toml"), "").unwrap();
        fs::write(world.root().join("dots/exceptions.tsv"), "").unwrap();
        let mut fixture = Self {
            world,
            bin,
            log,
            origin: HTTPS.into(),
        };
        fixture.configure(REPO, HTTPS);
        fixture
    }

    fn configure(&mut self, state_repo: &str, origin: &str) {
        self.origin = origin.to_owned();
        fs::write(
            self.world.root().join("config.toml"),
            format!(
                "[vars]\ndots_profiles = \"laptop server\"\ndots_state_repo = {state_repo:?}\n"
            ),
        )
        .unwrap();
    }

    fn dots(&self) -> support::FixtureCommand<'_> {
        let mut command = self.world.dots();
        let real_path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone()).chain(std::env::split_paths(&real_path)),
        )
        .unwrap();
        let fake = self.bin.join("mise");
        let base = self.world.root().parent().unwrap();
        command
            .env("PATH", path)
            .env("DOTS_MISE_BIN", &fake)
            .env("SETUP_MISE_BIN", &fake)
            .env("DOTS_UP_BIN", self.bin.join("absent-renderer"))
            .env("CODEX_HOME", base.join("codex"))
            .env("CLAUDE_CONFIG_DIR", base.join("claude"))
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("MISE_OFFLINE", "1")
            .env("MISE_SYSTEM_CONFIG_DIR", base.join("system"))
            .env("MISE_CEILING_PATHS", base)
            .env("MISE_TRUSTED_CONFIG_PATHS", base)
            .env("CONTRACT_HISTORY_REAL_PATH", real_path)
            .env("CONTRACT_HISTORY_LOG", &self.log)
            .env("CONTRACT_HISTORY_PATHS", EMPTY)
            .env("CONTRACT_HISTORY_STATUS", "{}")
            .env("CONTRACT_HISTORY_ORIGIN", "")
            .env("CONTRACT_HISTORY_BRANCH", "")
            .env(
                "CONTRACT_HISTORY_ACK_REMOTE",
                base.join("remotes/history-acknowledged"),
            )
            .env("CONTRACT_HISTORY_MACHINE_KEY", MACHINE_KEY);
        command
    }

    fn tracked_file(&self) -> PathBuf {
        let path = self.world.home().join(TRACKED_RELATIVE);
        write(&path, "fixture state\n", 0o644);
        path
    }

    fn declarations(&self, keys: &[&str], variant: Option<&str>, encrypt: bool) {
        let scope = variant.map_or_else(String::new, |host| {
            format!(", variants = [{{ profile = {host:?} }}]")
        });
        let mut contents = String::from("[dotfiles]\n");
        for key in keys {
            contents.push_str(&format!(
                "{key:?} = {{ mode = \"track\", encrypt = {encrypt}{scope} }}\n"
            ));
        }
        fs::write(self.world.root().join(DECLARATIONS), contents).unwrap();
    }

    fn inventory(&self, paths: &[&str], variant: Option<&str>) -> String {
        let entries = paths.iter().map(|path| {
            let mut entry = json!({
                "path": path,
                "mode": "track",
                "declared_in": self.world.root().join(DECLARATIONS),
            });
            if let Some(host) = variant {
                entry["variant"] = json!(host);
            }
            entry
        });
        json!({ "entries": entries.collect::<Vec<_>>(), "invalid": [] }).to_string()
    }

    fn enroll(&self, inventory: Option<&str>) -> Output {
        let identity = self.world.home().join(".config/age/keys.txt");
        write(&identity, &format!("{MACHINE_KEY}\n"), 0o600);
        let version = self.dots().arg("--version").output().unwrap();
        let version = String::from_utf8_lossy(&version.stdout);
        let mut command = self.dots();
        // v0.4.0 shipped this callback under `internal`. A git archive of
        // f5c8a0e has no release VERSION file and identifies itself as dev.
        if version.starts_with("dots 0.4.0 (") || version.trim() == "dots dev (archive)" {
            command.arg("internal");
        }
        if let Some(inventory) = inventory {
            command.env("CONTRACT_HISTORY_PATHS", inventory);
        } else {
            command.env_remove("CONTRACT_HISTORY_PATHS");
        }
        command
            .args([
                "enroll-history",
                "--profile",
                "laptop",
                "--machine-id",
                "fx",
                "--identity",
            ])
            .arg(identity)
            .args(["--recipient", RECOVERY_KEY])
            .output()
            .unwrap()
    }

    fn selected(&self) -> support::FixtureCommand<'_> {
        fs::write(
            self.world.root().join("miserc.toml"),
            "env = [\"laptop\", \"host-fx\"]\n",
        )
        .unwrap();
        fs::write(
            self.world.root().join("config.local.toml"),
            format!(
                "[env]\nSETUP_PROFILE = \"laptop\"\nSETUP_MACHINE_ID = \"fx\"\n\
                 [settings]\nhistory.sync = \"manual\"\n\
                 [history.origin]\nurl = {:?}\nbranch = {HOST:?}\n",
                self.origin
            ),
        )
        .unwrap();
        let mut command = self.dots();
        command
            .env("SETUP_PROFILE", "laptop")
            .env("SETUP_MACHINE_ID", "fx");
        command
    }

    fn backup(&self, inventory: &str, status: Value) -> Output {
        let sync = &status["history"]["sync"];
        self.selected()
            .arg("backup")
            .env("CONTRACT_HISTORY_PATHS", inventory)
            .env(
                "CONTRACT_HISTORY_ORIGIN",
                sync["origin"].as_str().unwrap_or_default(),
            )
            .env(
                "CONTRACT_HISTORY_BRANCH",
                sync["branch"].as_str().unwrap_or_default(),
            )
            .env("CONTRACT_HISTORY_STATUS", status.to_string())
            .output()
            .unwrap()
    }

    fn up(&self, inventory: &str) -> Output {
        self.selected()
            .args(["--plain", "up"])
            .env("CONTRACT_HISTORY_PATHS", inventory)
            .output()
            .unwrap()
    }

    fn update_hook_marker(&self) -> PathBuf {
        write(
            &self
                .world
                .root()
                .join("dots/hooks/update.d/50-History-guard"),
            "#!/bin/sh\nprintf 'hook ran\\n' > \"$HOME/history-update-hook\"\n",
            0o755,
        );
        self.world.home().join("history-update-hook")
    }

    fn history_store(&self) -> PathBuf {
        let command = self.dots();
        let state = command
            .get_envs()
            .find_map(|(key, value)| (key == "MISE_STATE_DIR").then_some(value).flatten())
            .expect("guarded history fixture must set MISE_STATE_DIR");
        Path::new(state).join("history/repo.git")
    }

    fn create_store(&self) -> PathBuf {
        let bare = self.world.bare_remote("history-store");
        let acknowledged = self.world.bare_remote("history-acknowledged");
        let store = self.history_store();
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::rename(bare, &store).unwrap();
        // The recorder reports an already acknowledged checkpoint without
        // contacting the fixture's synthetic GitHub endpoint.
        let seed = self.world.root().join("history-seed");
        fs::create_dir(&seed).unwrap();
        for args in [
            &["init", "--initial-branch=main"][..],
            &["commit", "--allow-empty", "-m", "fixture checkpoint"],
        ] {
            expect(
                &self
                    .world
                    .git()
                    .current_dir(&seed)
                    .args(args)
                    .output()
                    .unwrap(),
                0,
                &[],
            );
        }
        expect(
            &self
                .world
                .git()
                .current_dir(&seed)
                .args(["push", "--"])
                .arg(&store)
                .args(["HEAD:refs/heads/main", "HEAD:refs/remotes/origin/setup"])
                .output()
                .unwrap(),
            0,
            &[],
        );
        expect(
            &self
                .world
                .git()
                .current_dir(&seed)
                .args(["push", "--"])
                .arg(&acknowledged)
                .arg(format!("HEAD:refs/heads/{HOST}"))
                .output()
                .unwrap(),
            0,
            &[],
        );
        store
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap()
    }
}

fn write(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn expect(output: &Output, status: i32, words: &[&str]) {
    assert_eq!(output.status.code(), Some(status), "{output:?}");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for word in words {
        assert!(text.contains(word), "missing {word:?} in {output:?}");
    }
}

fn assert_history_calls(fixture: &History, expected: &[&str]) {
    let calls = fixture.log();
    let actions: Vec<_> = calls
        .lines()
        .filter_map(|line| line.strip_prefix("bootstrap dotfiles "))
        .filter(|action| matches!(*action, "save" | "sync"))
        .collect();
    assert_eq!(actions, expected, "{calls}");
}

fn status(store: &Path, origin: &str, branch: &str) -> Value {
    json!({ "history": { "sync": { "origin": origin, "branch": branch }, "history_repo": store } })
}

#[test]
fn unchanged_enrollment_secures_files_and_keeps_recipients_local() {
    let h = History::new();
    let tracked = h.tracked_file();
    let missing = "~/.config/app/missing";
    let paths = [TRACKED, missing];
    h.declarations(&paths, Some(HOST), true);
    let shared_configs = ["config.toml", "config.laptop.toml", DECLARATIONS].map(|name| {
        let path = h.world.root().join(name);
        let before = fs::read(&path).unwrap();
        (path, before)
    });
    expect(&h.enroll(Some(&h.inventory(&paths, Some(HOST)))), 0, &[]);
    assert_eq!(mode(&tracked), 0o600);
    assert!(!h.world.home().join(".config/app/missing").exists());
    let local = fs::read_to_string(h.world.root().join("config.local.toml"))
        .unwrap()
        .parse::<DocumentMut>()
        .unwrap();
    let recipients = local["history"]["encryption"]["recipients"]
        .as_array()
        .unwrap();
    for key in [MACHINE_KEY, RECOVERY_KEY] {
        assert!(
            recipients.iter().any(|value| value.as_str() == Some(key)),
            "{local}"
        );
    }
    for (path, before) in shared_configs {
        assert_eq!(fs::read(&path).unwrap(), before, "{}", path.display());
    }
    assert!(!h.history_store().exists());
    assert_history_calls(&h, &[]);
}

#[test]
fn unchanged_enrollment_defers_when_no_files_are_declared() {
    let h = History::new();
    expect(&h.enroll(Some(EMPTY)), 3, &[HOST, "track"]);

    let tracked = h.tracked_file();
    h.declarations(&[TRACKED], Some("host-other"), true);
    fs::rename(
        h.world.root().join(DECLARATIONS),
        h.world.root().join("config.laptop.toml"),
    )
    .unwrap();
    expect(&h.enroll(None), 3, &[HOST, "track"]);
    assert_eq!(mode(&tracked), 0o644);
    assert_history_calls(&h, &[]);
}

#[test]
fn unchanged_enrollment_rejects_an_unencrypted_entry() {
    let h = History::new();
    let tracked = h.tracked_file();
    h.declarations(&[TRACKED], Some(HOST), false);
    let output = h.enroll(Some(&h.inventory(&[TRACKED], Some(HOST))));
    expect(&output, 2, &[TRACKED, DECLARATIONS, "encrypt"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Prepared"));
    assert_eq!(mode(&tracked), 0o644);
    assert_history_calls(&h, &[]);
}

#[test]
fn unchanged_enrollment_rejects_a_symlinked_tracked_path() {
    let h = History::new();
    let target = h.world.root().with_file_name("outside-home");
    write(&target, "fixture outside HOME\n", 0o644);
    let link = "~/.config/link";
    symlink(&target, h.world.home().join(".config/link")).unwrap();
    h.declarations(&[link], Some(HOST), true);
    expect(
        &h.enroll(Some(&h.inventory(&[link], Some(HOST)))),
        2,
        &["symlink"],
    );
    assert_eq!(mode(&target), 0o644);
    assert_history_calls(&h, &[]);
}

#[test]
fn unchanged_backup_refuses_an_unencrypted_entry_before_saving() {
    let h = History::new();
    h.tracked_file();
    h.declarations(&[TRACKED], Some(HOST), false);
    let output = h.backup(&h.inventory(&[TRACKED], Some(HOST)), json!({}));
    expect(&output, 2, &[TRACKED, DECLARATIONS, "encrypt"]);
    assert_history_calls(&h, &[]);
}

#[test]
fn unchanged_backup_checks_effective_push_urls_and_branch_before_saving() {
    // These GitHub spellings are checked as text by local Git's offline
    // destination resolution. The fake save and sync never contact them.
    let h = History::new();
    let store = h.create_store();
    h.tracked_file();
    h.declarations(&[TRACKED], Some(HOST), true);
    let inventory = h.inventory(&[TRACKED], Some(HOST));
    let other = format!("file://{}", h.world.bare_remote("other").display());
    let git_config = h.world.home().join(".gitconfig");
    let wrong_branch = "host-override";
    let origin_with_newline = format!("{HTTPS}\n");
    for (origin, branch, rewrite, issue) in [
        (HTTPS, HOST, None, None),
        (
            "git@github.com:dots-test-fixture/fixture-state",
            HOST,
            None,
            None,
        ),
        (
            "ssh://git@github.com/dots-test-fixture/fixture-state.git",
            HOST,
            None,
            None,
        ),
        (other.as_str(), HOST, None, Some("destination")),
        (HTTPS, wrong_branch, None, Some("branch")),
        (HTTPS, "host-fx\n", None, Some("branch")),
        (&origin_with_newline, HOST, None, Some("origin")),
        (
            HTTPS,
            HOST,
            Some((format!("url.{other}.pushInsteadOf"), HTTPS)),
            Some("destination"),
        ),
        (
            HTTPS,
            HOST,
            Some((format!("remote.{HTTPS}.pushurl"), other.as_str())),
            Some("destination"),
        ),
    ] {
        fs::write(&h.log, "").unwrap();
        fs::write(&git_config, "").unwrap();
        if let Some((key, value)) = rewrite {
            expect(
                &h.world
                    .git()
                    .args(["config", "--global", &key, value])
                    .output()
                    .unwrap(),
                0,
                &[],
            );
        }
        let result = h.backup(&inventory, status(&store, origin, branch));
        if let Some(word) = issue {
            expect(&result, 2, &[word]);
            assert_history_calls(&h, &[]);
        } else {
            expect(&result, 0, &[]);
            assert_history_calls(&h, &["save", "sync"]);
        }
    }
}

#[test]
fn new_backup_accepts_a_local_state_url_and_checks_every_push_url() {
    let mut h = History::new();
    let store = h.create_store();
    let state = format!("file://{}", h.world.bare_remote("state").display());
    let other = format!("file://{}", h.world.bare_remote("other").display());
    h.configure(&state, &state);
    h.tracked_file();
    h.declarations(&[TRACKED], Some(HOST), true);
    let inventory = h.inventory(&[TRACKED], Some(HOST));
    let effective = status(&store, &state, HOST);
    expect(&h.backup(&inventory, effective.clone()), 0, &[]);
    assert_history_calls(&h, &["save", "sync"]);

    fs::write(&h.log, "").unwrap();
    let key = format!("remote.{state}.pushurl");
    // A non-GitHub state URL matches literally. Git reports both push URLs
    // even when they are identical.
    for _ in 0..2 {
        expect(
            &h.world
                .git()
                .args(["config", "--global", "--add", &key, &state])
                .output()
                .unwrap(),
            0,
            &[],
        );
    }
    expect(&h.backup(&inventory, effective.clone()), 0, &[]);
    assert_history_calls(&h, &["save", "sync"]);

    fs::write(&h.log, "").unwrap();
    for url in [&other, &state] {
        expect(
            &h.world
                .git()
                .args(["config", "--global", "--add", &key, url])
                .output()
                .unwrap(),
            0,
            &[],
        );
    }
    expect(&h.backup(&inventory, effective), 2, &["destination"]);
    assert_history_calls(&h, &[]);
}

#[test]
fn unchanged_up_stops_before_updates_when_the_checkpoint_is_unsafe() {
    let h = History::new();
    let hook = h.update_hook_marker();
    h.tracked_file();
    h.declarations(&[TRACKED], None, false);
    expect(&h.up(&h.inventory(&[TRACKED], None)), 1, &["checkpoint"]);
    assert_history_calls(&h, &[]);
    assert!(!hook.exists());
    assert!(
        !h.log().lines().any(|line| line == "install"),
        "{}",
        h.log()
    );
}

#[test]
fn unchanged_up_saves_encrypted_entries_twice() {
    let h = History::new();
    let hook = h.update_hook_marker();
    h.tracked_file();
    h.declarations(&[TRACKED], None, true);
    expect(&h.up(&h.inventory(&[TRACKED], None)), 0, &[]);
    assert_history_calls(&h, &["save", "save"]);
    assert_eq!(fs::read_to_string(hook).unwrap(), "hook ran\n");
}

#[test]
fn unchanged_enrollment_checks_absolute_declarations_for_home_relative_paths() {
    let h = History::new();
    let tracked = h.tracked_file();
    let key = tracked.to_str().unwrap();
    let inventory = h.inventory(&[TRACKED], Some(HOST));
    h.declarations(&[key], Some(HOST), true);
    expect(&h.enroll(Some(&inventory)), 0, &[]);
    assert_eq!(mode(&tracked), 0o600);

    fs::set_permissions(&tracked, fs::Permissions::from_mode(0o644)).unwrap();
    h.declarations(&[key], Some(HOST), false);
    expect(
        &h.enroll(Some(&inventory)),
        2,
        &[TRACKED, DECLARATIONS, "encrypt"],
    );
    assert_eq!(mode(&tracked), 0o644);
    assert_history_calls(&h, &[]);
}

#[test]
fn unchanged_enrollment_refuses_invalid_history_inventory_but_defers_for_an_empty_list() {
    let h = History::new();
    for inventory in [
        "",
        "{}",
        r#"{"entries":{}}"#,
        "{\"entries\":[]}\n{\"entries\":[]}",
    ] {
        expect(&h.enroll(Some(inventory)), 2, &["tracked history"]);
    }
    expect(&h.enroll(Some(EMPTY)), 3, &[HOST, "track"]);
    assert_history_calls(&h, &[]);
}
