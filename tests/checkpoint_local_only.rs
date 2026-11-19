mod support;

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
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
const DECISION_DONE: &str = "decision_child checked the expected result";

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

    fn seam_command(&self, name: &str, expected: &str) -> FixtureCommand<'_> {
        let test = std::env::current_exe().unwrap();
        let mut command = self.world.command(test);
        self.isolated(&mut command);
        command
            .env("DOTS_TEST_SEAM", name)
            .env("DOTS_TEST_EXPECTED", expected)
            .args(["--ignored", "--exact", "decision_child", "--nocapture"]);
        command
    }

    fn seam(&self, name: &str, expected: &str) {
        run_decision(&mut self.seam_command(name, expected));
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

fn run_decision(command: &mut FixtureCommand<'_>) {
    let output = success(command.output().unwrap());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line == DECISION_DONE),
        "exact decision child did not complete: {output:?}"
    );
}

fn native_mise() -> PathBuf {
    env::var_os("DOTS_TEST_NATIVE_MISE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::split_paths(&env::var_os("PATH").unwrap())
                .filter(|path| path.is_absolute())
                .map(|path| path.join("mise"))
                .find(|path| path.is_file())
                .expect("mise executable on PATH")
        })
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
                Err(_) => "blocked",
                Ok(Checkpoint::Saved) => "saved",
            };
            let _ = run.end(0);
            result
        }
        "source" => match sync::load(&ctx) {
            Loaded::LocalOnly => "local",
            Loaded::Problem(_) => "blocked",
            Loaded::Ready(_) => "shared",
        },
        _ => panic!("unexpected test seam {seam}"),
    };
    assert_eq!(actual, std::env::var("DOTS_TEST_EXPECTED").unwrap());
    println!("\n{DECISION_DONE}");
}

