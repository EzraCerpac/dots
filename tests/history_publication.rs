mod support;

#[path = "../src/history/publication.rs"]
mod publication;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dots_cli::{Fail, config};
use support::World;

const BRANCH: &str = "host-fixture";
const REMOTE_REF: &str = "refs/heads/host-fixture";
const UNSCOPED: &str = "home/fixture/payload";
const HOST_SCOPED: &str = "home@host-fixture/fixture/payload";
const PAYLOAD: &str = "synthetic checkpoint publication marker\n";

struct History {
    world: World,
    store: PathBuf,
    remote: PathBuf,
    origin: String,
    recipient: String,
}

impl History {
    fn new() -> Self {
        let world = World::new();
        let base = world.root().parent().unwrap().to_owned();
        let store = base.join("mise-state/history/repo.git");
        let remote = base.join("state-origin.git");
        let origin = format!("file://{}", remote.display());
        let mut history = Self {
            world,
            store,
            remote,
            origin,
            recipient: String::new(),
        };
        for dir in [
            "mise-data",
            "mise-state",
            "mise-cache",
            "mise-system",
            "mise-installs",
            "mise-plugins",
            "mise-shims",
            "codex",
            "claude",
            "cargo",
            "rustup",
            "tmp",
            "git-template",
        ] {
            fs::create_dir(base.join(dir)).unwrap();
        }
        let mut git = history.git(&history.remote);
        ok(git.args(["init", "--bare", "--initial-branch=main"]));
        let key = history.world.home().join("throwaway-age-key");
        ok(history.command("age-keygen").arg("-o").arg(&key));
        history.recipient = text(ok(history.command("age-keygen").arg("-y").arg(&key)));
        let live = history.world.home().join("fixture/payload");
        fs::create_dir(live.parent().unwrap()).unwrap();
        fs::write(&live, PAYLOAD).unwrap();
        fs::set_permissions(&live, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(history.world.root().join("config.laptop.toml"), "").unwrap();
        fs::write(
            history.world.root().join("config.local.toml"),
            format!(
                "[env]\nSETUP_PROFILE = \"laptop\"\nSETUP_MACHINE_ID = \"fixture\"\n\
                 [settings.history]\nsync = \"manual\"\nallow_plaintext_history = false\n\
                 [history.origin]\nurl = {:?}\nbranch = {:?}\n",
                history.origin, BRANCH,
            ),
        )
        .unwrap();
        history
    }

    fn configure(&self, encrypted: Option<bool>, host_scoped: bool) {
        let mut config = format!(
            "[vars]\ndots_profiles = \"laptop\"\ndots_state_repo = {:?}\n\
             [settings.age]\nidentity_files = [\"~/throwaway-age-key\"]\n\
             [history.encryption]\nrecipients = [{:?}]\n",
            self.origin, self.recipient,
        );
        if let Some(encrypted) = encrypted {
            let variant = if host_scoped {
                ", variants = [{ profile = \"host-fixture\" }]"
            } else {
                ""
            };
            config.push_str(&format!(
                "[dotfiles]\n\"~/fixture/payload\" = {{ mode = \"track\", encrypt = {encrypted}{variant} }}\n",
            ));
        }
        fs::write(self.world.root().join("config.toml"), config).unwrap();
    }

    fn isolate(&self, cmd: &mut Command) {
        let base = self.world.root().parent().unwrap();
        for (key, dir) in [
            ("MISE_DATA_DIR", "mise-data"),
            ("MISE_STATE_DIR", "mise-state"),
            ("MISE_CACHE_DIR", "mise-cache"),
            ("MISE_SYSTEM_CONFIG_DIR", "mise-system"),
            ("MISE_INSTALLS_DIR", "mise-installs"),
            ("MISE_PLUGINS_DIR", "mise-plugins"),
            ("MISE_SHIMS_DIR", "mise-shims"),
            ("CODEX_HOME", "codex"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("CARGO_HOME", "cargo"),
            ("RUSTUP_HOME", "rustup"),
            ("TMPDIR", "tmp"),
            ("GIT_TEMPLATE_DIR", "git-template"),
        ] {
            cmd.env(key, base.join(dir));
        }
        cmd.env("MISE_CONFIG_DIR", self.world.root())
            .env("MISE_TRUSTED_CONFIG_PATHS", self.world.root())
            .env("MISE_CEILING_PATHS", base)
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_AUTO_UPDATE", "0")
            .env("MISE_OFFLINE", "1")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null());
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .envs(
                self.world
                    .git()
                    .get_envs()
                    .filter_map(|(k, v)| v.map(|v| (k, v))),
            )
            .current_dir(self.world.root());
        self.isolate(&mut cmd);
        cmd
    }

    fn dots(&self) -> Command {
        let mut cmd = self.world.dots();
        self.isolate(&mut cmd);
        cmd
    }

    fn observed_backup(&self) -> (Command, PathBuf) {
        let base = self.world.root().parent().unwrap();
        let bin = base.join("observe-git");
        fs::create_dir(&bin).unwrap();
        let git = bin.join("git");
        fs::write(
            &git,
            r#"#!/bin/sh
case " $* " in
    *" ls-remote "*" --get-url "*) ;;
    *" ls-remote "*) printf 'ls-remote\n' >> "$FIXTURE_GIT_LOG" ;;
    *" fetch "*) printf 'fetch\n' >> "$FIXTURE_GIT_LOG" ;;
    *" push "*) printf 'push\n' >> "$FIXTURE_GIT_LOG" ;;
