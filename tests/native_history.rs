mod support;

use std::env;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use support::World;

const BRANCH: &str = "host-fixture";
const REF: &str = "refs/heads/host-fixture";
const URL: &str = "git@github.com:p06-fixture/state.git";
const MARKER: &[u8] = b"p06-fixture-content";
const FIRST: &[u8] = b"p06-fixture-content incoming first\n";
const SECOND: &[u8] = b"p06-fixture-content explicit backup\n";
const DEFAULT: &[u8] = b"p06-fixture-content first-run default\n";
const INACTIVE: &[u8] = b"p06-fixture-content inactive sentinel\n";
const LOCAL_EDIT: &[u8] = b"p06-fixture-content local conflict\n";
const REMOTE_EDIT: &[u8] = b"p06-fixture-content remote conflict\n";

struct Fixture {
    world: World,
    mise: PathBuf,
    age: PathBuf,
    keygen: PathBuf,
    git: PathBuf,
    path: std::ffi::OsString,
}

impl Fixture {
    fn new() -> Self {
        let world = World::new();
        let mise = tool("mise");
        let age = tool("age");
        let keygen = tool("age-keygen");
        let git = tool("git");
        let mut dirs = vec![world.home().join("bin")];
        for program in [&mise, &age, &keygen, &git] {
            dirs.push(program.parent().unwrap().to_owned());
        }
        if let Some(node) = find_tool("node") {
            dirs.push(node.parent().unwrap().to_owned());
        }
        dirs.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
        let path = env::join_paths(dirs).unwrap();
        for directory in ["bin", "tmp", "state", "data", "cache", "app"] {
            fs::create_dir(world.home().join(directory)).unwrap();
        }
        symlink(world.root(), world.home().join(".config/mise")).unwrap();
        let fixture = Self {
            world,
            mise,
            age,
            keygen,
            git,
            path,
        };
        fixture.watcher(1);
        fs::write(fixture.inactive(), INACTIVE).unwrap();
        fixture
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let script = self.world.home().join("bin").join(name);
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        script
    }

    fn watcher(&self, status: u8) {
        self.script("pgrep", &format!("#!/bin/sh\nexit {status}\n"));
        for service in ["launchctl", "systemctl"] {
            self.script(service, "#!/bin/sh\nexit 99\n");
        }
    }