#[test]
fn new_checkpoint_bounds_open_tracked_files() {
    use nix::libc;

    let f = Fixture::new();
    let config = f.world.root().join("config.toml");
    let mut declarations = format!("{CONFIG}\n[dotfiles]\n");
    let mut entries = Vec::new();
    let mut paths = Vec::new();
    for index in 0..128 {
        let name = format!("setting-{index}");
        let target = format!("~/{name}");
        declarations.push_str(&format!(
            "'{target}' = {{ mode = 'track', encrypt = true }}\n"
        ));
        f.write(&name, "fixture payload\n");
        let path = f.world.home().join(name);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        paths.push(path);
        entries.push(serde_json::json!({"mode": "track", "path": target, "declared_in": config}));
    }
    fs::write(config, declarations).unwrap();
    f.write(
        "shared",
        &serde_json::json!({"entries": entries, "invalid": []}).to_string(),
    );
    let mut child = f.seam_command("checkpoint", "saved");
    // Only lower the descriptor limit in the forked fixture process.
    unsafe {
        child.pre_exec(|| {
            let mut limit = std::mem::MaybeUninit::uninit();
            if libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut limit = limit.assume_init();
            limit.rlim_cur = limit.rlim_cur.min(64);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    run_decision(&mut child);
    assert!(
        paths
            .iter()
            .all(|path| fs::metadata(path).unwrap().permissions().mode() & 0o777 == 0o600)
    );
    let log = fs::read_to_string(f.world.home().join("history-calls")).unwrap();
    assert_eq!(log.lines().filter(|line| *line == "save").count(), 1);
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
        ("shared", "[[],[]]"),
        ("shared", r#"{"entries":[],"invalid":[["~/setting"]]}"#),
        (
            "local",
            r#"{"entries":[{"mode":"track-local","path":"~/setting"}],"invalid":[]}"#,
        ),
        ("local", invalid),
        ("local", r#"{"entries":null,"invalid":[]}"#),
        ("local", "[[],[]]"),
    ] {
        f.write(scope, output);
        f.seam("checkpoint", "blocked");
        f.write(scope, EMPTY);
    }
    f.write("local-failure", "");
    f.seam("checkpoint", "blocked");
    fs::remove_file(f.world.home().join("local-failure")).unwrap();
    f.write("shared", invalid);
    let inactive = r#"{"target":"~/setting","mode":"track","state":"tracked","reason":"no variant matches this machine: host-other"}"#;
    let active = r#"{"target":"~/setting","mode":"track","state":"tracked"}"#;
    for (status, expected) in [
        (format!(r#"{{"files":[{inactive}]}}"#), "empty"),
        (format!(r#"{{"files":[{active}]}}"#), "blocked"),
        (format!(r#"{{"files":[{inactive},{active}]}}"#), "blocked"),
        (format!("[[{inactive}]]"), "blocked"),
        (
            r#"{"files":[["~/setting","track","tracked","no variant matches this machine: host-other"]]}"#.to_owned(),
            "blocked",
        ),
        (format!(r#"{{"files":[{}]}}"#, inactive.replace("tracked", "applied")), "blocked"),
        (r#"{"files":[]}"#.to_owned(), "blocked"),
    ] {
        f.write("status", &status);
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
fn new_checkpoint_preserves_mise_notices_and_honors_global_override() {
    let f = Fixture::new();
    let mise = native_mise();
    let state = f.world.base().join("mise/state");
    let stores = [state.join("history"), state.join("history-local/history")];
    let notice = "fixture pending watcher notice\n";
    for store in &stores {
        fs::create_dir_all(store).unwrap();
        fs::write(store.join("notices"), notice).unwrap();
    }
    let config = f.world.home().join("alternate.toml");
    let checkpoint = |expected| {
        run_decision(
            f.seam_command("checkpoint", expected)
                .env("DOTS_TEST_NATIVE_MISE", &mise)
                .env("MISE_GLOBAL_CONFIG_FILE", &config),
        );
        for store in &stores {
            assert_eq!(fs::read_to_string(store.join("notices")).unwrap(), notice);
        }
    };
    f.write("setting", "fixture payload\n");
    fs::write(&config, "[dotfiles]\n'~/setting' = { mode = 'track' }\n").unwrap();
    checkpoint("blocked");

    // Native paths marks a symlinked ancestor invalid before resolving the
    // variant. The targeted status proof distinguishes the active host.
    symlink(f.world.home(), f.world.home().join("linked")).unwrap();
    for (host, expected) in [("host-other", "empty"), ("host-fixture", "blocked")] {
        fs::write(
            &config,
            format!("[dotfiles]\n'~/linked/setting' = {{ mode = 'track', variants = [{{ profile = '{host}' }}] }}\n"),
        )
        .unwrap();
        checkpoint(expected);
    }
    let calls = fs::read_to_string(f.world.home().join("history-calls")).unwrap();
    assert!(calls.lines().any(|line| line == "status"), "{calls}");

    fs::write(&config, "").unwrap();
    checkpoint("empty");
    for store in &stores {
        let repo = store.join("repo.git");
        fs::create_dir(&repo).unwrap();
        checkpoint("blocked");
        fs::remove_dir(repo).unwrap();
    }

    // Prove these are consumable watcher notices with each native dispatch.
    for (store, scope) in stores.iter().zip([None, Some("--local")]) {
        let mut command = f.world.command(&mise);
        f.isolated(&mut command);
        let output = success(
            command
                .args(["bootstrap", "dotfiles"])
                .args(scope)
                .args(["paths", "--json"])
                .output()
                .unwrap(),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(notice.trim()),
            "{output:?}"
        );
        assert!(!store.join("notices").exists());
    }
}

#[test]
fn new_checkpoint_skips_after_native_install_records_trust() {
    let f = Fixture::new();
    let mise = native_mise();
    let home = f.world.home();
    let root = home.join(".config/mise");
    let state = f.world.base().join("mise/state");
    let trust = state.join("trusted-configs");
    let fresh = |command: &mut FixtureCommand<'_>| {
        f.isolated(command);
        command
            .current_dir(home)
            .env("DOTS_MISE_BIN", &mise)
            .env("DOTS_ROOT", &root)
            .env("MISE_CONFIG_DIR", &root)
            .env_remove("MISE_TRUSTED_CONFIG_PATHS")
            .env_remove("SETUP_PROFILE")
            .env_remove("SETUP_MACHINE_ID");
    };
    let mut init = f.world.command(env!("CARGO_BIN_EXE_dots"));
    fresh(&mut init);
    success(init.arg("init").output().unwrap());
    let checkpoint = |expected| {
        let mut command = f.seam_command("checkpoint", expected);
        fresh(&mut command);
        run_decision(&mut command);
    };
    checkpoint("empty");
    assert!(!trust.exists());

    let mut install = f.world.command(&mise);
    fresh(&mut install);
    success(install.current_dir(&root).arg("install").output().unwrap());
    let records: Vec<_> = fs::read_dir(&trust)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(fs::read_link(&records[0]).unwrap(), home);
    checkpoint("empty");
    assert_eq!(fs::read_link(&records[0]).unwrap(), home);

    let unrelated = home.join("unrelated/mise.toml");
    fs::create_dir(unrelated.parent().unwrap()).unwrap();
    fs::write(&unrelated, "# Synthetic project config\n").unwrap();
    let mut trust_project = f.world.command(&mise);
    fresh(&mut trust_project);
    success(
        trust_project
            .env("MISE_PARANOID", "true")
            .arg("trust")
            .arg(&unrelated)
            .output()
            .unwrap(),
    );
    let sidecar = fs::read_dir(&trust)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "hash"))
        .expect("native mise did not write a content trust sidecar");
    let hash = fs::read(&sidecar).unwrap();
    assert_eq!(hash.len(), 64);
    checkpoint("empty");
    assert_eq!(fs::read(&sidecar).unwrap(), hash);
    assert!(!state.join("history/repo.git").exists());

    let config = root.join("config.toml");
    let original = fs::read_to_string(&config).unwrap();
    f.write("setting", "fixture payload\n");
    fs::write(
        &config,
        format!("{original}\n[dotfiles]\n'~/setting' = {{ mode = 'track' }}\n"),
    )
    .unwrap();
    checkpoint("blocked");
    fs::write(config, original).unwrap();

    for uncertain in [
        trust.join("unsupported.hash"),
        state.join("vars.toml"),
        state.join("ignored-configs"),
    ] {
        fs::write(&uncertain, "").unwrap();
        checkpoint("blocked");
        fs::remove_file(uncertain).unwrap();
    }
}

#[test]
fn new_checkpoint_keeps_the_merged_save_path_for_established_history() {
    let f = Fixture::new();
    let state = f.world.base().join("mise/state/history");
    let store = state.join("repo.git");
    fs::create_dir_all(&state).unwrap();
    success(
        f.world
            .git()
            .args(["init", "--bare"])
            .arg(&store)
            .output()
            .unwrap(),
    );
    let declaration = f.world.root().join("config.toml");
    fs::write(
        &declaration,
        format!("{CONFIG}\n[dotfiles]\n'~/setting' = {{ mode = 'track', encrypt = true }}\n"),
    )
    .unwrap();
    f.write("setting", "fixture payload\n");
    f.write(
        "shared",
        &serde_json::json!({
            "entries": [{"mode": "track", "path": "~/setting", "declared_in": declaration}],
            "invalid": [{"path": "~/inactive"}],
        })
        .to_string(),
    );
    f.write(
        "status",
        r#"{"files":[{"target":"~/inactive","mode":"track","state":"tracked","reason":"no variant matches this machine: host-other"}]}"#,
    );

    let local = f.world.base().join("mise/state/history-local/history");
    fs::create_dir_all(&local).unwrap();
    for scope in [&state, &local] {
        fs::write(scope.join("notices"), "queued note\n").unwrap();
    }

    f.seam("checkpoint", "saved");
    let calls = fs::read_to_string(f.world.home().join("history-calls")).unwrap();
    assert_eq!(
        calls.lines().filter(|call| *call == "save").count(),
        1,
        "{calls}"
    );
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
        let expected = match marker {
            "dots-sync-pending" | "dots-sync-write.json" => "shared",
            _ => "blocked",
        };
        f.seam("source", expected);
        fs::remove_file(path).unwrap();
    }
    f.git(&["switch", "--detach"]);
    f.seam("source", "blocked");
    f.git(&["switch", "dev"]);
    let jj = root.join(".jj");
    fs::create_dir(&jj).unwrap();
    f.seam("source", "blocked");
    fs::remove_dir(jj).unwrap();
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
    f.seam("source", "shared");
    fs::write(&conf, CONFIG).unwrap();
    f.seam("source", "shared");
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
fn new_local_only_sync_skips_the_shared_pipeline_on_main_or_old_git() {
    for old in [false, true] {
        let f = Fixture::new();
        f.seed();
        if !old {
            f.git(&["branch", "-m", "main"]);
        }
        let root = f.world.root();
        fs::write(root.join("config.toml"), format!("{CONFIG}# local edit\n")).unwrap();
        let status = f.git(&["status", "--porcelain"]);
        let refs = f.git(&["show-ref"]);
        let index = fs::read(root.join(".git/index")).unwrap();

        let bin = f.world.home().join(".local/bin");
        let git = bin.join("git");
        let real = git.canonicalize().unwrap();
        fs::remove_file(&git).unwrap();
        executable(
            &git,
            r#"#!/bin/sh
set -eu
for argument do
    case "$argument" in
        --version)
            if [ "$FIXTURE_OLD_GIT" = 1 ]; then
                printf 'old\n' > "$HOME/fixture-git-version"
                printf 'git version 2.42.0\n'
                exit 0
            fi ;;
        fetch|push|ls-remote|clone|pull|commit|write-tree|update-index)
            printf '%s\n' "$argument" >> "$HOME/pipeline-ran"
            exit 88 ;;
    esac
done
exec "$FIXTURE_REAL_GIT" "$@"
"#,
        );
        for tool in ["mise", "gitleaks"] {
            executable(
                &bin.join(tool),
                "#!/bin/sh\nprintf 'unexpected child\n' >> \"$HOME/pipeline-ran\"\nexit 88\n",
            );
        }

        let sync = |args: &[&str]| {
            let mut command = f.world.dots();
            f.isolated(&mut command);
            success(
                command
                    .args(args)
                    .env("FIXTURE_REAL_GIT", &real)
                    .env("FIXTURE_OLD_GIT", if old { "1" } else { "0" })
                    .output()
                    .unwrap(),
            )
        };
        let live = sync(&["--plain", "sync"]);
        assert!(live.stderr.is_empty(), "{live:?}");
        let receipt = String::from_utf8(live.stdout).unwrap();
        assert_eq!(receipt.lines().count(), 1, "{receipt}");
        let root_argument = fs::canonicalize(root).unwrap();
        assert!(
            receipt.contains("local-only")
                && receipt.contains(&format!(
                    "git -C '{}' remote add origin URL",
                    root_argument.display()
                )),
            "{receipt}"
        );
        if old {
            assert_eq!(
                fs::read(f.world.home().join("fixture-git-version")).unwrap(),
                b"old\n"
            );
        }
        let replay = sync(&["--plain", "sync", "--last"]);
        assert!(
            String::from_utf8(replay.stdout)
                .unwrap()
                .contains(receipt.trim())
        );
        assert!(!f.world.home().join("pipeline-ran").exists());
        assert_eq!(f.git(&["show-ref"]), refs);
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(f.git(&["status", "--porcelain"]), status);

        let remote = format!("file://{}", f.world.bare_remote("origin").display());
        f.git(&["remote", "add", "origin", &remote]);
        let shared = String::from_utf8(sync(&["--plain", "sync"]).stdout).unwrap();
        let refusal = if old {
            "Git 2.43"
        } else {
            "HEAD on a dev branch"
        };
        assert!(shared.contains(refusal), "{shared}");
        assert!(!shared.contains("local-only"), "{shared}");
        assert!(!f.world.home().join("pipeline-ran").exists());
        assert_eq!(f.git(&["show-ref"]), refs);
    }
}

#[test]
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
# Model notice output from the live scope, including targeted status and an
# empty local inventory.
case "$scope" in
    local) queue="$MISE_STATE_DIR/history-local/history/notices" ;;
    *) queue="$MISE_STATE_DIR/history/notices" ;;
esac
[ ! -f "$queue" ] || cat "$queue" >&2
cat "$HOME/$scope"
[ ! -f "$HOME/$scope-failure" ]
"#;