esac
exec "$FIXTURE_REAL_GIT" "$@"
"#,
        )
        .unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
        let real_git = text(ok(self.command("sh").args(["-c", "command -v git"])));
        let original_path = std::env::var_os("PATH").unwrap();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&original_path)))
                .unwrap();
        let log = base.join("git-observations");
        let mut cmd = self.dots();
        cmd.arg("backup")
            .env("PATH", path)
            .env("FIXTURE_REAL_GIT", real_git)
            .env("FIXTURE_GIT_LOG", &log);
        (cmd, log)
    }

    fn git(&self, repo: &Path) -> Command {
        let mut cmd = self.world.git();
        self.isolate(&mut cmd);
        cmd.arg("--git-dir").arg(repo);
        cmd
    }

    fn mise(&self, operation: &str) -> Command {
        let mut cmd = self.command("mise");
        cmd.args([
            "-E",
            "laptop,host-fixture",
            "bootstrap",
            "dotfiles",
            operation,
        ]);
        cmd
    }

    fn save(&self) -> String {
        ok(self.mise("save").env("GIT_ALLOW_PROTOCOL", ""));
        self.tip(&self.store, "refs/heads/main")
    }

    fn tip(&self, repo: &Path, name: &str) -> String {
        text(ok(self.git(repo).args(["rev-parse", "--verify", name])))
    }

    fn object(&self, repo: &Path, checkpoint: &str, path: &str) -> Vec<u8> {
        ok(self
            .git(repo)
            .args(["show", &format!("{checkpoint}:{path}")]))
        .stdout
    }

    fn preflight(&self) -> Result<(), Fail> {
        publication::preflight(
            || Ok(self.git(&self.store)),
            |mut cmd| {
                cmd.output()
                    .map_err(|_| Fail::new(1, "fixture Git did not run"))
            },
            BRANCH,
            &self.origin,
            &self.tip(&self.store, "refs/heads/main"),
        )
    }

    fn empty_remote(&self, absent: &str) {
        assert!(
            ok(self.git(&self.remote).arg("for-each-ref"))
                .stdout
                .is_empty()
        );
        assert!(
            !self
                .git(&self.remote)
                .args(["cat-file", "-e", absent])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    fn assert_refusal(&self, old: &str, path: &str) {
        let refs = ok(self.git(&self.store).arg("for-each-ref")).stdout;
        let before = ok(self.git(&self.store).args(["count-objects", "-v"])).stdout;
        let refusal = self.preflight().unwrap_err();
        assert_ne!(refusal.status, 0);
        assert!(
            refusal.diagnostic.contains(path) && refusal.diagnostic.contains(old),
            "{refusal}"
        );
        assert!(refusal.diagnostic.contains("separate MISE_STATE_DIR"));
        assert!(!refusal.diagnostic.contains(PAYLOAD.trim()));
        assert_eq!(refs, ok(self.git(&self.store).arg("for-each-ref")).stdout);
        assert_eq!(
            before,
            ok(self.git(&self.store).args(["count-objects", "-v"])).stdout
        );
        self.empty_remote(old);
    }
}

fn ok(cmd: &mut Command) -> Output {
    let result = cmd.output().unwrap();
    assert!(result.status.success(), "{cmd:?}\n{result:?}");
    result
}

fn text(output: Output) -> String {
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

#[test]
fn new_clean_delta_leaves_publication_to_native_sync() {
    let history = History::new();
    history.configure(Some(true), true);
    let local = history.save();
    assert!(
        history
            .object(&history.store, &local, HOST_SCOPED)
            .starts_with(b"mise-encrypted-file-v1\n")
    );
    history.preflight().unwrap();
    history.empty_remote(&local);
    ok(history.dots().arg("backup"));
    assert_eq!(history.tip(&history.remote, REMOTE_REF), local);
    let status: serde_json::Value =
        serde_json::from_slice(&ok(history.mise("status").arg("--json")).stdout).unwrap();
    assert!(status["history"]["sync"]["last_publish"].is_string());
}

#[test]
fn new_removed_declaration_does_not_hide_the_old_payload() {
    let history = History::new();
    history.configure(Some(false), true);
    let old = history.save();
    history.configure(None, false);
    let tip = history.save();
    assert_ne!(tip, old);
    assert_eq!(
        history.object(&history.store, &old, HOST_SCOPED),
        PAYLOAD.as_bytes()
    );
    history.assert_refusal(&old, HOST_SCOPED);
}

#[test]
fn new_encryption_in_the_same_slot_does_not_hide_the_old_payload() {
    let history = History::new();
    history.configure(Some(false), true);
    let old = history.save();
    history.configure(Some(true), true);
    let tip = history.save();
    assert!(
        history
            .object(&history.store, &tip, HOST_SCOPED)
            .starts_with(b"mise-encrypted-file-v1\n")
    );
    history.assert_refusal(&old, HOST_SCOPED);
}

#[test]
fn new_host_variant_does_not_hide_the_old_unscoped_payload() {
    let history = History::new();
    history.configure(Some(false), false);
    let old = history.save();
    history.configure(Some(true), true);
    let tip = history.save();
    assert!(
        history
            .object(&history.store, &tip, HOST_SCOPED)
            .starts_with(b"mise-encrypted-file-v1\n")
    );
    assert_eq!(
        history.object(&history.store, &old, UNSCOPED),
        PAYLOAD.as_bytes()
    );
    history.assert_refusal(&old, UNSCOPED);
}

#[test]
fn new_already_on_target_is_excluded_but_native_encryption_audit_still_refuses() {
    let history = History::new();
    history.configure(Some(false), true);
    let old = history.save();
    // This commits the synthetic legacy payload to the disposable destination.
    ok(history
        .git(&history.store)
        .args(["push", &history.origin, &format!("{old}:{REMOTE_REF}")]));
    history.configure(Some(true), true);
    let tip = history.save();
    history.preflight().unwrap();
    assert_eq!(history.tip(&history.remote, REMOTE_REF), old);
    let native = history.mise("sync").output().unwrap();
    assert!(!native.status.success());
    assert!(String::from_utf8_lossy(&native.stderr).contains("unencrypted or invalid version"));
    assert_eq!(history.tip(&history.remote, REMOTE_REF), old);
    assert!(
        !history
            .git(&history.remote)
            .args(["cat-file", "-e", &tip])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn new_plaintext_on_a_different_remote_ref_is_new_to_the_target_branch() {
    let history = History::new();
    history.configure(Some(false), false);
    let old = history.save();
    ok(history.git(&history.store).args([
        "push",
        &history.origin,
        &format!("{old}:refs/heads/other-host"),
    ]));
    history.configure(None, false);
    history.save();
    let refusal = history.preflight().unwrap_err();
    assert!(refusal.diagnostic.contains(UNSCOPED) && refusal.diagnostic.contains(&old));
    assert_eq!(history.tip(&history.remote, "refs/heads/other-host"), old);
    assert!(
        !history
            .git(&history.remote)
            .args(["rev-parse", "--verify", REMOTE_REF])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn new_magic_prefix_does_not_make_a_plaintext_blob_an_envelope() {
    let history = History::new();
    let live = history.world.home().join("fixture/payload");
    fs::write(live, format!("mise-encrypted-file-v1\n{PAYLOAD}")).unwrap();
    history.configure(Some(false), true);
    let old = history.save();
    history.configure(None, false);
    history.save();
    history.assert_refusal(&old, HOST_SCOPED);
}

#[test]
fn new_unscoped_config_track_uses_the_checkpoint_manifest() {
    let history = History::new();
    let live = history.world.root().join("fixture/payload");
    fs::create_dir(live.parent().unwrap()).unwrap();
    fs::write(&live, PAYLOAD).unwrap();
    history.configure(Some(false), false);
    let config = history.world.root().join("config.toml");
    let input = fs::read_to_string(&config)
        .unwrap()
        .replace("\"~/fixture/payload\"", &format!("{:?}", live));
    fs::write(config, input).unwrap();
    let old = history.save();
    assert_eq!(
        history.object(&history.store, &old, "config/fixture/payload"),
        PAYLOAD.as_bytes()
    );
    history.configure(None, false);
    history.save();
    history.assert_refusal(&old, "config/fixture/payload");
}

#[test]
fn new_changed_destination_tip_refuses() {
    let history = History::new();
    history.configure(Some(true), true);
    let tip = history.save();
    let other = history.world.root().parent().unwrap().join("second.git");
    ok(history.git(&other).args(["init", "--bare"]));
    let other_origin = format!("file://{}", other.display());
    ok(history
        .git(&history.store)
        .args(["push", &other_origin, &format!("{tip}:{REMOTE_REF}")]));
    let observed = std::cell::Cell::new(false);
    let err = publication::preflight(
        || Ok(history.git(&history.store)),
        |mut cmd| {
            if cmd.get_args().any(|arg| arg == "ls-remote") && observed.replace(true) {
                cmd = history.git(&history.store);
                cmd.args([
                    "ls-remote",
                    "--refs",
                    "--exit-code",
                    "--",
                    &other_origin,
                    REMOTE_REF,
                ]);
            }
            cmd.output()
                .map_err(|_| Fail::new(1, "fixture Git did not run"))
        },
        BRANCH,
        &history.origin,
        &tip,
    )
    .unwrap_err();
    assert!(err.diagnostic.contains("changed during verification"));
    history.empty_remote(&tip);
}

#[test]
fn new_backup_default_false_refuses_before_native_sync() {
    let history = History::new();
    history.configure(Some(false), false);
    let old = history.save();
    history.configure(Some(true), true);
    let config = history.world.root().join("config.local.toml");
    let input = fs::read_to_string(&config)
        .unwrap()
        .replace("allow_plaintext_history = false\n", "");
    fs::write(config, input).unwrap();
    let tip = history.save();

    let (mut backup, trace) = history.observed_backup();
    let refused = backup.output().unwrap();
    assert!(!refused.status.success(), "{refused:?}");
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        diagnostic.contains(UNSCOPED) && diagnostic.contains(&old),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("separate MISE_STATE_DIR"));
    assert!(!diagnostic.contains(PAYLOAD.trim()));
    let git_trace = fs::read_to_string(trace).unwrap();
    assert!(git_trace.contains("ls-remote"));
    assert!(!git_trace.contains("push") && !git_trace.contains("fetch"));
    assert_eq!(history.tip(&history.store, "refs/heads/main"), tip);
    history.empty_remote(&old);
}

#[test]
fn new_backup_explicit_effective_opt_in_leaves_publication_to_native_sync() {
    let history = History::new();
    history.configure(Some(false), false);
    let old = history.save();
    history.configure(Some(true), true);

    let refused = history.dots().arg("backup").output().unwrap();
    assert!(!refused.status.success(), "{refused:?}");
    history.empty_remote(&old);
    ok(history
        .dots()
        .arg("backup")
        .env("MISE_HISTORY_ALLOW_PLAINTEXT_HISTORY", "true"));
    let published = history.tip(&history.remote, REMOTE_REF);
    assert_eq!(
        history.object(&history.remote, &old, UNSCOPED),
        PAYLOAD.as_bytes()
    );
    ok(history
        .git(&history.remote)
        .args(["merge-base", "--is-ancestor", &old, &published]));
    let status: serde_json::Value =
        serde_json::from_slice(&ok(history.mise("status").arg("--json")).stdout).unwrap();
    assert!(status["history"]["sync"]["last_publish"].is_string());
}

#[test]
fn new_allow_plaintext_requires_a_successful_exact_boolean() {
    use std::os::unix::process::ExitStatusExt as _;
    let output = |raw, stdout: &[u8]| Output {
        status: std::process::ExitStatus::from_raw(raw),
        stdout: stdout.to_vec(),
        stderr: b"private configuration diagnostic".to_vec(),
    };
    for (raw, bytes) in [
        (0, b"".as_slice()),
        (0, b"TRUE\n"),
        (0, b"true\nfalse\n"),
        (256, b"true\n"),
    ] {
        let error = publication::allows_plaintext(output(raw, bytes)).unwrap_err();
        assert_ne!(error.status, 0);
        assert!(error.diagnostic.contains("history.allow_plaintext_history"));
        assert!(
            !error
                .diagnostic
                .contains("private configuration diagnostic")
        );
    }
    assert!(publication::allows_plaintext(output(0, b"true\n")).unwrap());
    assert!(!publication::allows_plaintext(output(0, b"false\n")).unwrap());
}

#[test]
fn new_recovery_can_publish_from_separate_state_without_rewriting_history() {
    let history = History::new();
    history.configure(Some(false), false);
    let old = history.save();
    history.configure(Some(true), true);
    let tip = history.save();
    history.assert_refusal(&old, UNSCOPED);

    let base = history.world.root().parent().unwrap();
    let clean_config = base.join("fresh-config");
    let clean_state = base.join("fresh-state");
    let clean_remote = base.join("fresh-origin.git");
    let clean_origin = format!("file://{}", clean_remote.display());
    fs::create_dir(&clean_config).unwrap();
    fs::create_dir(&clean_state).unwrap();
    ok(history.git(&clean_remote).args(["init", "--bare"]));
    for name in ["config.toml", "config.local.toml"] {
        let source = fs::read_to_string(history.world.root().join(name)).unwrap();
        fs::write(
            clean_config.join(name),
            source.replace(&history.origin, &clean_origin),
        )
        .unwrap();
    }
    ok(history
        .dots()
        .arg("backup")
        .current_dir(&clean_config)
        .env("DOTS_ROOT", &clean_config)
        .env("MISE_CONFIG_DIR", &clean_config)
        .env("MISE_TRUSTED_CONFIG_PATHS", &clean_config)
        .env("MISE_STATE_DIR", &clean_state));
    let published = history.tip(&clean_remote, REMOTE_REF);
    assert!(
        history
            .object(&clean_remote, &published, HOST_SCOPED)
            .starts_with(b"mise-encrypted-file-v1\n")
    );
    assert!(
        !history
            .git(&clean_remote)
            .args(["cat-file", "-e", &old])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(history.tip(&history.store, "refs/heads/main"), tip);
    assert_eq!(
        history.object(&history.store, &old, UNSCOPED),
        PAYLOAD.as_bytes()
    );
    history.empty_remote(&old);
}

#[test]
fn new_backup_refusal_names_escaped_path_and_checkpoint() {
    let history = History::new();
    let source = history.world.home().join("fixture/payload");
    let quoted_path = "fixture/payload\"quoted";
    fs::rename(source, history.world.home().join(quoted_path)).unwrap();
    history.configure(Some(false), false);
    let config = history.world.root().join("config.toml");
    let updated = fs::read_to_string(&config).unwrap().replace(
        "\"~/fixture/payload\"",
        &format!("{:?}", format!("~/{quoted_path}")),
    );
    fs::write(config, updated).unwrap();
    let old = history.save();
    history.configure(None, false);
    let refused = history.dots().arg("backup").output().unwrap();
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8(refused.stderr).unwrap();
    let escaped = format!("home/{quoted_path}");
    let expected =
        format!("plaintext or invalid encrypted payload at {escaped:?} in checkpoint {old}.");
    let line = stderr
        .lines()
        .find(|line| line.contains(&expected))
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(line.contains("run dots backup with a separate MISE_STATE_DIR."));
    println!("{line}");
    history.empty_remote(&old);
}

#[test]
fn new_current_unencrypted_declaration_stops_before_preflight() {
    let history = History::new();
    history.configure(Some(false), true);
    let old = history.save();
    let (mut backup, trace) = history.observed_backup();
    let refused = backup.output().unwrap();
    assert_eq!(refused.status.code(), Some(2));
    let trace = fs::read_to_string(trace).unwrap_or_default();
    assert!(
        !trace.contains("ls-remote")
            && !trace.contains("upload-pack")
            && !trace.contains("receive-pack")
    );
    assert_eq!(history.tip(&history.store, "refs/heads/main"), old);
    history.empty_remote(&old);
}

#[test]
fn new_split_routes_refuse_before_any_remote_contact() {
    let history = History::new();
    history.configure(Some(true), true);
    let tip = history.save();
    let origin = "https://github.com/example/fixture.git";
    let other = "ssh://git@github.com/example/fixture.git";
    let error = publication::preflight(
        || {
            let mut git = history.git(&history.store);
            git.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", format!("url.{other}.pushInsteadOf"))
                .env("GIT_CONFIG_VALUE_0", origin);
            Ok(git)
        },
        |mut cmd| {
            assert!(!cmd.get_args().any(|arg| arg == "ls-remote"));
            cmd.output()
                .map_err(|_| Fail::new(1, "fixture Git did not run"))
        },
        BRANCH,
        origin,
        &tip,
    )
    .unwrap_err();
    assert!(error.diagnostic.contains("different fetch and push routes"));
}

#[test]
fn new_only_ref_queries_allow_git_transport_and_do_not_prompt() {
    let history = History::new();
    history.configure(Some(true), true);
    let tip = history.save();
    let ref_queries = std::cell::Cell::new(0);
    publication::preflight(
        || {
            let mut git = history.git(&history.store);
            git.env("GIT_TERMINAL_PROMPT", "1");
            Ok(git)
        },
        |mut cmd| {
            let env = |name| {
                cmd.get_envs()
                    .find_map(|(key, value)| (key == name).then_some(value))
                    .flatten()
            };
            if cmd.get_args().any(|arg| arg == "ls-remote") {
                assert_eq!(env("GIT_TERMINAL_PROMPT").unwrap(), "0");
                assert_eq!(env("GIT_ALLOW_PROTOCOL").unwrap(), "file");
                ref_queries.set(ref_queries.get() + 1);
            } else {
                assert_eq!(env("GIT_ALLOW_PROTOCOL").unwrap(), "");
            }
            cmd.output()
                .map_err(|_| Fail::new(1, "fixture Git did not run"))
        },
        BRANCH,
        &history.origin,
        &tip,
    )
    .unwrap();
    assert_eq!(ref_queries.get(), 2);
    history.empty_remote(&tip);
}

fn with_unscoped_config(history: &History) {
    let live = history.world.root().join("fixture/payload");
    fs::create_dir(live.parent().unwrap()).unwrap();
    fs::write(&live, PAYLOAD).unwrap();
    history.configure(Some(false), false);
    let config = history.world.root().join("config.toml");
    let input = fs::read_to_string(&config)
        .unwrap()
        .replace("\"~/fixture/payload\"", &format!("{live:?}"));
    fs::write(config, input).unwrap();
}

fn replace_manifest(history: &History, original: &str, donor: &str, parents: &[&str]) -> String {
    let index = history
        .world
        .root()
        .parent()
        .unwrap()
        .join("fixture-tree-index");
    let git = || {
        let mut cmd = history.git(&history.store);
        cmd.env("GIT_INDEX_FILE", &index);
        cmd
    };
    let manifest = ".mise-history/manifest.json";
    let oid = history.tip(&history.store, &format!("{donor}:{manifest}"));
    ok(git().args(["read-tree", original]));
    ok(git().args(["update-index", "--cacheinfo", "100644", &oid, manifest]));
    let tree = text(ok(git().arg("write-tree")));
    let mut commit = git();
    commit.args([
        "commit-tree",
        &tree,
        "-m",
        "synthetic carried unscoped config",
    ]);
    for parent in parents {
        commit.args(["-p", parent]);
    }
    text(ok(&mut commit))
}

#[test]
fn new_unscoped_config_retains_tracked_provenance_from_a_published_merge_parent() {
    let history = History::new();
    with_unscoped_config(&history);
    let old = history.save();
    let path = "config/fixture/payload";
    assert_eq!(
        history.object(&history.store, &old, path),
        PAYLOAD.as_bytes()
    );
    ok(history
        .git(&history.store)
        .args(["push", &history.origin, &format!("{old}:{REMOTE_REF}")]));
    history.configure(None, false);
    let removed = history.save();
    let independent = replace_manifest(&history, &removed, &removed, &[]);
    let merge = replace_manifest(&history, &old, &removed, &[&independent, &old]);
    ok(history
        .git(&history.store)
        .args(["update-ref", "refs/heads/main", &merge, &removed]));
    let refusal = history.preflight().unwrap_err();
    assert!(
        refusal.diagnostic.contains(path) && refusal.diagnostic.contains(&merge),
        "{refusal}"
    );
    assert_eq!(history.tip(&history.remote, REMOTE_REF), old);
}

#[test]
fn new_plain_unscoped_config_without_tracked_provenance_is_allowed() {
    let history = History::new();
    with_unscoped_config(&history);
    let old = history.save();
    history.configure(None, false);
    let plain = history.save();
    // The same file content in a new history with an ordinary configuration
    // tree and no enrollment ancestry is valid native plaintext.
    let checkpoint = replace_manifest(&history, &old, &plain, &[]);
    ok(history
        .git(&history.store)
        .args(["update-ref", "refs/heads/main", &checkpoint, &plain]));
    history.preflight().unwrap();
    history.empty_remote(&checkpoint);
}

#[test]
fn new_unreadable_promisor_payload_refuses_without_fetching_it() {
    let history = History::new();
    history.configure(Some(true), true);
    let tip = history.save();
    let oid = history.tip(&history.store, &format!("{tip}:{HOST_SCOPED}"));
    let promisor = history.world.root().parent().unwrap().join("promisor.git");
    ok(history.git(&promisor).args(["init", "--bare"]));
    let promised_url = format!("file://{}", promisor.display());
    ok(history
        .git(&history.store)
        .args(["push", &promised_url, &format!("{tip}:{REMOTE_REF}")]));
    ok(history
        .git(&history.store)
        .args(["config", "remote.promise.url", &promised_url]));
    ok(history
        .git(&history.store)
        .args(["config", "remote.promise.promisor", "true"]));
    let (directory, object) = oid.split_at(2);
    fs::remove_file(history.store.join("objects").join(directory).join(object)).unwrap();
    history.assert_refusal(&tip, HOST_SCOPED);
    assert!(
        !history
            .git(&history.store)
            .args(["cat-file", "-e", &oid])
            .env("GIT_ALLOW_PROTOCOL", "")
            .output()
            .unwrap()
            .status
            .success()
    );
}