    fn base(&self, command: &mut Command) {
        let home = self.world.home();
        command
            .env("PATH", &self.path)
            .env("LC_ALL", "C")
            .env("TMPDIR", home.join("tmp"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("DOTS_SKILL_LINKS", "0")
            .env("MISE_CONFIG_DIR", self.world.root())
            .env("MISE_TRUSTED_CONFIG_PATHS", self.world.root())
            .env("MISE_SYSTEM_CONFIG_DIR", home.join(".mise-system"))
            .env("MISE_CEILING_PATHS", self.world.root().parent().unwrap())
            .env("DOTS_MISE_BIN", &self.mise)
            .env("MISE_STATE_DIR", home.join("state"))
            .env("MISE_DATA_DIR", home.join("data"))
            .env("MISE_CACHE_DIR", home.join("cache"))
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_EXEC_AUTO_INSTALL", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file:ssh");
    }

    fn command(&self, program: &Path) -> Command {
        let world = self.world.dots();
        let mut command = Command::new(program);
        command.env_clear().current_dir(self.world.root());
        for (key, value) in world.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        self.base(&mut command);
        command
    }

    fn dots(&self, args: &[&str], remote: &Path) -> Command {
        let mut cmd = self.world.dots();
        self.base(&mut cmd);
        let transport = self.script(
            "ssh",
            r#"#!/bin/sh
set -eu
for last do :; done
printf '%s\n' "$last" >> "$FIXTURE_TRANSPORT"
case "$last" in
  "git-upload-pack 'p06-fixture/state.git'") exec "$FIXTURE_GIT" upload-pack "$FIXTURE_REMOTE" ;;
  "git-receive-pack 'p06-fixture/state.git'") exec "$FIXTURE_GIT" receive-pack "$FIXTURE_REMOTE" ;;
  *) exit 97 ;;
esac
"#,
        );
        cmd.env("GIT_SSH_COMMAND", transport)
            .env("GIT_SSH_VARIANT", "ssh")
            .env("FIXTURE_GIT", &self.git)
            .env("FIXTURE_REMOTE", remote)
            .env("FIXTURE_TRANSPORT", self.world.home().join("transport"))
            .args(args);
        cmd
    }

    fn git(&self, store: Option<&Path>) -> Command {
        let mut cmd = self.world.git();
        self.base(&mut cmd);
        if let Some(store) = store {
            cmd.arg("--git-dir").arg(store);
        }
        cmd
    }

    fn mise(&self, args: &[&str]) -> Command {
        let mut cmd = self.command(&self.mise);
        cmd.args(["-E", "laptop,host-fixture"]).args(args);
        cmd
    }

    fn configure(&self, setting: &str, origin: &str, key: &Path, recipients: &[String]) {
        let root = self.world.root();
        fs::write(
            root.join("config.toml"),
            format!(
                r#"
min_version = "2026.11.8"
[vars]
dots_profiles = "laptop"
dots_state_repo = {setting:?}

[dotfiles]
"~/app/selected" = {{ mode = "track", encrypt = true, variants = [{{ profile = "{BRANCH}" }}] }}
"~/app/inactive" = {{ mode = "track", encrypt = false, variants = [{{ profile = "host-other" }}] }}
"#
            ),
        )
        .unwrap();
        fs::write(root.join("config.laptop.toml"), "").unwrap();
        fs::write(
            root.join("miserc.toml"),
            format!("env = [\"laptop\", {BRANCH:?}]\n"),
        )
        .unwrap();
        fs::write(
            root.join("config.local.toml"),
            format!(
                r#"
[env]
SETUP_PROFILE = "laptop"
SETUP_MACHINE_ID = "fixture"
SETUP_HISTORY_ORIGIN = {origin:?}
SETUP_AGE_IDENTITY = {key:?}

[settings.history]
sync = "manual"

[settings.age]
identity_files = [{key:?}]

[history.origin]
url = {origin:?}
branch = {BRANCH:?}

[history.encryption]
recipients = {recipients:?}
"#
            ),
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "config.local.toml\nmiserc.toml\n").unwrap();
    }

    fn key(&self, name: &str) -> (PathBuf, String) {
        let path = self.world.home().join(name);
        ok(self.command(&self.keygen).arg("-o").arg(&path));
        let public = text(ok(self.command(&self.keygen).arg("-y").arg(&path)));
        (path, public)
    }

    fn store(&self) -> PathBuf {
        self.world.home().join("state/history/repo.git")
    }
    fn active(&self) -> PathBuf {
        self.world.home().join("app/selected")
    }
    fn inactive(&self) -> PathBuf {
        self.world.home().join("app/inactive")
    }

    fn write(&self, payload: &[u8], mode: u32) {
        fs::write(self.active(), payload).unwrap();
        fs::set_permissions(self.active(), fs::Permissions::from_mode(mode)).unwrap();
    }

    fn tip(&self, repo: &Path, reference: &str) -> String {
        text(ok(self.git(Some(repo)).args([
            "rev-parse",
            "--verify",
            reference,
        ])))
    }

    fn trace(&self) -> Vec<String> {
        fs::read_to_string(self.world.home().join("transport"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn seed(&self, remote: &Path, data: &[u8]) {
        self.write(data, 0o640);
        ok(&mut self.mise(&["bootstrap", "dotfiles", "save"]));
        ok(self
            .git(Some(&self.store()))
            .args(["push", "--"])
            .arg(remote)
            .arg(format!("HEAD:{REF}")));
    }

    fn assert_encrypted(&self, repo: &Path, keys: [&Path; 3]) {
        let all = ok(self
            .git(Some(repo))
            .args(["cat-file", "--batch-all-objects", "--batch"]));
        assert!(
            !contains(&all.stdout, MARKER),
            "a fixture plaintext marker entered a Git object"
        );
        let commits = text(ok(self.git(Some(repo)).args(["rev-list", "--all"])));
        for revision in commits.lines() {
            let tree = text(ok(self.git(Some(repo)).args([
                "ls-tree",
                "-r",
                "--name-only",
                revision,
            ])));
            assert!(!tree.lines().any(|path| path.ends_with("/app/inactive")));
            let paths = tree.lines().filter(|path| path.ends_with("/app/selected"));
            for path in paths {
                let blob = ok(self
                    .git(Some(repo))
                    .arg("show")
                    .arg(format!("{revision}:{path}")))
                .stdout;
                assert!(
                    blob.starts_with(b"mise-encrypted-file-v1\n"),
                    "expected an encrypted native history envelope"
                );
                let age_header = b"age-encryption.org/v1\n";
                let payload = blob
                    .windows(age_header.len())
                    .position(|bytes| bytes == age_header)
                    .unwrap();
                for (index, key) in keys.iter().enumerate() {
                    let mut child = self
                        .command(&self.age)
                        .args(["--decrypt", "-i"])
                        .arg(key)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap();
                    child
                        .stdin
                        .take()
                        .unwrap()
                        .write_all(&blob[payload..])
                        .unwrap();
                    assert_eq!(
                        child.wait_with_output().unwrap().status.success(),
                        index < 2
                    );
                }
            }
        }
    }
}

fn find_tool(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| {
            fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .and_then(|path| path.canonicalize().ok())
}

fn tool(name: &str) -> PathBuf {
    find_tool(name).unwrap_or_else(|| panic!("native history scenarios require {name} on PATH"))
}

fn ok(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}\n{}",
        messages(&output)
    );
    output
}

fn text(output: Output) -> String {
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}

fn messages(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|part| part == needle)
}

