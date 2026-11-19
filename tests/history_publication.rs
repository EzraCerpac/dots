mod support;

#[path = "../src/history/publication.rs"]
mod publication;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use dots_cli::{Fail, config};
use publication::{GitAccess, HttpPolicy, git_environment, git_parameters};
use support::{FixtureCommand, World};

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
        let base = world.base();
        let store = base.join("mise/state/history/repo.git");
        let remote = world.bare_remote("state-origin.git");
        let origin = format!("file://{}", remote.display());
        fs::create_dir(base.join("git-template")).unwrap();
        let mut history = Self {
            world,
            store,
            remote,
            origin,
            recipient: String::new(),
        };
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

    fn isolate(&self, cmd: &mut FixtureCommand<'_>) {
        let base = self.world.base();
        cmd.env("CARGO_HOME", base.join("cargo"))
            .env("RUSTUP_HOME", base.join("rustup"))
            .env("GIT_TEMPLATE_DIR", base.join("git-template"))
            .env("MISE_CONFIG_DIR", self.world.root())
            .env("MISE_TRUSTED_CONFIG_PATHS", self.world.root())
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_AUTO_UPDATE", "0")
            .env("MISE_OFFLINE", "1")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null());
    }

    fn command(&self, program: &str) -> FixtureCommand<'_> {
        let mut cmd = self.world.command(program);
        self.isolate(&mut cmd);
        cmd
    }

    fn dots(&self) -> FixtureCommand<'_> {
        let mut cmd = self.world.dots();
        self.isolate(&mut cmd);
        cmd
    }

    fn observed_backup(&self) -> (FixtureCommand<'_>, PathBuf) {
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

    fn git(&self, repo: &Path) -> FixtureCommand<'_> {
        let mut cmd = self.world.git();
        self.isolate(&mut cmd);
        cmd.arg("--git-dir").arg(repo);
        cmd
    }

    fn mise(&self, operation: &str) -> FixtureCommand<'_> {
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
            |args, access| self.probe(args, access),
            BRANCH,
            &self.origin,
            &self.tip(&self.store, "refs/heads/main"),
            false,
        )
        .map(|_| ())
    }

    fn probe(&self, args: &[&str], access: GitAccess<'_>) -> Result<Output, Fail> {
        let mut cmd = self.git(&self.store);
        if let Some(parameters) = git_parameters(access, cmd.get_envs()) {
            cmd.env("GIT_CONFIG_PARAMETERS", parameters);
        }
        cmd.envs(git_environment(access))
            .args(args)
            .output()
            .map_err(|_| Fail::new(1, "fixture Git did not run"))
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

fn ok(cmd: &mut FixtureCommand<'_>) -> Output {
    let result = cmd.output().unwrap();
    assert!(result.status.success(), "{result:?}");
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
        |args, access| {
            if access == GitAccess::Remote && observed.replace(true) {
                let mut changed = args.to_vec();
                changed[4] = &other_origin;
                return history.probe(&changed, access);
            }
            history.probe(args, access)
        },
        BRANCH,
        &history.origin,
        &tip,
        false,
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
    // Prepare the encrypted tip with native mise so this test reaches the
    // publication policy after the independent capture guard.
    history.save();

    let refused = history.dots().arg("backup").output().unwrap();
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let diagnostic = String::from_utf8(refused.stderr).unwrap();
    assert!(diagnostic.contains(UNSCOPED) && diagnostic.contains(&old));
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
        |args, access| {
            assert!(access == GitAccess::Local);
            history
                .git(&history.store)
                .envs(git_environment(access))
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", format!("url.{other}.pushInsteadOf"))
                .env("GIT_CONFIG_VALUE_0", origin)
                .args(args)
                .output()
                .map_err(|_| Fail::new(1, "fixture Git did not run"))
        },
        BRANCH,
        origin,
        &tip,
        false,
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
        |args, access| {
            let mut cmd = history.git(&history.store);
            cmd.env("GIT_TERMINAL_PROMPT", "1")
                .envs(git_environment(access))
                .args(args);
            let env = |name| {
                cmd.get_envs()
                    .find_map(|(key, value)| (key == name).then_some(value))
                    .flatten()
            };
            if args.first() == Some(&"ls-remote") {
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
        false,
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

#[test]
fn new_per_origin_git_service_override_is_ambiguous() {
    let history = History::new();
    history.configure(Some(true), true);
    let tip = history.save();
    for key in ["receivepack", "vcs"] {
        let refusal = publication::preflight(
            |args, access| {
                assert!(access == GitAccess::Local);
                history
                    .git(&history.store)
                    .envs(git_environment(access))
                    .env("GIT_CONFIG_COUNT", "1")
                    .env(
                        "GIT_CONFIG_KEY_0",
                        format!("remote.{}.{key}", history.origin),
                    )
                    .env("GIT_CONFIG_VALUE_0", "synthetic-custom-backend")
                    .args(args)
                    .output()
                    .map_err(|_| Fail::new(1, "fixture Git did not run"))
            },
            BRANCH,
            &history.origin,
            &tip,
            false,
        )
        .unwrap_err();
        assert!(
            refusal
                .diagnostic
                .contains("different fetch and push routes")
        );
    }
    history.empty_remote(&tip);
}

#[test]
fn new_x25519_requires_canonical_share_and_wrapped_key() {
    let valid = "A".repeat(43);
    let noncanonical = format!("{}B", "A".repeat(42));
    let age = |args: &str, body: &str| {
        let mut bytes =
            format!("age-encryption.org/v1\n-> {args}\n{body}\n--- {valid}\n").into_bytes();
        bytes.extend([0; 33]);
        bytes
    };
    assert!(publication::age_file(&age(
        &format!("X25519 {valid}"),
        &valid
    )));
    for (args, body) in [
        ("X25519".to_owned(), valid.as_str()),
        (format!("X25519 {valid} extra"), valid.as_str()),
        (format!("X25519 {noncanonical}"), valid.as_str()),
        (format!("X25519 {}", "A".repeat(42)), valid.as_str()),
        (format!("X25519 {valid}"), ""),
        (format!("X25519 {valid}"), &valid[..42]),
        (format!("X25519 {valid}"), noncanonical.as_str()),
    ] {
        assert!(!publication::age_file(&age(&args, body)));
    }
    assert!(publication::age_file(&age(
        "plugin-fixture arbitrary arguments",
        ""
    )));
    assert!(publication::age_file(&age(
        "fixture-grease arbitrary",
        &format!("{}\n{valid}", "A".repeat(64))
    )));
}

#[test]
fn new_http_exact_scope_disables_redirects_without_changing_other_git_config() {
    let history = History::new();
    let url = "https://fixture.invalid/repo=fixture'with-quote.git/";
    let prior =
        std::ffi::OsStr::new("'http.followRedirects'='true' 'credential.helper'='fixture-helper'");
    // Build the native command without starting it. Real Git in the
    // guarded fixture reads the exact parameters that native sync inherits.
    let mut native = std::process::Command::new("mise");
    native.env_clear().env("GIT_CONFIG_PARAMETERS", prior);
    publication::constrain_http(&mut native, Some(&HttpPolicy::Exact(url.to_owned())));
    let parameters = native
        .get_envs()
        .find_map(|(key, value)| (key == "GIT_CONFIG_PARAMETERS").then_some(value).flatten())
        .unwrap();
    let value = |key: &str, target: &str| {
        text(ok(history
            .git(&history.remote)
            .env("GIT_CONFIG_PARAMETERS", parameters)
            .args(["config", "--get-urlmatch", key, target])))
    };
    assert_eq!(value("http.followRedirects", url), "false");
    assert_eq!(
        value("http.followRedirects", "https://elsewhere.invalid/other"),
        "true"
    );
    assert_eq!(value("credential.helper", url), "fixture-helper");
}

#[test]
fn new_http_slash_scoped_redirect_cannot_supply_remote_ancestry() {
    let history = History::new();
    history.configure(Some(false), false);
    let tip = history.save();
    let requests = http_requests(&tip, |http| {
        let origin = format!("{http}/declared");
        let scoped = format!("{origin}/");
        let competing = format!("http.{scoped}/.followRedirects");
        ok(history
            .git(&history.store)
            .args(["config", &competing, "true"]));

        // The old proof resolved false at the raw URL, but stock Git followed
        // a redirect and returned a checkpoint advertised by a different path.
        let old_params = format!("'http.{origin}.followRedirects'='false'");
        let value = |url: &str| {
            text(ok(history
                .git(&history.store)
                .env("GIT_CONFIG_PARAMETERS", &old_params)
                .args([
                    "config",
                    "--get-urlmatch",
                    "http.followRedirects",
                    url,
                ])))
        };
        assert_eq!(value(&origin), "false");
        assert_eq!(value(&scoped), "true");
        let refs = [
            "ls-remote",
            "--refs",
            "--exit-code",
            "--",
            &origin,
            REMOTE_REF,
        ];
        let lookup = |parameters: &std::ffi::OsStr| {
            history
                .git(&history.store)
                .env("GIT_ALLOW_PROTOCOL", "http")
                .env("GIT_CONFIG_PARAMETERS", parameters)
                .args(refs)
                .output()
                .unwrap()
        };
        let old = lookup(old_params.as_ref());
        assert!(old.status.success());
        assert_eq!(text(old), format!("{tip}\t{REMOTE_REF}"));

        let run = |origin| {
            publication::preflight(
                |args, access| {
                    let mut cmd = history.git(&history.store);
                    if let Some(params) = git_parameters(access, cmd.get_envs()) {
                        cmd.env("GIT_CONFIG_PARAMETERS", params);
                    }
                    if !access.is_local() {
                        cmd.env("GIT_ALLOW_PROTOCOL", "http");
                    }
                    cmd.envs(git_environment(access))
                        .args(args)
                        .output()
                        .map_err(|_| Fail::new(1, "fixture Git did not run"))
                },
                BRANCH,
                origin,
                &tip,
                false,
            )
        };
        let refusal = run(&origin).unwrap_err();
        assert_eq!(refusal.status, 1);
        assert!(
            refusal
                .diagnostic
                .contains("could not verify saved settings history")
        );

        // A direct route still supplies ancestry. The returned policy uses
        // the same match URL for the native command and its ref readback.
        let direct = format!("{http}/direct/");
        assert_eq!(run(&direct).unwrap(), Some(HttpPolicy::Exact(direct)));
        let mut native = std::process::Command::new("mise");
        native.env_clear();
        publication::constrain_http(&mut native, Some(&HttpPolicy::Exact(scoped)));
        let params = native
            .get_envs()
            .find_map(|(key, value)| (key == "GIT_CONFIG_PARAMETERS").then_some(value).flatten())
            .unwrap();
        assert!(!lookup(params).status.success());
    });
    let advertised_elsewhere = requests
        .iter()
        .filter(|path| path.starts_with("/elsewhere/info/refs"))
        .count();
    assert_eq!(
        advertised_elsewhere, 1,
        "only the old lookup follows a redirect: {requests:?}"
    );
}

fn http_requests(tip: &str, exercise: impl FnOnce(String)) -> Vec<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let refs = format!("{tip}\t{REMOTE_REF}\n");
    let (done, stop) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.recv_timeout(Duration::from_millis(5))
                        == Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        continue;
                    }
                    return requests;
                }
                Err(err) => panic!("fixture HTTP accept failed: {err}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut lines = BufReader::new(&stream).lines();
            let request = lines.next().unwrap().unwrap();
            let path = request.split_ascii_whitespace().nth(1).unwrap();
            for header in lines {
                if header.unwrap().is_empty() {
                    break;
                }
            }
            requests.push(path.to_owned());
            if let Some(path) = path.strip_prefix("/declared/") {
                write!(stream, "HTTP/1.1 302 Found\r\nConnection: close\r\nLocation: /elsewhere/{path}\r\nContent-Length: 0\r\n\r\n").unwrap();
            } else {
                let body = if path.ends_with("/HEAD") {
                    "ref: refs/heads/host-fixture\n"
                } else {
                    &refs
                };
                write!(stream, "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            }
        }
    });
    exercise(endpoint);
    drop(done);
    server.join().unwrap()
}

#[test]
fn new_git_askpass_is_suppressed_and_provisioned_helpers_still_work() {
    let history = History::new();
    let file = history.world.base().join("credential-input");
    let sent = history.world.base().join("askpass-ran");
    let askpass = history.world.base().join("askpass");
    let helper = history.world.base().join("credential-helper");
    fs::write(&file, b"protocol=https\nhost=fixture.invalid\n\n").unwrap();
    fs::write(&askpass, "#!/bin/sh\n: > \"$FIXTURE_ASKPASS_LOG\"\n").unwrap();
    fs::write(
        &helper,
        "#!/bin/sh\nprintf 'username=fixture\npassword=synthetic-credential\n'\n",
    )
    .unwrap();
    for program in [&askpass, &helper] {
        fs::set_permissions(program, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let run = |helper: Option<&Path>| {
        let mut cmd = history.git(&history.remote);
        cmd.env("GIT_ASKPASS", &askpass)
            .env("SSH_ASKPASS", &askpass)
            .env("FIXTURE_ASKPASS_LOG", &sent)
            .args(["-c", &format!("core.askPass={}", askpass.display())]);
        if let Some(helper) = helper {
            cmd.args(["-c", &format!("credential.helper={}", helper.display())]);
        }
        let params = git_parameters(GitAccess::Remote, cmd.get_envs()).unwrap();
        cmd.envs(git_environment(GitAccess::Remote))
            .env("GIT_CONFIG_PARAMETERS", params)
            .stdin(Stdio::from(fs::File::open(&file).unwrap()))
            .args(["credential", "fill"])
            .output()
            .unwrap()
    };
    assert!(!run(None).status.success());
    assert!(!sent.exists());
    let authenticated = run(Some(&helper));
    assert!(authenticated.status.success());
    assert!(!sent.exists());
}

#[test]
fn new_stock_ssh_guard_uses_batch_mode_with_existing_agent() {
    let history = History::new();
    let mut cmd = history.git(&history.remote);
    let provisioned = history.world.base().join("fixture-agent");
    cmd.env("SSH_AUTH_SOCK", &provisioned);
    assert!(!publication::has_ssh_launcher(|name| fixture_env(
        &cmd, name
    )));
    cmd.envs(git_environment(GitAccess::Ssh));
    assert_eq!(
        fixture_env(&cmd, "GIT_SSH_COMMAND").unwrap(),
        "ssh -oBatchMode=yes"
    );
    assert_eq!(fixture_env(&cmd, "SSH_ASKPASS_REQUIRE").unwrap(), "never");
    assert_eq!(
        fixture_env(&cmd, "SSH_AUTH_SOCK").unwrap(),
        provisioned.as_os_str()
    );
}

fn fixture_env<'a>(cmd: &'a FixtureCommand<'_>, name: &str) -> Option<&'a std::ffi::OsStr> {
    cmd.get_envs()
        .find_map(|(key, value)| (key == name).then_some(value))
        .flatten()
}

#[test]
fn new_configured_ssh_audits_full_local_ancestry_without_probing_the_wrapper() {
    let history = History::new();
    let origin = "ssh://git@fixture.invalid/state.git";
    history.configure(Some(false), false);
    let old = history.save();
    history.configure(None, false);
    let tip = history.save();

    // Model both environment and effective Git config without invoking SSH.
    for command in [
        "ssh -i '/fixture/private identity'",
        "ssh -F /fixture/ssh_config",
        "opaque-launcher",
    ] {
        for from_env in [true, false] {
            let mut launch = history.git(&history.store);
            if from_env {
                launch.env("GIT_SSH_COMMAND", command);
            }
            let err = publication::preflight(
                |args, access| {
                    assert!(access.is_local());
                    let mut git = history.git(&history.store);
                    if !from_env {
                        git.env("GIT_CONFIG_COUNT", "1")
                            .env("GIT_CONFIG_KEY_0", "core.sshCommand")
                            .env("GIT_CONFIG_VALUE_0", command);
                    }
                    git.envs(git_environment(access))
                        .args(args)
                        .output()
                        .map_err(|_| Fail::new(1, "fixture Git did not run"))
                },
                BRANCH,
                origin,
                &tip,
                publication::has_ssh_launcher(|name| fixture_env(&launch, name)),
            )
            .unwrap_err();
            assert!(err.diagnostic.contains(UNSCOPED) && err.diagnostic.contains(&old));
            assert!(!err.diagnostic.contains(command));
        }
    }
    history.empty_remote(&old);
}

#[test]
fn new_unverified_http_scope_audits_full_local_ancestry_before_network() {
    use std::os::unix::process::ExitStatusExt as _;

    let run = |history: &History, tip: &str| {
        publication::preflight(
            |args, access| match access {
                GitAccess::CheckHttp(_) => Ok(Output {
                    status: std::process::ExitStatus::from_raw(128 << 8),
                    stdout: Vec::new(),
                    stderr: b"private Git URL matcher diagnostic".to_vec(),
                }),
                GitAccess::Local => history.probe(args, access),
                _ => panic!("an unverified HTTP route must not supply ref ancestry"),
            },
            BRANCH,
            "http://%31%32%37.0.0.1:9/fixture",
            tip,
            false,
        )
    };
    let history = History::new();
    history.configure(Some(false), false);
    let old = history.save();
    history.configure(None, false);
    let tip = history.save();
    let refusal = run(&history, &tip).unwrap_err();
    assert!(refusal.diagnostic.contains(UNSCOPED) && refusal.diagnostic.contains(&old));
    assert!(
        !refusal
            .diagnostic
            .contains("private Git URL matcher diagnostic")
    );

    let clean = History::new();
    clean.configure(Some(true), true);
    let tip = clean.save();
    assert_eq!(run(&clean, &tip).unwrap(), Some(HttpPolicy::Default));
    clean.empty_remote(&tip);
}

#[test]
fn new_age_ssh_recipient_stanzas_require_the_builtin_fields() {
    let tag = "A".repeat(6);
    let share = "A".repeat(43);
    let rsa = "A".repeat(342);
    let raw = |args: &str, body: &str| {
        let mut bytes = format!("age-encryption.org/v1\n-> {args}\n").into_bytes();
        for line in body.as_bytes().chunks(64) {
            bytes.extend_from_slice(line);
            bytes.push(b'\n');
        }
        if body.len().is_multiple_of(64) {
            bytes.push(b'\n');
        }
        bytes.extend(format!("--- {share}\n").as_bytes());
        bytes.extend([0; 33]);
        bytes
    };
    let ed = format!("ssh-ed25519 {tag} {share}");
    assert!(publication::age_file(&raw(&ed, &share)));
    for (args, body) in [
        ("ssh-ed25519".to_owned(), share.as_str()),
        (format!("ssh-ed25519 {share}"), share.as_str()),
        (format!("ssh-ed25519 AAAAAQ {share} extra"), share.as_str()),
        (format!("ssh-ed25519 AAAAAB {share}"), share.as_str()),
        (
            format!("ssh-ed25519 {tag} {}", &share[..42]),
            share.as_str(),
        ),
        (ed.clone(), ""),
        (ed, &share[..42]),
    ] {
        assert!(!publication::age_file(&raw(&args, body)));
    }

    let rsa_args = format!("ssh-rsa {tag}");
    assert!(publication::age_file(&raw(&rsa_args, &rsa)));
    for (args, body) in [
        ("ssh-rsa".to_owned(), rsa.as_str()),
        (format!("ssh-rsa {tag} extra"), rsa.as_str()),
        ("ssh-rsa AAAAAB".to_owned(), rsa.as_str()),
        (rsa_args, &rsa[..340]),
    ] {
        assert!(!publication::age_file(&raw(&args, body)));
    }
}