fn assert_no_plaintext(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            assert_no_plaintext(&entry.path());
        } else if kind.is_file() {
            assert!(
                !contains(&fs::read(entry.path()).unwrap(), MARKER),
                "plaintext survived at {:?}",
                entry.path()
            );
        }
    }
}

#[test]
fn unchanged_first_checkpoint_is_encrypted_and_native_rollback_restores_content_and_mode() {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("private.git");
    let (key, first) = fixture.key("machine-key");
    let (recovery, second) = fixture.key("recovery-key");
    let (wrong, _) = fixture.key("unrelated-key");
    fixture.configure("p06-fixture/state", URL, &key, &[first, second]);
    fixture.write(FIRST, 0o600);
    ok(&mut fixture.dots(&["backup"], &remote));
    let initial = fixture.tip(&remote, REF);
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert_eq!(
        fs::metadata(fixture.active()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fixture.write(SECOND, 0o600);
    ok(&mut fixture.dots(&["backup"], &remote));
    fixture.assert_encrypted(&remote, [&key, &recovery, &wrong]);
    ok(&mut fixture.mise(&[
        "bootstrap",
        "dotfiles",
        "rollback",
        "--to",
        &format!("commit:{initial}"),
        "~/app/selected",
        "--yes",
    ]));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert_eq!(
        fs::symlink_metadata(fixture.active())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(fs::symlink_metadata(fixture.active()).unwrap().is_file());
}

#[test]
fn unchanged_fresh_restore_requires_key_and_fetches_then_backup_publishes_undo_ancestry() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, first) = seed.key("recovery-key");
    let (partner, second) = seed.key("partner-key");
    let (wrong, _) = seed.key("unrelated-key");
    let recipients = [first, second];
    seed.configure("p06-fixture/state", URL, &key, &recipients);
    seed.seed(&remote, FIRST);
    let initial = seed.tip(&remote, REF);

    let fixture = Fixture::new();
    let missing = fixture.world.home().join("missing-recovery");
    fixture.configure("p06-fixture/state", URL, &missing, &recipients);
    fixture.write(DEFAULT, 0o644);
    let without_key = fixture.dots(&["restore"], &remote).output().unwrap();
    assert!(!without_key.status.success(), "{}", messages(&without_key));
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert_eq!(fixture.tip(&remote, REF), initial);
    fixture.configure("p06-fixture/state", URL, &key, &recipients);
    fs::write(
        fixture.world.root().join("public-sentinel"),
        b"public source fixture\n",
    )
    .unwrap();
    for args in [
        &["init"][..],
        &["add", "."],
        &["commit", "-m", "fixture public source"],
    ] {
        ok(fixture.git(None).args(args));
    }
    let source = text(ok(fixture.git(None).args(["rev-parse", "HEAD"])));
    let before = fixture.trace().len();
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert_eq!(fs::read(fixture.inactive()).unwrap(), INACTIVE);
    assert_eq!(
        fs::metadata(fixture.active()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fixture.tip(&remote, REF), initial);
    let requests = fixture.trace();
    assert!(!requests[before..].is_empty());
    assert!(
        requests[before..]
            .iter()
            .all(|line| line.starts_with("git-upload-pack "))
    );
    assert_eq!(
        text(ok(fixture.git(None).args(["rev-parse", "HEAD"]))),
        source
    );
    assert_eq!(
        text(ok(fixture.git(None).args(["status", "--porcelain"]))),
        ""
    );
    let store = fixture.store();
    let log = text(ok(fixture.git(Some(&store)).args([
        "log",
        "--format=%s",
        "main",
    ])));
    for subject in ["edited ~/app/selected", "before rollback", "rollback:"] {
        assert!(log.contains(subject), "{log}");
    }
    fixture.assert_encrypted(&store, [&key, &partner, &wrong]);
    for name in ["state", "cache", "tmp"] {
        assert_no_plaintext(&fixture.world.home().join(name));
    }
    let history = store.parent().unwrap();
    assert!(!history.join(".repo.git.restore-pending").exists());
    assert!(!history.join(".dots-restore-install").exists());
    assert!(!store.join("dots-restore-pending").exists());
    assert!(!fs::read_dir(history).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".restore.")
    }));

    let restored_tip = fixture.tip(&store, "refs/heads/main");
    fixture.write(SECOND, 0o600);
    ok(&mut fixture.dots(&["backup"], &remote));
    assert_ne!(fixture.tip(&remote, REF), initial);
    ok(fixture
        .git(Some(&remote))
        .args(["merge-base", "--is-ancestor", &restored_tip, REF]));
    fixture.assert_encrypted(&remote, [&key, &partner, &wrong]);

    let sender = Fixture::new();
    sender.configure("p06-fixture/state", URL, &key, &recipients);
    fs::create_dir_all(sender.store().parent().unwrap()).unwrap();
    ok(sender
        .git(None)
        .args(["clone", "--bare", "--single-branch", "--branch", BRANCH])
        .arg(&remote)
        .arg(sender.store()));
    ok(sender
        .git(Some(&sender.store()))
        .args(["branch", "-m", "main"]));
    sender.seed(&remote, REMOTE_EDIT);
    let conflicting_tip = fixture.tip(&remote, REF);
    let before = fixture.trace().len();
    fixture.write(LOCAL_EDIT, 0o600);
    let conflict = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(conflict.status.code(), Some(1), "{}", messages(&conflict));
    assert!(messages(&conflict).contains("both sides changed the same lines"));
    assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
    assert_eq!(fixture.tip(&remote, REF), conflicting_tip);
    assert!(
        fixture.trace()[before..]
            .iter()
            .all(|line| line.starts_with("git-upload-pack "))
    );
    fixture.assert_encrypted(&store, [&key, &partner, &wrong]);
}

#[test]
fn new_restore_reports_the_watcher_before_an_invalid_store() {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("private.git");
    let (key, public) = fixture.key("recovery");
    fixture.configure("p06-fixture/state", URL, &key, &[public]);
    fixture.write(DEFAULT, 0o644);
    let target = fixture.world.home().join("untouched");
    fs::write(&target, b"invalid store target\n").unwrap();
    let store = fixture.store();
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    symlink(&target, &store).unwrap();
    fixture.watcher(0);
    let output = fixture
        .dots(&["restore", "--initialize-history"], &remote)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", messages(&output));
    assert!(messages(&output).contains("watcher is running"));
    assert_eq!(fs::read_link(store).unwrap(), target);
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert!(fixture.trace().is_empty());
}

#[test]
fn new_restore_retries_after_interrupted_native_pull() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let (partner, second) = seed.key("partner");
    let (wrong, _) = seed.key("unrelated");
    let recipients = [public, second];
    let origin = format!("file://{}", remote.display());
    seed.configure(&origin, &origin, &key, &recipients);
    seed.seed(&remote, FIRST);
    let initial = seed.tip(&remote, REF);
    let fixture = Fixture::new();
    fixture.configure(&origin, &origin, &key, &recipients);
    fixture.write(DEFAULT, 0o644);
    let wrapper = fixture.script(
        "interrupt-mise",
        r#"#!/bin/sh
case " $* " in
  *" bootstrap dotfiles pull "*) exit 93 ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
    );
    let output = fixture
        .dots(&["restore"], &remote)
        .env("DOTS_MISE_BIN", wrapper)
        .env("FIXTURE_MISE", &fixture.mise)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", messages(&output));
    assert!(fixture.store().is_dir());
    assert!(fixture.store().join("dots-restore-pending").is_file());
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert_eq!(fixture.tip(&remote, REF), initial);
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert!(!fixture.store().join("dots-restore-pending").exists());
    assert_eq!(fixture.tip(&remote, REF), initial);
    fixture.assert_encrypted(&fixture.store(), [&key, &partner, &wrong]);
}

#[test]
fn new_initialize_preserves_the_displaced_history_and_a_failed_clone_leaves_it_in_place() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, first) = seed.key("recovery");
    let (_, second) = seed.key("partner");
    let recipients = [first, second];
    let origin = format!("file://{}", remote.display());
    seed.configure(&origin, &origin, &key, &recipients);
    seed.seed(&remote, FIRST);
    let fixture = Fixture::new();
    let wrong = format!(
        "file://{}",
        fixture.world.root().join("no-bare-remote").display()
    );
    fixture.configure(&wrong, &wrong, &key, &recipients);
    fixture.write(DEFAULT, 0o600);
    ok(&mut fixture.mise(&["bootstrap", "dotfiles", "save"]));
    let original = fixture.tip(&fixture.store(), "HEAD");
    let failed = fixture
        .dots(&["restore", "--initialize-history"], &remote)
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1), "{}", messages(&failed));
    assert_eq!(fixture.tip(&fixture.store(), "HEAD"), original);
    fixture.configure(&origin, &origin, &key, &recipients);
    let no_flag = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(no_flag.status.code(), Some(2), "{}", messages(&no_flag));
    ok(&mut fixture.dots(&["restore", "--initialize-history"], &remote));
    let store = fixture.store();
    let history = store.parent().unwrap();
    let backup = fs::read_dir(history)
        .unwrap()
        .find_map(|entry| {
            let entry = entry.unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("repo.git.pre-restore.")
                .then(|| entry.path())
        })
        .unwrap();
    assert_eq!(
        fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(fixture.tip(&backup.join("repo.git"), "HEAD"), original);
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
}
