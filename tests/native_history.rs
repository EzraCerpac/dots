mod support;

use std::env;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use support::{FixtureCommand, World};

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

    fn base(&self, command: &mut FixtureCommand<'_>) {
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
            .env("FIXTURE_GIT", &self.git)
            .env("MISE_STATE_DIR", home.join("state"))
            .env("MISE_DATA_DIR", home.join("data"))
            .env("MISE_CACHE_DIR", home.join("cache"))
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_EXEC_AUTO_INSTALL", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file:ssh");
    }

    fn command(&self, program: &Path) -> FixtureCommand<'_> {
        let mut command = self.world.command(program);
        self.base(&mut command);
        command
    }

    fn dots(&self, args: &[&str], remote: &Path) -> FixtureCommand<'_> {
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
            .env("FIXTURE_AGE_KEYGEN", &self.keygen)
            .env(
                "FIXTURE_GENERATION_MARKER",
                self.world.home().join("attempted-key-generation"),
            )
            .env("FIXTURE_REMOTE", remote)
            .env("FIXTURE_TRANSPORT", self.world.home().join("transport"))
            .args(args);
        cmd
    }

    fn offline_dots(&self, action: &str, remote: &Path) -> Output {
        let mise = self.script(
            "offline-native-history",
            r#"#!/bin/sh
case " $* " in
    *" bootstrap dotfiles sync "*) export GIT_ALLOW_PROTOCOL= ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
        );
        self.dots(&[action], remote)
            .env("DOTS_MISE_BIN", mise)
            .env("FIXTURE_MISE", &self.mise)
            .output()
            .unwrap()
    }

    fn enroll(&self, remote: &Path, identity: &Path, recipient: &str) -> Output {
        let mut command = self.dots(&[], remote);
        if self.archive_dots(remote) {
            command.arg("internal");
        }
        ok(command
            .args([
                "enroll-history",
                "--profile",
                "laptop",
                "--machine-id",
                "fixture",
                "--identity",
            ])
            .arg(identity)
            .args(["--origin", URL, "--recipient", recipient]))
    }

    fn archive_dots(&self, remote: &Path) -> bool {
        let version = text(ok(&mut self.dots(&["--version"], remote)));
        version.starts_with("dots 0.4.0 (") || version == "dots dev (archive)"
    }

    fn git(&self, store: Option<&Path>) -> FixtureCommand<'_> {
        let mut cmd = self.world.git();
        self.base(&mut cmd);
        if let Some(store) = store {
            cmd.arg("--git-dir").arg(store);
        }
        cmd
    }

    fn mise(&self, args: &[&str]) -> FixtureCommand<'_> {
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
        self.write(data, 0o600);
        ok(&mut self.mise(&["bootstrap", "dotfiles", "save"]));
        ok(self
            .git(Some(&self.store()))
            .args(["push", "--"])
            .arg(remote)
            .arg(format!("HEAD:{REF}")));
    }

    fn assert_published_encrypted(&self, repo: &Path, keys: [&Path; 3]) {
        let stream = format!("{REF}:home@{BRANCH}/app/selected");
        ok(self.git(Some(repo)).args(["cat-file", "-e", &stream]));
        self.assert_encrypted(repo, keys);
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
                for (index, key) in keys.iter().enumerate() {
                    assert_eq!(self.can_decrypt(&blob, key), index < 2);
                }
            }
        }
    }

    fn can_decrypt(&self, blob: &[u8], key: &Path) -> bool {
        let age_header = b"age-encryption.org/v1\n";
        let payload = blob
            .windows(age_header.len())
            .position(|bytes| bytes == age_header)
            .unwrap();
        let mut child = self
            .command(&self.age)
            .args(["--decrypt", "-i"])
            .arg(key)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&blob[payload..])
            .unwrap();
        child.wait().unwrap().success()
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

fn ok(command: &mut FixtureCommand<'_>) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{:?}\n{}",
        command.get_program(),
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
fn new_legacy_restore_marker_points_to_the_version_that_can_resume_it() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    seed.configure(
        "p06-fixture/state",
        URL,
        &key,
        std::slice::from_ref(&public),
    );
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    let fixture = Fixture::new();
    fixture.configure("p06-fixture/state", URL, &key, &[public]);
    fixture.write(DEFAULT, 0o600);
    let store = fixture.store();
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    ok(fixture
        .git(None)
        .args(["clone", "--bare", "--single-branch", "--branch", BRANCH])
        .arg(&remote)
        .arg(&store));
    ok(fixture.git(Some(&store)).args(["branch", "-m", "main"]));
    ok(fixture
        .git(Some(&store))
        .args(["config", "remote.origin.url", URL]));
    let marker = store.parent().unwrap().join(".repo.git.restore-pending");
    let pending = format!("{URL}\n");
    fs::write(&marker, &pending).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();

    if fixture.archive_dots(&remote) {
        ok(&mut fixture.dots(&["restore"], &remote));
        assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
        assert!(!marker.exists());
    } else {
        for command in ["backup", "restore"] {
            let output = fixture.dots(&[command], &remote).output().unwrap();
            assert_eq!(output.status.code(), Some(1), "{}", messages(&output));
            assert!(
                messages(&output).contains("finish it with the dots version that started it"),
                "{}",
                messages(&output)
            );
            assert_eq!(fs::read_to_string(&marker).unwrap(), pending);
            assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
        }
        assert!(fixture.trace().is_empty());
    }
    assert_eq!(seed.tip(&remote, REF), tip);
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
    fixture.assert_published_encrypted(&remote, [&key, &recovery, &wrong]);
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
    fixture.assert_published_encrypted(&remote, [&key, &partner, &wrong]);

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
fn new_native_sync_can_succeed_without_publishing_a_conflicting_checkpoint() {
    let local = Fixture::new();
    let remote = local.world.bare_remote("private.git");
    let (key, public) = local.key("recovery");
    let url = format!("file://{}", remote.display());
    local.configure(&url, &url, &key, std::slice::from_ref(&public));
    local.write(FIRST, 0o600);
    ok(&mut local.dots(&["backup"], &remote));
    let initial = local.tip(&remote, REF);

    let sender = Fixture::new();
    sender.configure(&url, &url, &key, &[public]);
    fs::create_dir_all(sender.store().parent().unwrap()).unwrap();
    ok(sender
        .git(None)
        .args(["clone", "--bare", "--single-branch", "--branch", BRANCH])
        .arg(&remote)
        .arg(sender.store()));
    ok(sender
        .git(Some(&sender.store()))
        .args(["branch", "-m", "main"]));
    sender.write(REMOTE_EDIT, 0o600);
    ok(&mut sender.mise(&["bootstrap", "dotfiles", "save"]));
    ok(sender
        .git(Some(&sender.store()))
        .args(["push", "--"])
        .arg(&remote)
        .arg(format!("HEAD:{REF}")));
    let remote_edit = sender.tip(&remote, REF);
    assert_ne!(remote_edit, initial);

    local.write(LOCAL_EDIT, 0o600);
    ok(&mut local.mise(&["bootstrap", "dotfiles", "save"]));
    let saved = local.tip(&local.store(), "refs/heads/main");
    let output = ok(&mut local.mise(&["bootstrap", "dotfiles", "sync"]));
    let report = messages(&output);
    let recovery = report
        .lines()
        .find(|line| line.contains("conflict(s) need a decision"))
        .unwrap_or_else(|| panic!("{report}"));
    assert!(
        recovery.contains("`mise dot status` lists them"),
        "{report}"
    );
    assert_eq!(local.tip(&remote, REF), remote_edit);
    assert_eq!(local.tip(&local.store(), "refs/heads/main"), saved);
    assert_eq!(
        local.tip(&local.store(), "refs/remotes/origin/setup"),
        remote_edit
    );
    let ancestry = local
        .git(Some(&local.store()))
        .args(["merge-base", "--is-ancestor", &saved, &remote_edit])
        .status()
        .unwrap();
    assert_eq!(ancestry.code(), Some(1));
    eprintln!(
        "mise sync exit 0; saved={saved}; remote {REF}={remote_edit}; saved is not an ancestor (Git exit 1)\n{recovery}"
    );
    assert_eq!(fs::read(local.active()).unwrap(), LOCAL_EDIT);
    let backup = local.dots(&["backup"], &remote).output().unwrap();
    assert_eq!(backup.status.code(), Some(1), "{}", messages(&backup));
    let pending = messages(&backup);
    let diagnostic = pending
        .lines()
        .find(|line| line.contains("checkpoint saved locally; publication is pending"))
        .unwrap_or_else(|| panic!("{pending}"));
    assert!(diagnostic.contains("Run mise dot status"));
    assert!(!messages(&backup).contains("Settings backup published"));
    let native_status = ok(local.mise(&["dot", "status"]).env("GIT_ALLOW_PROTOCOL", ""));
    let status = messages(&native_status);
    assert!(
        status.contains("both sides changed the same lines"),
        "{status}"
    );
    eprintln!(
        "dots backup exit 1: {diagnostic}\nmise dot status exit 0: both sides changed the same lines"
    );
    assert_eq!(local.tip(&remote, REF), remote_edit);
    assert_eq!(local.tip(&local.store(), "refs/heads/main"), saved);
}

#[test]
fn unchanged_standalone_reenrollment_selects_the_explicit_key_for_restore() {
    standalone_reenrollment_restore(false);
}

#[test]
fn new_standalone_explicit_replacement_restores_when_the_recipient_sets_match() {
    standalone_reenrollment_restore(true);
}

fn standalone_reenrollment_restore(matching_seed: bool) {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("private.git");
    let (old, old_recipient) = fixture.key("old-key");
    let (replacement, new_recipient) = fixture.key("new-key");
    let seed = Fixture::new();
    let seed_recipients = if matching_seed {
        vec![old_recipient.clone(), new_recipient]
    } else {
        vec![new_recipient]
    };
    seed.configure("p06-fixture/state", URL, &replacement, &seed_recipients);
    seed.seed(&remote, FIRST);
    let original_tip = seed.tip(&remote, REF);
    fixture.configure(
        "p06-fixture/state",
        URL,
        &old,
        std::slice::from_ref(&old_recipient),
    );
    fixture.write(DEFAULT, 0o600);
    fixture.script(
        "age-keygen",
        r#"#!/bin/sh
if [ "${1-}" = "-y" ]; then exec "$FIXTURE_AGE_KEYGEN" "$@"; fi
touch "$FIXTURE_GENERATION_MARKER"
exit 97
"#,
    );
    fixture.enroll(&remote, &replacement, &old_recipient);
    fs::rename(&old, old.with_extension("moved")).unwrap();
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert_eq!(fixture.tip(&remote, REF), original_tip);
    assert!(
        !fixture
            .world
            .home()
            .join("attempted-key-generation")
            .exists()
    );
    assert!(!fixture.world.home().join(".config/age/keys.txt").exists());
    let store = fixture.store();
    let head = fixture.tip(&store, "refs/heads/main");
    let checkpoint: serde_json::Value = serde_json::from_slice(
        &ok(&mut fixture.mise(&[
            "bootstrap",
            "dotfiles",
            "history",
            "show",
            &format!("commit:{head}"),
            "--json",
        ]))
        .stdout,
    )
    .unwrap();
    let undo = checkpoint["operation"]["before"]
        .as_str()
        .expect("native undo checkpoint");
    assert_ne!(undo, original_tip);
    let envelope = ok(fixture
        .git(Some(&store))
        .args(["show", &format!("{undo}:home@host-fixture/app/selected")]))
    .stdout;
    assert!(envelope.starts_with(b"mise-encrypted-file-v1\n"));
    assert!(fixture.can_decrypt(&envelope, &replacement));
    ok(fixture
        .mise(&["bootstrap", "dotfiles", "undo", "--yes"])
        .env("MISE_AGE_IDENTITY_FILES", &replacement)
        .env("GIT_ALLOW_PROTOCOL", ""));
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
}

#[test]
fn new_restore_refuses_plaintext_history_and_an_unconfigured_recipient_list() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (active, public) = seed.key("active");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &active, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);
    for plaintext in [true, false] {
        let fixture = Fixture::new();
        let keys = if plaintext {
            std::slice::from_ref(&public)
        } else {
            &[]
        };
        fixture.configure(&url, &url, &active, keys);
        fixture.write(LOCAL_EDIT, 0o644);
        let output = fixture
            .dots(&["restore"], &remote)
            .env(
                "MISE_HISTORY_ALLOW_PLAINTEXT_HISTORY",
                plaintext.to_string(),
            )
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{}", messages(&output));
        let expected = if plaintext {
            "plaintext history must be disabled before restore"
        } else {
            "configure [history.encryption].recipients"
        };
        assert!(
            messages(&output).contains(expected),
            "{}",
            messages(&output)
        );
        assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
        assert_eq!(
            fs::metadata(fixture.active()).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(!fixture.store().exists());
    }
    assert_eq!(seed.tip(&remote, REF), tip);
}

#[test]
fn new_standalone_reenrollment_retains_old_identity_paths_and_recipients() {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("private.git");
    let (old, old_recipient) = fixture.key("old-key");
    let (replacement, new_recipient) = fixture.key("new-key");
    let (_, archive_recipient) = fixture.key("archive-key");
    fixture.configure(
        "p06-fixture/state",
        URL,
        &old,
        &[old_recipient.clone(), archive_recipient.clone()],
    );
    fixture.write(DEFAULT, 0o600);
    fixture.enroll(&remote, &replacement, &old_recipient);
    let local: toml_edit::DocumentMut =
        fs::read_to_string(fixture.world.root().join("config.local.toml"))
            .unwrap()
            .parse()
            .unwrap();
    assert_eq!(
        local["env"]["SETUP_AGE_IDENTITY"].as_str(),
        replacement.to_str()
    );
    let identities = local["settings"]["age"]["identity_files"]
        .as_array()
        .unwrap();
    assert_eq!(
        identities
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
        [old.to_str(), replacement.to_str()]
    );
    let recipients = local["history"]["encryption"]["recipients"]
        .as_array()
        .unwrap();
    for expected in [&old_recipient, &new_recipient, &archive_recipient] {
        assert!(
            recipients
                .iter()
                .any(|value| value.as_str() == Some(expected))
        );
    }
    assert!(!fixture.store().exists());
}

#[test]
fn new_backup_keeps_an_unconfirmed_result_when_direct_remote_readback_fails() {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("private.git");
    let (key, public) = fixture.key("recovery");
    let url = format!("file://{}", remote.display());
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(FIRST, 0o600);
    ok(&mut fixture.dots(&["backup"], &remote));
    let initial = fixture.tip(&remote, REF);

    fixture.script(
        "git",
        r#"#!/bin/sh
if [ "${1-}" = "--git-dir" ] && [ "${3-}" = "ls-remote" ] && [ "${4-}" = "--exit-code" ]; then
    printf '%s\n' 'synthetic-direct-readback-error' >&2
    exit 97
fi
exec "$FIXTURE_GIT" "$@"
"#,
    );
    fixture.write(SECOND, 0o600);
    let output = fixture.dots(&["backup"], &remote).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", messages(&output));
    let report = messages(&output);
    assert!(
        report.contains("publication could not be verified"),
        "{report}"
    );
    assert!(!report.contains("Settings backup published"));
    assert!(!report.contains("synthetic-direct-readback-error"));
    fs::remove_file(fixture.world.home().join("bin/git")).unwrap();
    let remote_tip = fixture.tip(&remote, REF);
    assert_ne!(remote_tip, initial);
    assert_eq!(remote_tip, fixture.tip(&fixture.store(), "refs/heads/main"));
}

#[test]
fn new_backup_keeps_http_readback_bound_with_plaintext_opt_in() {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("unused.git");
    let (key, public) = fixture.key("recovery");
    let origin = "https://github.com/p06-fixture/state.git";
    fixture.configure(origin, origin, &key, &[public]);
    fixture.write(FIRST, 0o600);
    let readback = fixture.world.home().join("http-readback");
    let mise = fixture.script(
        "report-native-sync",
        r#"#!/bin/sh
set -eu
case " $* " in
    *" bootstrap dotfiles sync "*)
        if [ -n "$FIXTURE_REWRITE" ]; then
            "$FIXTURE_GIT" --git-dir "$FIXTURE_STORE" config \
                "url.$FIXTURE_REWRITE.insteadOf" "$FIXTURE_ORIGIN"
        fi
        exit 0
        ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
    );
    fixture.script(
        "git",
        r#"#!/bin/sh
set -eu
if [ "${1-}" = "--git-dir" ] && [ "${3-}" = "ls-remote" ] && [ "${4-}" = "--exit-code" ]; then
    printf 'readback\n' > "$FIXTURE_READBACK"
    [ "$7" = "$FIXTURE_ORIGIN" ]
    policy=$("$FIXTURE_GIT" --git-dir "$2" config --get-urlmatch http.followRedirects "$7/")
    printf '%s\n' "$policy" > "$FIXTURE_READBACK"
    oid=$("$FIXTURE_GIT" --git-dir "$2" rev-parse refs/heads/main)
    printf '%s\t%s\n' "$oid" "$8"
    exit 0
fi
exec "$FIXTURE_GIT" "$@"
"#,
    );
    let backup = |rewrite: &str| {
        fixture
            .dots(&["backup"], &remote)
            .env("DOTS_MISE_BIN", &mise)
            .env("FIXTURE_MISE", &fixture.mise)
            .env("FIXTURE_STORE", fixture.store())
            .env("FIXTURE_ORIGIN", origin)
            .env("FIXTURE_REWRITE", rewrite)
            .env("FIXTURE_READBACK", &readback)
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_SSH_COMMAND", "/bin/false")
            .env("MISE_HISTORY_ALLOW_PLAINTEXT_HISTORY", "true")
            .env(
                "GIT_CONFIG_PARAMETERS",
                format!("'http.{origin}/.followRedirects'='true'"),
            )
            .output()
            .unwrap()
    };

    let verified = backup("");
    assert_eq!(verified.status.code(), Some(0), "{}", messages(&verified));
    assert_eq!(fs::read_to_string(&readback).unwrap(), "false\n");
    fs::remove_file(&readback).unwrap();

    // The spelling still names the same GitHub repository, but the effective
    // transport changed after its redirect policy was checked.
    let mismatch = backup("https://github.com/P06-fixture/state.git");
    assert_eq!(mismatch.status.code(), Some(1), "{}", messages(&mismatch));
    assert!(messages(&mismatch).contains("publication could not be verified"));
    assert!(
        !readback.exists(),
        "readback was attempted after the route changed"
    );
}

#[test]
fn new_backup_reports_native_sync_failure_without_readback() {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("private.git");
    let (key, public) = fixture.key("recovery");
    let url = format!("file://{}", remote.display());
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(FIRST, 0o600);

    let rejection = remote.join("hooks/pre-receive");
    fs::write(
        &rejection,
        "#!/bin/sh\n: > pre-receive-ran\nprintf '%s\\n' 'synthetic-native-sync-rejection' >&2\nexit 91\n",
    )
    .unwrap();
    fs::set_permissions(&rejection, fs::Permissions::from_mode(0o700)).unwrap();
    let readback = fixture.world.home().join("direct-readback");
    fixture.script(
        "git",
        r#"#!/bin/sh
if [ "${1-}" = "--git-dir" ] && [ "${3-}" = "ls-remote" ] && [ "${4-}" = "--exit-code" ]; then
    printf 'readback\n' > "$FIXTURE_READBACK"
fi
exec "$FIXTURE_GIT" "$@"
"#,
    );
    let output = fixture
        .dots(&["backup"], &remote)
        .env("FIXTURE_READBACK", &readback)
        .output()
        .unwrap();
    let report = messages(&output);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert!(report.contains("native history sync failed"), "{report}");
    assert!(report.contains("checkpoint was kept locally"), "{report}");
    assert!(
        report.contains("publication could not be verified"),
        "{report}"
    );
    assert!(!report.contains("publication is pending"));
    assert!(!report.contains("Settings backup published"));
    assert!(!report.contains("synthetic-native-sync-rejection"));
    assert!(remote.join("pre-receive-ran").is_file());
    assert!(!readback.exists());
    assert_eq!(
        fixture
            .git(Some(&remote))
            .args(["show-ref", "--verify", "--quiet", REF])
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    let stored = ok(fixture
        .git(Some(&fixture.store()))
        .args(["show", "refs/heads/main:home@host-fixture/app/selected"]))
    .stdout;
    assert!(fixture.can_decrypt(&stored, &key));
}

#[test]
fn new_backup_probe_cancellation_matches_its_report() {
    history_probe_cancellation_matches_its_report(
        "backup",
        "could not read the effective history sync mode",
    );
}

#[test]
fn new_restore_probe_cancellation_matches_its_report() {
    history_probe_cancellation_matches_its_report(
        "restore",
        "could not confirm that the mise history watcher is stopped",
    );
}

fn history_probe_cancellation_matches_its_report(command: &str, diagnostic: &str) {
    use std::time::{Duration, Instant};

    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("unused.git");
    let ready = fixture.world.home().join("probe-ready");
    let probe = fixture.script(
        "pgrep",
        "#!/bin/sh\nprintf 'ready\\n' > \"$FIXTURE_READY\"\nexec sleep 30\n",
    );
    let mut child = fixture
        .dots(&[command], &remote)
        .env("DOTS_MISE_BIN", probe)
        .env("FIXTURE_READY", &ready)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() && Instant::now() < deadline && child.try_wait().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(15));
    }
    if !ready.exists() {
        let _ = child.kill();
        panic!(
            "history probe did not start: {:?}",
            child.wait_with_output().unwrap()
        );
    }
    // SAFETY: the PID belongs to the unreaped dots child in this fixture.
    assert_eq!(
        unsafe { nix::libc::kill(child.id() as i32, nix::libc::SIGINT) },
        0
    );
    let output = child.wait_with_output().unwrap();
    let report = fs::read_dir(fixture.world.home().join(".local/state/dots/reports"))
        .unwrap()
        .find_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension()? == "json").then(|| fs::read(path).unwrap())
        })
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&report).unwrap();
    assert_eq!(report["status"], 130, "{report}");
    assert_eq!(report["cancelled"], true);
    assert_eq!(report["stages"][0]["issues"][0]["diagnostic"], diagnostic);
    assert!(
        messages(&output).contains(diagnostic),
        "{}",
        messages(&output)
    );
    assert_eq!(
        output.status.code(),
        Some(130),
        "{command}: {}",
        messages(&output)
    );
}

#[test]
fn new_established_restore_can_undo_after_the_configured_recipient_changes() {
    established_restore_can_undo(false);
    established_restore_can_undo(true);
}

fn established_restore_can_undo(folder_override: bool) {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (selected, public) = seed.key("selected");
    let (_, other_public) = seed.key("other");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &selected, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let fixture = Fixture::new();
    fixture.configure(&url, &url, &selected, std::slice::from_ref(&public));
    fixture.write(DEFAULT, 0o600);
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);

    let root = fixture.world.root();
    let local = root.join("config.local.toml");
    let folder = if folder_override {
        remove_local_history_encryption(&fixture);
        history_fragment(root, "10-selected.toml", std::slice::from_ref(&public));
        let path = history_fragment(
            root,
            "replacement/mise.toml",
            std::slice::from_ref(&other_public),
        );
        let original = fs::read(&path).unwrap();
        Some((path, original))
    } else {
        let mut contents: toml_edit::DocumentMut =
            fs::read_to_string(&local).unwrap().parse().unwrap();
        contents["history"]["encryption"]["recipients"] = toml_edit::value(
            [other_public.as_str()]
                .into_iter()
                .collect::<toml_edit::Array>(),
        );
        fs::write(&local, contents.to_string()).unwrap();
        None
    };
    seed.seed(&remote, SECOND);
    let verify_refresh = fixture.script(
        "verify-refresh",
        r#"#!/bin/sh
if [ "$*" = "bootstrap dotfiles sync --fetch-only" ]; then
    recipients=$("$FIXTURE_MISE" --no-config config get --file "$FIXTURE_LOCAL_CONFIG" history.encryption.recipients) || exit 98
    case "$recipients" in
        *"$FIXTURE_SELECTED"*) ;;
        *) exit 98 ;;
    esac
fi
exec "$FIXTURE_MISE" "$@"
"#,
    );
    ok(fixture
        .dots(&["restore"], &remote)
        .env("DOTS_MISE_BIN", verify_refresh)
        .env("FIXTURE_MISE", &fixture.mise)
        .env("FIXTURE_LOCAL_CONFIG", &local)
        .env("FIXTURE_SELECTED", &public));
    assert_eq!(fs::read(fixture.active()).unwrap(), SECOND);
    if let Some((path, original)) = folder {
        assert_eq!(fs::read(path).unwrap(), original);
    }
    assert_eq!(
        contains(&fs::read(local).unwrap(), other_public.as_bytes()),
        !folder_override
    );

    let store = fixture.store();
    let head = fixture.tip(&store, "refs/heads/main");
    let checkpoint: serde_json::Value = serde_json::from_slice(
        &ok(&mut fixture.mise(&[
            "bootstrap",
            "dotfiles",
            "history",
            "show",
            &format!("commit:{head}"),
            "--json",
        ]))
        .stdout,
    )
    .unwrap();
    let undo = checkpoint["operation"]["before"]
        .as_str()
        .expect("native undo checkpoint");
    let envelope = ok(fixture
        .git(Some(&store))
        .args(["show", &format!("{undo}:home@host-fixture/app/selected")]))
    .stdout;
    assert!(envelope.starts_with(b"mise-encrypted-file-v1\n"));
    assert!(
        fixture.can_decrypt(&envelope, &selected),
        "selected identity cannot decrypt native undo"
    );
    ok(fixture
        .mise(&["bootstrap", "dotfiles", "undo", "--yes"])
        .env("MISE_AGE_IDENTITY_FILES", &selected)
        .env("GIT_ALLOW_PROTOCOL", ""));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
}

fn remove_local_history_encryption(fixture: &Fixture) -> PathBuf {
    let local = fixture.world.root().join("config.local.toml");
    let mut doc: toml_edit::DocumentMut = fs::read_to_string(&local).unwrap().parse().unwrap();
    doc["history"]
        .as_table_mut()
        .unwrap()
        .remove("encryption")
        .unwrap();
    fs::write(&local, doc.to_string()).unwrap();
    local
}

fn history_fragment(dir: &Path, name: &str, recipients: &[String]) -> PathBuf {
    let fragment = dir.join("conf.d").join(name);
    fs::create_dir_all(fragment.parent().unwrap()).unwrap();
    fs::write(
        &fragment,
        format!("[history.encryption]\nrecipients = {recipients:?}\n"),
    )
    .unwrap();
    fragment
}

#[test]
fn new_restore_accepts_trusted_history_fragments() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("fragment.git");
    let url = format!("file://{}", remote.display());
    let (identity, selected) = seed.key("selected.age");
    let (recovery, independent) = seed.key("fragment-only.age");
    let (shadowed, lower) = seed.key("lower-precedence.age");
    let recipients = [selected.clone(), independent.clone()];
    seed.configure(&url, &url, &identity, &[]);
    remove_local_history_encryption(&seed);
    history_fragment(seed.world.root(), "10-first.toml", &[lower]);
    let global = history_fragment(seed.world.root(), "50-history.toml", &recipients);
    let listing = ok(&mut seed.mise(&["config", "ls", "--json"]));
    let listing: serde_json::Value = serde_json::from_slice(&listing.stdout).unwrap();
    assert!(listing.as_array().unwrap().iter().any(|row| {
        row["path"]
            .as_str()
            .is_some_and(|path| Path::new(path) == global)
    }));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);
    let payload = ok(seed
        .git(Some(&remote))
        .arg("show")
        .arg(format!("{tip}:home@host-fixture/app/selected")))
    .stdout;
    assert!(payload.starts_with(b"mise-encrypted-file-v1\n"));
    assert!(seed.can_decrypt(&payload, &recovery));
    assert!(!seed.can_decrypt(&payload, &shadowed));

    for system in [false, true] {
        let target = Fixture::new();
        let key = target.world.home().join("selected.age");
        fs::copy(&identity, &key).unwrap();
        target.configure(&url, &url, &key, &[]);
        let local = remove_local_history_encryption(&target);
        let system_dir = target.world.home().join(".mise-system");
        let dir = if system {
            &system_dir
        } else {
            target.world.root()
        };
        let name = if system {
            "history/mise.laptop.local.toml"
        } else {
            "50-history.laptop.toml"
        };
        let fragment = history_fragment(dir, name, &recipients);
        let original_fragment = fs::read(&fragment).unwrap();
        let original_local = fs::read(&local).unwrap();
        target.write(DEFAULT, 0o600);

        if !system {
            fs::write(
                target.world.root().join("miserc.local.toml"),
                "# Tera syntax in a comment: {{ config_root }}\n",
            )
            .unwrap();
            let inactive = fragment.with_file_name("90-history.work.toml");
            fs::rename(&fragment, &inactive).unwrap();
            let refused = target
                .dots(&["restore"], &remote)
                .env("MISE_ENV_CONF_D", "1")
                .output()
                .unwrap();
            assert_eq!(refused.status.code(), Some(2), "{}", messages(&refused));
            assert!(messages(&refused).contains("configure [history.encryption].recipients"));
            assert!(!target.store().exists());
            fs::rename(&inactive, &fragment).unwrap();

            fs::write(&inactive, "[fixture-secret-marker\n").unwrap();
            let status_state = target.world.home().join("fragment-status");
            ok(target
                .mise(&["bootstrap", "dotfiles", "status", "--json"])
                .env("MISE_STATE_DIR", &status_state)
                .env("MISE_ENV_CONF_D", "1"));
            let active = target
                .command(&target.mise)
                .args(["-E", "work", "bootstrap", "dotfiles", "status", "--json"])
                .env("MISE_STATE_DIR", &status_state)
                .env("MISE_ENV_CONF_D", "1")
                .output()
                .unwrap();
            assert_eq!(active.status.code(), Some(1), "{}", messages(&active));
            let refused = target
                .dots(&["restore"], &remote)
                .env("MISE_FRIENDLY_ERROR", "0")
                .output()
                .unwrap();
            assert_eq!(refused.status.code(), Some(2), "{}", messages(&refused));
            let diagnostic = messages(&refused);
            assert!(diagnostic.contains("90-history.work.toml"), "{diagnostic}");
            assert!(
                diagnostic.contains("mise reported a TOML parse error"),
                "{diagnostic}"
            );
            assert!(
                !diagnostic.contains("fixture-secret-marker"),
                "{diagnostic}"
            );
            assert!(!target.store().exists());
            assert_eq!(fs::read(target.active()).unwrap(), DEFAULT);
            assert_eq!(fs::read(&local).unwrap(), original_local);
        }

        let alias = target.world.home().join("legacy-config.toml");
        fs::write(&alias, "[history.encryption]\nrecipients = []\n").unwrap();
        ok(target
            .dots(&["restore"], &remote)
            .env("MISE_ENV_CONF_D", "1")
            .env("MISE_CONFIG_FILE", &alias)
            .env("MISE_FRIENDLY_ERROR", "0"));
        assert_eq!(fs::read(target.active()).unwrap(), FIRST);
        assert_eq!(fs::read(&fragment).unwrap(), original_fragment);
        let written = fs::read(local).unwrap();
        assert!(contains(&written, selected.as_bytes()));
        assert!(!contains(&written, independent.as_bytes()));
        assert_eq!(seed.tip(&remote, REF), tip);
    }
}

#[test]
fn new_restore_does_not_promote_a_project_only_recipient() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (selected, public) = seed.key("selected");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &selected, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    let fixture = Fixture::new();
    let (project_key, project_public) = fixture.key("project");
    fixture.configure(&url, &url, &selected, std::slice::from_ref(&public));
    let root = fixture.world.root();
    let override_file = root.join("config.override.toml");
    let override_text = format!("[history.encryption]\nrecipients = [{project_public:?}]\n");
    fs::write(&override_file, &override_text).unwrap();
    fixture.write(DEFAULT, 0o600);
    let local_file = root.join("config.local.toml");
    let original = fs::read_to_string(&local_file).unwrap();
    let mut without_recipients: toml_edit::DocumentMut = original.parse().unwrap();
    without_recipients["history"]
        .as_table_mut()
        .unwrap()
        .remove("encryption");
    fs::write(&local_file, without_recipients.to_string()).unwrap();
    let refused = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(refused.status.code(), Some(2), "{}", messages(&refused));
    assert!(messages(&refused).contains("configure [history.encryption].recipients"));
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert!(!fixture.store().exists());
    fs::write(&local_file, original).unwrap();

    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert!(
        fs::read(override_file).unwrap() == override_text.as_bytes(),
        "restore modified the project override"
    );
    let local = fs::read(local_file).unwrap();
    assert!(
        contains(&local, public.as_bytes()),
        "restore lost the selected recipient"
    );
    assert!(
        !contains(&local, project_public.as_bytes()),
        "restore persisted a recipient from the project override"
    );

    let store = fixture.store();
    let commits = text(ok(fixture.git(Some(&store)).args([
        "rev-list",
        "refs/heads/main",
        "--not",
        &tip,
    ])));
    let mut checked = 0;
    for commit in commits.lines() {
        let manifest = ok(fixture
            .git(Some(&store))
            .args(["show", &format!("{commit}:.mise-history/manifest.json")]))
        .stdout;
        assert!(
            !contains(&manifest, project_public.as_bytes()),
            "a new checkpoint recorded the project recipient"
        );
        let payload = fixture
            .git(Some(&store))
            .args(["show", &format!("{commit}:home@host-fixture/app/selected")])
            .output()
            .unwrap();
        if payload.status.success() {
            assert!(payload.stdout.starts_with(b"mise-encrypted-file-v1\n"));
            assert!(
                fixture.can_decrypt(&payload.stdout, &selected),
                "selected identity could not decrypt a new checkpoint"
            );
            assert!(
                !fixture.can_decrypt(&payload.stdout, &project_key),
                "project identity decrypted a new checkpoint"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no new encrypted payloads were checked");
    ok(fixture
        .mise(&["bootstrap", "dotfiles", "undo", "--yes"])
        .env("MISE_AGE_IDENTITY_FILES", &selected)
        .env("GIT_ALLOW_PROTOCOL", ""));
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert_eq!(seed.tip(&remote, REF), tip);
}

#[test]
fn new_first_restore_keeps_an_edit_made_during_native_fetch() {
    restore_keeps_an_edit_made_during_native_fetch(false);
}

#[test]
fn new_established_restore_keeps_an_edit_made_during_native_fetch() {
    restore_keeps_an_edit_made_during_native_fetch(true);
}

fn restore_keeps_an_edit_made_during_native_fetch(established: bool) {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o600);
    if established {
        ok(&mut fixture.dots(&["restore"], &remote));
        assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    }
    let tip = seed.tip(&remote, REF);
    let marker = fixture.world.home().join("edited-during-fetch");
    let mise = fixture.script(
        "edit-at-native-fetch",
        r#"#!/bin/sh
case " $* " in
    *" bootstrap dotfiles sync --fetch-only "*) export FIXTURE_EDIT_DURING_FETCH=1 ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
    );
    fixture.script(
        "git",
        r#"#!/bin/sh
set -eu
if [ "${FIXTURE_EDIT_DURING_FETCH-}" = 1 ] && [ ! -e "$FIXTURE_EDIT_MARKER" ]; then
    for arg do
        if [ "$arg" = fetch ]; then
            printf '%s' "$FIXTURE_EDIT_CONTENTS" > "$FIXTURE_EDIT_TARGET"
            printf 'edited\n' > "$FIXTURE_EDIT_MARKER"
            break
        fi
    done
fi
exec "$FIXTURE_GIT" "$@"
"#,
    );
    let output = fixture
        .dots(&["restore"], &remote)
        .env("DOTS_MISE_BIN", mise)
        .env("FIXTURE_MISE", &fixture.mise)
        .env("FIXTURE_EDIT_TARGET", fixture.active())
        .env("FIXTURE_EDIT_MARKER", &marker)
        .env(
            "FIXTURE_EDIT_CONTENTS",
            std::str::from_utf8(LOCAL_EDIT).unwrap(),
        )
        .output()
        .unwrap();
    assert!(marker.is_file(), "native fetch injection was never reached");
    assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
    assert_eq!(output.status.code(), Some(1), "{}", messages(&output));
    assert!(messages(&output).contains("changed after restore started"));
    assert_fetching_retry(&fixture, &remote, established);
    assert_eq!(seed.tip(&remote, REF), tip);
}

#[test]
fn new_restore_reobserves_a_live_replacement_after_offline_fetch() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    for established in [false, true] {
        let fixture = Fixture::new();
        fixture.configure(&url, &url, &key, std::slice::from_ref(&public));
        fixture.write(DEFAULT, 0o600);
        if established {
            ok(&mut fixture.dots(&["restore"], &remote));
        }
        let failed = fixture.offline_dots("restore", &remote);
        assert_eq!(failed.status.code(), Some(1), "{}", messages(&failed));
        assert!(messages(&failed).contains("mise could not fetch encrypted history"));
        assert_eq!(
            fs::read(fixture.active()).unwrap(),
            if established { FIRST } else { DEFAULT }
        );
        let old_inode = fs::metadata(fixture.active()).unwrap().ino();
        let replacement = fixture.world.home().join("replacement");
        fs::write(&replacement, LOCAL_EDIT).unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o640)).unwrap();
        fs::rename(replacement, fixture.active()).unwrap();
        let metadata = fs::metadata(fixture.active()).unwrap();
        assert_ne!(metadata.ino(), old_inode);
        assert_eq!(metadata.mode() & 0o777, 0o640);

        let head = fixture.tip(&fixture.store(), "HEAD");
        let backup = fixture.offline_dots("backup", &remote);
        assert_eq!(backup.status.code(), Some(1), "{}", messages(&backup));
        if established {
            assert!(messages(&backup).contains("the settings checkpoint was kept locally"));
            assert_ne!(fixture.tip(&fixture.store(), "HEAD"), head);
        } else {
            assert!(messages(&backup).contains("a first restore is incomplete"));
            assert_eq!(fixture.tip(&fixture.store(), "HEAD"), head);
            assert_eq!(seed.tip(&remote, REF), tip);
        }
        assert_fetching_retry(&fixture, &remote, established);
        assert_eq!(seed.tip(&remote, REF), tip);
    }
}

fn assert_fetching_retry(fixture: &Fixture, remote: &Path, established: bool) {
    let marker = fixture.store().join("dots-restore-pending");
    let journal: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert!(journal["progress"]["Fetching"].is_object());
    ok(&mut fixture.dots(&["restore"], remote));
    assert!(!marker.exists());
    assert_eq!(
        fs::metadata(fixture.active()).unwrap().mode() & 0o777,
        0o600
    );
    if !established {
        assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
        assert_restore_undo(fixture, "rollback", LOCAL_EDIT);
    }
    assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
}

fn assert_restore_undo(fixture: &Fixture, kind: &str, previous: &[u8]) {
    let head = fixture.tip(&fixture.store(), "HEAD");
    let checkpoint: serde_json::Value = serde_json::from_slice(
        &ok(&mut fixture.mise(&[
            "bootstrap",
            "dotfiles",
            "history",
            "show",
            &format!("commit:{head}"),
            "--json",
        ]))
        .stdout,
    )
    .unwrap();
    assert_eq!(checkpoint["operation"]["kind"], kind, "{checkpoint}");
    assert_eq!(checkpoint["operation"]["status"], "completed");
    ok(fixture
        .mise(&["bootstrap", "dotfiles", "undo", "--yes"])
        .env("GIT_ALLOW_PROTOCOL", ""));
    assert_eq!(fs::read(fixture.active()).unwrap(), previous);
}

#[test]
fn new_first_restore_reobserves_before_native_pull_and_can_undo() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o600);
    let offline = fixture.offline_dots("restore", &remote);
    assert!(messages(&offline).contains("mise could not fetch encrypted history"));
    let marker = fixture.store().join("dots-restore-pending");
    let journal: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(journal["initial"], true);
    assert!(journal["progress"]["Fetching"].is_object());
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);

    // Bring the live file to the cloned baseline so the next remote revision
    // can be applied by native pull without a merge conflict.
    fixture.write(FIRST, 0o600);
    seed.seed(&remote, REMOTE_EDIT);
    let tip = seed.tip(&remote, REF);
    ok(&mut fixture.dots(&["restore"], &remote));
    assert!(!marker.exists());
    assert_eq!(fs::read(fixture.active()).unwrap(), REMOTE_EDIT);
    assert_restore_undo(&fixture, "apply", FIRST);
    assert_eq!(seed.tip(&remote, REF), tip);
}

#[test]
fn new_first_restore_rebinds_a_previously_missing_parent_after_offline_fetch() {
    restore_rebinds_a_previously_missing_parent_after_offline_fetch(false);
}

#[test]
fn new_established_restore_rebinds_a_previously_missing_parent_after_offline_fetch() {
    restore_rebinds_a_previously_missing_parent_after_offline_fetch(true);
}

fn restore_rebinds_a_previously_missing_parent_after_offline_fetch(established: bool) {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    let fixture = Fixture::new();
    for host in [&seed, &fixture] {
        host.configure(&url, &url, &key, std::slice::from_ref(&public));
        fs::OpenOptions::new()
            .append(true)
            .open(host.world.root().join("config.toml"))
            .unwrap()
            .write_all(
                br#""~/new-parent/selected" = { mode = "track", encrypt = true, variants = [{ profile = "host-fixture" }] }
"#,
            )
            .unwrap();
    }
    seed.seed(&remote, FIRST);
    fixture.write(DEFAULT, 0o600);
    if established {
        ok(&mut fixture.dots(&["restore"], &remote));
    }
    let live = if established { FIRST } else { DEFAULT };
    let offline = fixture.offline_dots("restore", &remote);
    assert!(messages(&offline).contains("mise could not fetch encrypted history"));
    let marker = fixture.store().join("dots-restore-pending");
    let fetching = fs::read(&marker).unwrap();
    let journal: serde_json::Value = serde_json::from_slice(&fetching).unwrap();
    assert_eq!(journal["initial"], !established);
    assert!(journal["progress"]["Fetching"].is_object());

    let seed_parent = seed.world.home().join("new-parent");
    fs::create_dir(&seed_parent).unwrap();
    let seed_file = seed_parent.join("selected");
    fs::write(&seed_file, SECOND).unwrap();
    fs::set_permissions(&seed_file, fs::Permissions::from_mode(0o600)).unwrap();
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    let parent = fixture.world.home().join("new-parent");
    let other = fixture.world.home().join("app");
    symlink(&other, &parent).unwrap();
    let refused = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(refused.status.code(), Some(2), "{}", messages(&refused));
    assert!(
        messages(&refused).contains("new-parent"),
        "{}",
        messages(&refused)
    );
    assert_eq!(fs::read(&marker).unwrap(), fetching);
    assert_eq!(fs::read(fixture.active()).unwrap(), live);
    fs::remove_file(&parent).unwrap();

    fs::write(&parent, LOCAL_EDIT).unwrap();
    let refused = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(refused.status.code(), Some(2), "{}", messages(&refused));
    assert!(
        messages(&refused).contains("new-parent"),
        "{}",
        messages(&refused)
    );
    assert_eq!(fs::read(&marker).unwrap(), fetching);
    assert_eq!(fs::read(&parent).unwrap(), LOCAL_EDIT);
    fs::remove_file(&parent).unwrap();

    fs::create_dir(&parent).unwrap();
    ok(&mut fixture.dots(&["restore"], &remote));
    assert!(!marker.exists());
    assert_eq!(fs::read(parent.join("selected")).unwrap(), SECOND);
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    if !established {
        assert_restore_undo(&fixture, "rollback", DEFAULT);
    }
    assert_eq!(seed.tip(&remote, REF), tip);
}

#[test]
fn new_established_restore_retries_after_an_overlapping_conflict_is_resolved() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(FIRST, 0o600);
    ok(&mut fixture.dots(&["restore"], &remote));
    seed.seed(&remote, REMOTE_EDIT);
    let tip = seed.tip(&remote, REF);
    fixture.write(LOCAL_EDIT, 0o600);

    let marker = fixture.store().join("dots-restore-pending");
    let refused = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(refused.status.code(), Some(1), "{}", messages(&refused));
    assert!(messages(&refused).contains("both sides changed the same lines"));
    assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
    assert_eq!(
        fixture.tip(&fixture.store(), "refs/remotes/origin/setup"),
        tip
    );
    let retired = !marker.exists();

    // Remove the overlapping local change so the incoming version can apply.
    fixture.write(FIRST, 0o640);
    ok(&mut fixture.dots(&["restore"], &remote));
    assert!(
        retired,
        "the verified pre-apply conflict left a restore marker"
    );
    assert!(!marker.exists());
    assert_eq!(fs::read(fixture.active()).unwrap(), REMOTE_EDIT);
    assert_eq!(seed.tip(&remote, REF), tip);
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
    let backup = fixture.dots(&["backup"], &remote).output().unwrap();
    assert_eq!(backup.status.code(), Some(1), "{}", messages(&backup));
    assert!(messages(&backup).contains("rerun dots restore before saving history"));
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert!(!fixture.store().join("dots-restore-pending").exists());
    assert_eq!(fixture.tip(&remote, REF), initial);
    fixture.assert_encrypted(&fixture.store(), [&key, &partner, &wrong]);
}

#[test]
fn new_established_interrupted_restore_explains_recovery_in_each_command() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let source = seed.world.bare_remote("source.git");
    let (key, public) = seed.key("recovery");
    let origin = format!("file://{}", remote.display());
    seed.configure(&origin, &origin, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);

    let fixture = Fixture::new();
    fixture.configure(&origin, &origin, &key, &[public]);
    let source = format!("file://{}", source.display());
    let config = fixture.world.root().join("config.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        config,
        text.replace("[vars]", &format!("[vars]\ndots_source_repo = {source:?}")),
    )
    .unwrap();
    ok(fixture.git(None).args(["init", "-b", "dev"]));
    ok(fixture.git(None).args(["add", "config.toml"]));
    ok(fixture.git(None).args(["commit", "-m", "Fixture source"]));
    ok(fixture.git(None).args(["remote", "add", "origin", &source]));
    ok(fixture.git(None).args(["push", "origin", "dev"]));
    ok(&mut fixture.dots(&["restore"], &remote));
    seed.seed(&remote, REMOTE_EDIT);

    let wrapper = fixture.script(
        "interrupt-established-pull",
        r#"#!/bin/sh
case " $* " in
  *" bootstrap dotfiles pull "*) exit 93 ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
    );
    let interrupted = fixture
        .dots(&["restore"], &remote)
        .env("DOTS_MISE_BIN", wrapper)
        .env("FIXTURE_MISE", &fixture.mise)
        .output()
        .unwrap();
    assert_eq!(
        interrupted.status.code(),
        Some(1),
        "{}",
        messages(&interrupted)
    );
    let marker = fixture.store().join("dots-restore-pending");
    let original = fs::read(&marker).unwrap();
    let journal: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(journal["initial"], false);
    assert!(journal["progress"]["Pulling"].is_object());
    fixture.write(LOCAL_EDIT, 0o600);

    for (command, status) in [("restore", 1), ("up", 1), ("backup", 1), ("sync", 0)] {
        let output = fixture.dots(&[command], &remote).output().unwrap();
        let diagnostic = messages(&output);
        assert_eq!(
            output.status.code(),
            Some(status),
            "{command}: {diagnostic}"
        );
        for text in [
            "An earlier restore was interrupted and a tracked settings file changed afterwards.",
            "mise dot history --pending",
            "mise dot recover UUID",
            "Otherwise, reconcile the file and run dots restore.",
        ] {
            assert!(diagnostic.contains(text), "{command}: {diagnostic}");
        }
        assert_eq!(fs::read(&marker).unwrap(), original);
        assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
    }
}

#[test]
fn new_interrupted_pull_accepts_only_equivalent_rewrites() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let origin = format!("file://{}", remote.display());
    seed.configure(&origin, &origin, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    let mut changed = DEFAULT.to_vec();
    changed[0] = b'P';
    for (bytes, mode, legacy, resumes) in [
        (DEFAULT, 0o600, false, true),
        (changed.as_slice(), 0o600, false, false),
        (DEFAULT, 0o640, false, false),
        (DEFAULT, 0o600, true, false),
    ] {
        let fixture = Fixture::new();
        fixture.configure(&origin, &origin, &key, std::slice::from_ref(&public));
        fixture.write(DEFAULT, 0o600);
        let wrapper = fixture.script(
            "interrupt-pull-before-write",
            r#"#!/bin/sh
case " $* " in
  *" bootstrap dotfiles pull "*) exit 93 ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
        );
        let interrupted = fixture
            .dots(&["restore"], &remote)
            .env("DOTS_MISE_BIN", wrapper)
            .env("FIXTURE_MISE", &fixture.mise)
            .output()
            .unwrap();
        assert_eq!(
            interrupted.status.code(),
            Some(1),
            "{}",
            messages(&interrupted)
        );
        let marker = fixture.store().join("dots-restore-pending");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        let file = &mut journal["progress"]["Pulling"]["live"]
            [fixture.active().canonicalize().unwrap().to_str().unwrap()];
        let digest = file["sha256"].as_str().expect("saved content digest");
        assert_eq!(digest.len(), 64);
        if legacy {
            file.as_object_mut().unwrap().remove("sha256");
            fs::write(&marker, serde_json::to_vec(&journal).unwrap()).unwrap();
        }
        let head = fixture.tip(&fixture.store(), "HEAD");
        let old_inode = fs::metadata(fixture.active()).unwrap().ino();
        let replacement = fixture.world.home().join("replacement");
        fs::write(&replacement, bytes).unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(mode)).unwrap();
        fs::rename(replacement, fixture.active()).unwrap();
        assert_ne!(fs::metadata(fixture.active()).unwrap().ino(), old_inode);

        let result = fixture.dots(&["restore"], &remote).output().unwrap();
        if resumes {
            assert!(result.status.success(), "{}", messages(&result));
            assert!(!marker.exists());
            assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
            assert_restore_undo(&fixture, "rollback", DEFAULT);
        } else {
            assert_eq!(result.status.code(), Some(1), "{}", messages(&result));
            assert!(messages(&result).contains("changed after restore started"));
            assert!(marker.exists());
            assert_eq!(fixture.tip(&fixture.store(), "HEAD"), head);
            assert_eq!(fs::read(fixture.active()).unwrap(), bytes);
            assert_eq!(fs::metadata(fixture.active()).unwrap().mode() & 0o777, mode);
        }
        assert_eq!(seed.tip(&remote, REF), tip);
    }
}

#[test]
fn new_restore_with_each_approved_identity_and_native_undo_recovers_the_live_default() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (first, first_public) = seed.key("first");
    let (second, second_public) = seed.key("second");
    let (unrelated, _) = seed.key("unrelated");
    let recipients = [first_public, second_public];
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &first, &recipients);
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    for identity in [&first, &second] {
        let fixture = Fixture::new();
        fixture.configure(&url, &url, identity, &recipients);
        let local = fixture.world.root().join("config.local.toml");
        let config = fs::read_to_string(&local).unwrap().replace(
            &format!("identity_files = [{identity:?}]"),
            &format!("identity_files = [{unrelated:?}]"),
        );
        fs::write(&local, config).unwrap();
        fixture.write(DEFAULT, 0o644);
        ok(&mut fixture.dots(&["restore"], &remote));
        assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
        assert_eq!(
            fs::metadata(fixture.active()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fixture.assert_encrypted(&fixture.store(), [&first, &second, &unrelated]);
        let config: toml_edit::DocumentMut = fs::read_to_string(local).unwrap().parse().unwrap();
        let identities = config["settings"]["age"]["identity_files"]
            .as_array()
            .unwrap();
        assert_eq!(
            identities
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect::<Vec<_>>(),
            [unrelated.to_str().unwrap(), identity.to_str().unwrap()]
        );
        ok(fixture
            .mise(&["bootstrap", "dotfiles", "undo", "--yes"])
            .env("GIT_ALLOW_PROTOCOL", ""));
        assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
        fixture.assert_encrypted(&fixture.store(), [&first, &second, &unrelated]);
        for private in ["state", "cache", "tmp"] {
            assert_no_plaintext(&fixture.world.home().join(private));
        }
        assert_eq!(seed.tip(&remote, REF), tip);
    }

    let wrong = Fixture::new();
    wrong.configure(&url, &url, &unrelated, &recipients);
    wrong.write(DEFAULT, 0o600);
    let output = wrong.dots(&["restore"], &remote).output().unwrap();
    assert!(!output.status.success(), "{}", messages(&output));
    assert_eq!(fs::read(wrong.active()).unwrap(), DEFAULT);
    assert_eq!(seed.tip(&remote, REF), tip);
}

#[test]
fn new_restore_finalizes_after_native_rollback_without_overwriting_a_later_edit() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o600);
    let wrapper = fixture.script(
        "interrupt-after-rollback",
        r#"#!/bin/sh
case " $* " in
  *" bootstrap dotfiles rollback "*" --yes "*)
    "$FIXTURE_MISE" "$@" || exit "$?"
    exit 92
    ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
    );
    let interrupted = fixture
        .dots(&["restore"], &remote)
        .env("DOTS_MISE_BIN", wrapper)
        .env("FIXTURE_MISE", &fixture.mise)
        .output()
        .unwrap();
    assert_eq!(
        interrupted.status.code(),
        Some(1),
        "{}",
        messages(&interrupted)
    );
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert!(fixture.store().join("dots-restore-pending").is_file());
    let local = fixture.tip(&fixture.store(), "refs/heads/main");
    fixture.write(LOCAL_EDIT, 0o600);
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
    assert_eq!(fixture.tip(&fixture.store(), "refs/heads/main"), local);
    assert_eq!(seed.tip(&remote, REF), tip);
    assert!(!fixture.store().join("dots-restore-pending").exists());
}

#[test]
fn new_restore_recovers_its_interrupted_native_write_when_there_is_no_later_edit() {
    assert_interrupted_native_rollback("finished");
}

#[test]
fn new_restore_resumes_an_owned_native_rollback_before_the_target_is_recorded() {
    assert_interrupted_native_rollback("pending");
}

fn assert_interrupted_native_rollback(phase: &str) {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    for later_edit in [false, true] {
        let fixture = Fixture::new();
        fixture.configure(&url, &url, &key, std::slice::from_ref(&public));
        fixture.write(DEFAULT, 0o600);
        let interrupt_trace = fixture.world.home().join("native-interrupt");
        let interrupted = fixture.script(
            "interrupt-native-rollback",
            r#"#!/bin/sh
set -eu
case " $* " in
  *" bootstrap dotfiles rollback "*" --yes "*)
    printf 'armed\n' > "$FIXTURE_NATIVE_INTERRUPT"
    export FIXTURE_STOP_NATIVE=1
    ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
        );
        fixture.script(
            "git",
            r#"#!/bin/sh
set -eu
if [ "${FIXTURE_STOP_NATIVE-}" = 1 ]; then
    for record in "$MISE_STATE_DIR"/history/index/pending/*.json; do
        [ -f "$record" ] || continue
        committed=0
        recovery=
        while IFS= read -r line; do
            case "$line" in
                *'"kind": "committed"'*) committed=1 ;;
                *'"recovery": "pending"'*) recovery=pending ;;
                *'"recovery": "finished"'*) recovery=finished ;;
            esac
        done < "$record"
        case "$FIXTURE_INTERRUPT_PHASE:$recovery:$committed" in
            pending:pending:0|finished:finished:1)
                printf 'hit\n' >> "$FIXTURE_NATIVE_INTERRUPT"
                kill -KILL "$PPID"
                exit 94
                ;;
        esac
    done
fi
exec "$FIXTURE_GIT" "$@"
"#,
        );
        let output = fixture
            .dots(&["restore"], &remote)
            .env("DOTS_MISE_BIN", interrupted)
            .env("FIXTURE_MISE", &fixture.mise)
            .env("FIXTURE_NATIVE_INTERRUPT", &interrupt_trace)
            .env("FIXTURE_INTERRUPT_PHASE", phase)
            .output()
            .unwrap();
        assert_eq!(
            fs::read_to_string(&interrupt_trace).unwrap_or_default(),
            "armed\nhit\n",
            "native rollback never reached {phase} (later_edit={later_edit}): {}",
            messages(&output)
        );
        assert_eq!(output.status.code(), Some(1), "{}", messages(&output));
        let store = fixture.store();
        let marker = store.join("dots-restore-pending");
        assert!(marker.is_file());
        let pending = store.parent().unwrap().join("index/pending");
        let record = fs::read_dir(&pending)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let snapshot = fs::read(&record).unwrap();
        let native: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
        let operation = &native["checkpoint"]["operation"];
        assert_eq!(native["recovery"], phase);
        let journal = operation["journal"].as_array().unwrap();
        if phase == "finished" {
            assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
            assert_eq!(operation["status"], "completed");
            assert!(journal.iter().any(|entry| entry["kind"] == "committed"));
            assert_eq!(
                fixture.tip(&store, "refs/heads/main"),
                operation["before"]
                    .as_str()
                    .expect("native undo checkpoint"),
                "the native outcome was already committed when the interruption fired"
            );
        } else {
            assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
            assert_eq!(operation["status"], "pending");
            assert!(operation["to"].is_null() && operation["applied"].is_null());
            assert!(journal.is_empty());
        }
        if later_edit {
            fixture.write(LOCAL_EDIT, 0o600);
            let retry = fixture.dots(&["restore"], &remote).output().unwrap();
            assert_eq!(retry.status.code(), Some(1), "{}", messages(&retry));
            assert!(
                messages(&retry).contains("mise dot history --pending"),
                "{}",
                messages(&retry)
            );
            assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
            assert!(marker.is_file());
            assert_eq!(fs::read(&record).unwrap(), snapshot);
        } else {
            ok(&mut fixture.dots(&["restore"], &remote));
            assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
            assert!(!marker.exists());
            assert!(!record.exists());
        }
        assert_eq!(fixture.tip(&remote, REF), tip);
    }
}

#[test]
fn new_restore_refuses_a_wider_checkpoint_mode_before_native_apply() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.write(FIRST, 0o640);
    ok(&mut seed.mise(&["bootstrap", "dotfiles", "save"]));
    ok(seed
        .git(Some(&seed.store()))
        .args(["push", "--"])
        .arg(&remote)
        .arg(format!("HEAD:{REF}")));
    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o644);
    let result = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(result.status.code(), Some(2), "{}", messages(&result));
    assert!(messages(&result).contains("unsafe path, stream, object or recorded mode"));
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert_eq!(
        fs::metadata(fixture.active()).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(!fixture.store().exists());
}

#[test]
fn new_restore_refuses_the_inner_symlink_mode_at_a_missing_target() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    symlink(seed.inactive(), seed.active()).unwrap();
    ok(&mut seed.mise(&["bootstrap", "dotfiles", "save"]));
    let saved = seed.store();
    let stream = "home@host-fixture/app/selected";
    let envelope = ok(seed
        .git(Some(&saved))
        .args(["show", &format!("refs/heads/main:{stream}")]))
    .stdout;
    assert!(envelope.starts_with(b"mise-encrypted-file-v1\n"));
    assert!(contains(&envelope, b"\xa4mode\xa6120000"));

    // Keep the real native envelope. Give its outer Git blob the same
    // recorded permissions as a private file, so only the inner type differs.
    let checkout = seed.world.home().join("test-checkout");
    ok(seed
        .git(None)
        .args(["clone", "--no-local"])
        .arg(&saved)
        .arg(&checkout));
    let manifest = checkout.join(".mise-history/manifest.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    record["permissions"][stream] = 0o600.into();
    fs::write(manifest, serde_json::to_vec(&record).unwrap()).unwrap();
    ok(seed
        .git(None)
        .current_dir(&checkout)
        .args(["add", ".mise-history/manifest.json"]));
    ok(seed
        .git(None)
        .current_dir(&checkout)
        .args(["commit", "-m", "Record private fixture mode"]));
    ok(seed
        .git(None)
        .current_dir(&checkout)
        .args(["push", "--"])
        .arg(&remote)
        .arg(format!("HEAD:{REF}")));

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    let output = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", messages(&output));
    assert!(messages(&output).contains("unsafe path, stream, object or recorded mode"));
    assert!(fixture.active().symlink_metadata().is_err());
    assert!(!fixture.store().exists());
}

#[test]
fn new_pending_restore_rejects_a_different_configuration_store_or_seed() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let initial = seed.tip(&remote, REF);

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o600);
    let wrapper = fixture.script(
        "interrupt-pull",
        r#"#!/bin/sh
case " $* " in
  *" bootstrap dotfiles pull "*) exit 93 ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
    );
    let interrupted = fixture
        .dots(&["restore"], &remote)
        .env("DOTS_MISE_BIN", wrapper)
        .env("FIXTURE_MISE", &fixture.mise)
        .output()
        .unwrap();
    assert_eq!(
        interrupted.status.code(),
        Some(1),
        "{}",
        messages(&interrupted)
    );
    let store = fixture.store();
    let pending = store.join("dots-restore-pending");
    let saved_marker = fs::read(&pending).unwrap();

    let alternate = fixture.world.root().with_file_name("alternate-config");
    fs::create_dir(&alternate).unwrap();
    for file in [
        "config.toml",
        "config.laptop.toml",
        "miserc.toml",
        "config.local.toml",
    ] {
        fs::copy(fixture.world.root().join(file), alternate.join(file)).unwrap();
    }
    let mismatch = fixture
        .dots(&["restore"], &remote)
        .env("DOTS_ROOT", &alternate)
        .env("MISE_CONFIG_DIR", &alternate)
        .output()
        .unwrap();
    assert_eq!(mismatch.status.code(), Some(2), "{}", messages(&mismatch));
    assert!(messages(&mismatch).contains("another configuration root"));

    let displaced = store.with_file_name("original.git");
    fs::rename(&store, &displaced).unwrap();
    ok(fixture
        .git(None)
        .args(["clone", "--bare", "--no-local"])
        .arg(&displaced)
        .arg(&store));
    fs::write(&pending, &saved_marker).unwrap();
    let mismatch = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(mismatch.status.code(), Some(2), "{}", messages(&mismatch));
    assert!(messages(&mismatch).contains("different store"));
    fs::rename(&store, store.with_file_name("substitute.git")).unwrap();
    fs::rename(&displaced, &store).unwrap();

    let main = fixture.tip(&store, "refs/heads/main");
    let foreign = fixture.tip(&store, "refs/heads/main^{tree}");
    let unrelated = text(ok(fixture.git(Some(&store)).args([
        "commit-tree",
        &foreign,
        "-m",
        "unrelated fixture seed",
    ])));
    ok(fixture
        .git(Some(&store))
        .args(["update-ref", "refs/heads/main", &unrelated, &main]));
    let mismatch = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(mismatch.status.code(), Some(2), "{}", messages(&mismatch));
    assert!(messages(&mismatch).contains("no longer contains its seed"));
    assert_eq!(fixture.tip(&store, "refs/heads/main"), unrelated);
    assert_eq!(fs::read(&pending).unwrap(), saved_marker);
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert_eq!(seed.tip(&remote, REF), initial);

    for replacement in [true, false] {
        let graft = store.join("info/grafts");
        if replacement {
            ok(fixture
                .git(Some(&store))
                .args(["replace", &unrelated, &main]));
        } else {
            fs::write(&graft, format!("{unrelated} {initial}\n")).unwrap();
        }
        ok(fixture
            .git(Some(&store))
            .args(["merge-base", "--is-ancestor", &initial, &unrelated]));
        let changed_graph = fixture.dots(&["restore"], &remote).output().unwrap();
        assert_eq!(
            changed_graph.status.code(),
            Some(2),
            "{}",
            messages(&changed_graph)
        );
        assert!(messages(&changed_graph).contains("no longer contains its seed"));
        assert_eq!(fs::read(&pending).unwrap(), saved_marker);
        assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
        if replacement {
            ok(fixture
                .git(Some(&store))
                .args(["replace", "-d", &unrelated]));
        } else {
            fs::remove_file(graft).unwrap();
        }
    }

    ok(fixture
        .git(Some(&store))
        .args(["update-ref", "refs/heads/main", &main, &unrelated]));
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert!(!pending.exists());
}

#[test]
fn new_restore_uses_the_native_connection_when_the_bare_store_has_no_git_origin() {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("private.git");
    let (key, public) = fixture.key("recovery");
    let url = format!("file://{}", remote.display());
    fixture.configure(&url, &url, &key, std::slice::from_ref(&public));
    fixture.seed(&remote, FIRST);
    ok(&mut fixture.mise(&["bootstrap", "dotfiles", "sync", "--fetch-only"]));
    let no_origin = fixture
        .git(Some(&fixture.store()))
        .args(["config", "--local", "--get", "remote.origin.url"])
        .output()
        .unwrap();
    assert_eq!(no_origin.status.code(), Some(1));

    let sender = Fixture::new();
    sender.configure(&url, &url, &key, &[public]);
    fs::create_dir_all(sender.store().parent().unwrap()).unwrap();
    ok(sender
        .git(None)
        .args(["clone", "--bare", "--single-branch", "--branch", BRANCH])
        .arg(&remote)
        .arg(sender.store()));
    ok(sender
        .git(Some(&sender.store()))
        .args(["branch", "-m", "main"]));
    sender.seed(&remote, SECOND);
    let incoming = sender.tip(&remote, REF);
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), SECOND);
    assert_eq!(fixture.tip(&remote, REF), incoming);
    assert!(!fixture.store().join("dots-restore-pending").exists());
}

#[test]
fn new_restore_local_inspection_never_follows_an_alternate_promisor() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    ok(seed
        .git(Some(&remote))
        .args(["config", "uploadpack.allowFilter", "true"]));

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o600);
    let store = fixture.store();
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    ok(fixture
        .git(None)
        .args([
            "clone",
            "--bare",
            "--no-local",
            "--filter=blob:none",
            "--single-branch",
            "--branch",
            BRANCH,
            "--origin",
            "promisor",
            &url,
        ])
        .arg(&store));
    ok(fixture.git(Some(&store)).args(["branch", "-m", "main"]));
    for metadata in [
        "HEAD:.mise-history/manifest.json",
        "HEAD:.mise-history/format.toml",
    ] {
        ok(fixture.git(Some(&store)).args(["show", metadata]));
    }
    ok(fixture.git(Some(&store)).args([
        "config",
        "remote.promisor.url",
        "ssh://git@fixture-promisor.invalid/alternate",
    ]));

    let result = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(result.status.code(), Some(1), "{}", messages(&result));
    let diagnostic = messages(&result);
    assert!(
        diagnostic.contains("could not inspect the encrypted checkpoint envelope"),
        "{diagnostic}"
    );
    assert!(fixture.trace().is_empty());
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);

    // Positive control on the same missing object. With SSH allowed, Git
    // invokes our fixture stub, which records the call and never dials out.
    let control = fixture
        .git(Some(&store))
        .args(["show", "HEAD:home@host-fixture/app/selected"])
        .env("GIT_ALLOW_PROTOCOL", "ssh")
        .env("GIT_SSH_COMMAND", fixture.world.home().join("bin/ssh"))
        .env("GIT_SSH_VARIANT", "ssh")
        .env("FIXTURE_TRANSPORT", fixture.world.home().join("transport"))
        .output()
        .unwrap();
    assert!(!control.status.success());
    assert!(
        fixture
            .trace()
            .iter()
            .any(|line| line.contains("/alternate"))
    );
}

#[test]
fn new_restore_ignores_an_inherited_global_config_for_another_origin() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);

    let alternate = Fixture::new();
    let wrong = alternate.world.bare_remote("wrong.git");
    let wrong_url = format!("file://{}", wrong.display());
    alternate.configure(&wrong_url, &wrong_url, &key, std::slice::from_ref(&public));
    alternate.seed(&wrong, REMOTE_EDIT);
    let other_tip = alternate.tip(&wrong, REF);

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o600);
    let override_dir = fixture.world.home().join("override");
    fs::create_dir(&override_dir).unwrap();
    let override_file = override_dir.join("config.toml");
    let wrong_config = format!("[history.origin]\nurl = {wrong_url:?}\nbranch = {BRANCH:?}\n");
    fs::write(&override_file, &wrong_config).unwrap();
    ok(fixture
        .dots(&["restore"], &remote)
        .env("MISE_GLOBAL_CONFIG_FILE", &override_file));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert_eq!(fs::read_to_string(&override_file).unwrap(), wrong_config);
    assert!(!override_dir.join("config.local.toml").exists());
    assert_eq!(alternate.tip(&wrong, REF), other_tip);
}

#[test]
fn new_restore_refuses_a_more_specific_remote_variant() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    let config = seed.world.root().join("config.toml");
    let contents = fs::read_to_string(&config).unwrap().replace(
        "{ profile = \"host-fixture\" }",
        "{ profile = \"host-fixture\" }, { os = \"unix\", profile = \"host-fixture\" }",
    );
    fs::write(config, contents).unwrap();
    seed.seed(&remote, FIRST);
    let branch = seed.tip(&remote, REF);
    assert!(
        text(ok(seed.git(Some(&remote)).args([
            "ls-tree",
            "-r",
            "--name-only",
            &branch
        ])))
        .contains("home@unix+host-fixture/")
    );

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o644);
    let result = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(result.status.code(), Some(2), "{}", messages(&result));
    assert!(messages(&result).contains("unsafe path, stream, object or recorded mode"));
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert!(!fixture.store().exists());
}

#[test]
fn new_restore_rejects_hard_links_and_protects_ordinary_file_modes() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    ok(fixture.git(None).args(["init"]));
    let source = fixture.world.root().join("source-script");
    fs::write(&source, b"synthetic executable\n").unwrap();
    for alias in [&source, &fixture.inactive()] {
        let bytes = fs::read(alias).unwrap();
        fs::set_permissions(alias, fs::Permissions::from_mode(0o755)).unwrap();
        fs::hard_link(alias, fixture.active()).unwrap();
        let rejected = fixture.dots(&["restore"], &remote).output().unwrap();
        assert_eq!(rejected.status.code(), Some(2), "{}", messages(&rejected));
        assert!(messages(&rejected).contains("hard-linked tracked file"));
        assert!(!fixture.store().exists());
        for path in [alias.as_path(), &fixture.active()] {
            assert_eq!(fs::read(path).unwrap(), bytes);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        fs::remove_file(fixture.active()).unwrap();
    }
    fixture.write(DEFAULT, 0o755);
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert_eq!(
        fs::metadata(fixture.active()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(&source).unwrap(), b"synthetic executable\n");
    assert_eq!(fs::read(fixture.inactive()).unwrap(), INACTIVE);
    for alias in [source, fixture.inactive()] {
        assert_eq!(
            fs::metadata(alias).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    assert_eq!(seed.tip(&remote, REF), tip);
}

#[test]
fn new_restore_recovers_a_killed_staging_fetch_and_keeps_native_health_in_place() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let previous_remote = seed.world.bare_remote("previous.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    let previous_url = format!("file://{}", previous_remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);

    let fixture = Fixture::new();
    fixture.configure(
        &previous_url,
        &previous_url,
        &key,
        std::slice::from_ref(&public),
    );
    fixture.seed(&previous_remote, DEFAULT);
    ok(fixture
        .git(Some(&fixture.store()))
        .args(["config", "remote.origin.url", &previous_url]));
    ok(&mut fixture.mise(&[
        "--yes",
        "bootstrap",
        "dotfiles",
        "origin",
        "set",
        &previous_url,
        "--branch",
        BRANCH,
        "--sync",
        "fetch-only",
    ]));
    let store = fixture.store();
    let old_tip = fixture.tip(&store, "refs/heads/main");
    let history = store.parent().unwrap();
    let health = history.join("sync.json");
    let old_health = fs::read(&health).unwrap();
    let health_inode = fs::metadata(&health).unwrap().ino();
    let old_status: serde_json::Value = serde_json::from_slice(&old_health).unwrap();
    assert_eq!(old_status["origin_url"], previous_url);
    assert_ne!(old_status["upstream_commit"], tip);

    fs::set_permissions(history, fs::Permissions::from_mode(0o755)).unwrap();
    let unknown_stage = history.join(".dots-restore-clone-unrelated");
    fs::create_dir(&unknown_stage).unwrap();
    fs::write(unknown_stage.join("keep"), b"unrelated fixture").unwrap();
    fixture.configure(&url, &url, &key, &[public]);
    let hit = fixture.world.home().join("staging-fetch-hit");
    let modes = fixture.world.home().join("staging-fetch-modes");
    fixture.script(
        "git",
        r#"#!/bin/sh
if [ "${FIXTURE_KILL_STAGING_FETCH-}" = 1 ] && [ "${3-}" = fetch ]; then
    ls -ld "${2%/*}" "$2" > "$FIXTURE_STAGE_MODES" || exit "$?"
    "$FIXTURE_GIT" "$@" || exit "$?"
    printf '%s\n' "$2" > "$FIXTURE_STAGE_HIT"
    kill -KILL "$PPID"
    exit 94
fi
exec "$FIXTURE_GIT" "$@"
"#,
    );
    let mut command = fixture.dots(&["restore", "--initialize-history"], &remote);
    // Use a permissive umask only in this forked fixture process.
    unsafe {
        command.pre_exec(|| {
            nix::libc::umask(0o022);
            Ok(())
        });
    }
    let killed = command
        .env("FIXTURE_KILL_STAGING_FETCH", "1")
        .env("FIXTURE_STAGE_HIT", &hit)
        .env("FIXTURE_STAGE_MODES", &modes)
        .output()
        .unwrap();
    assert_eq!(killed.status.signal(), Some(9), "{}", messages(&killed));
    let modes = fs::read_to_string(modes).expect("staging modes were not observed before fetch");
    let modes: Vec<_> = modes.lines().collect();
    assert_eq!(modes.len(), 2);
    for line in modes {
        assert!(line.starts_with("drwx------"), "unexpected mode: {line}");
    }
    let staging = history.join(".dots-restore-install");
    let clone = PathBuf::from(
        fs::read_to_string(hit)
            .expect("the staged fetch did not run")
            .trim_end(),
    );
    assert_eq!(clone.parent(), Some(staging.as_path()));
    assert_eq!(fixture.tip(&clone, "refs/heads/main"), tip);
    assert_eq!(fixture.tip(&store, "refs/heads/main"), old_tip);
    assert_eq!(fs::metadata(&health).unwrap().ino(), health_inode);
    assert!(fs::read(&health).unwrap() == old_health);

    let marker = staging.join("dots-restore-pending");
    let owner = fs::read(&marker).unwrap();
    let retirement = history.join(".dots-restore-retiring");
    let foreign = staging.join("unrelated");
    // Stop rmdir after the sibling marker becomes the only name, with no installed journal.
    fs::write(&foreign, b"unrelated fixture").unwrap();
    let interrupted = fixture
        .dots(&["restore", "--initialize-history"], &remote)
        .output()
        .unwrap();
    assert_eq!(
        interrupted.status.code(),
        Some(1),
        "{}",
        messages(&interrupted)
    );
    assert!(
        messages(&interrupted).contains("could not finish the history install staging directory")
    );
    assert!(!clone.exists());
    assert!(!marker.exists());
    assert_eq!(fs::read(&retirement).unwrap(), owner);
    assert_eq!(fs::read(&foreign).unwrap(), b"unrelated fixture");
    assert!(!store.join("dots-restore-pending").exists());
    assert_eq!(fixture.tip(&store, "refs/heads/main"), old_tip);
    assert_eq!(fs::metadata(&health).unwrap().ino(), health_inode);
    assert!(fs::read(&health).unwrap() == old_health);
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);

    // An independent copy with the same contents cannot prove ownership.
    let receipt_inode = fs::metadata(&retirement).unwrap().ino();
    fs::write(&marker, &owner).unwrap();
    let foreign_inode = fs::metadata(&marker).unwrap().ino();
    assert_ne!(foreign_inode, receipt_inode);
    let mismatch = fixture
        .dots(&["restore", "--initialize-history"], &remote)
        .output()
        .unwrap();
    assert_eq!(mismatch.status.code(), Some(2), "{}", messages(&mismatch));
    assert!(messages(&mismatch).contains("the history clone ownership markers do not match"));
    assert_eq!(fs::metadata(&marker).unwrap().ino(), foreign_inode);
    assert_eq!(fs::metadata(&retirement).unwrap().ino(), receipt_inode);
    assert_eq!(fs::read(&marker).unwrap(), owner);
    assert_eq!(fs::read(&retirement).unwrap(), owner);

    // An interrupted hard link leaves two names for the original inode.
    fs::remove_file(&marker).unwrap();
    fs::hard_link(&retirement, &marker).unwrap();
    assert_eq!(fs::metadata(&marker).unwrap().ino(), receipt_inode);
    let linked = fixture
        .dots(&["restore", "--initialize-history"], &remote)
        .output()
        .unwrap();
    assert_eq!(linked.status.code(), Some(1), "{}", messages(&linked));
    assert!(messages(&linked).contains("could not finish the history install staging directory"));
    assert!(!marker.exists());
    assert_eq!(fs::metadata(&retirement).unwrap().ino(), receipt_inode);
    assert_eq!(fs::read(&foreign).unwrap(), b"unrelated fixture");
    fs::remove_file(foreign).unwrap();

    let report_only = fixture.script(
        "skip-native-fetch",
        r#"#!/bin/sh
case " $* " in
    *" bootstrap dotfiles sync --fetch-only "*) exit 0 ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
    );
    let skipped = fixture
        .dots(&["restore", "--initialize-history"], &remote)
        .env("DOTS_MISE_BIN", report_only)
        .env("FIXTURE_MISE", &fixture.mise)
        .output()
        .unwrap();
    assert_eq!(skipped.status.code(), Some(1), "{}", messages(&skipped));
    assert!(
        messages(&skipped).contains("could not resolve the fetched private history checkpoint")
    );
    assert!(!retirement.exists());
    assert!(store.join("dots-restore-pending").is_file());
    assert_eq!(fixture.tip(&store, "refs/heads/main"), tip);
    assert_eq!(fs::metadata(&health).unwrap().ino(), health_inode);
    assert!(fs::read(&health).unwrap() == old_health);
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);

    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    let native_status: serde_json::Value =
        serde_json::from_slice(&fs::read(&health).unwrap()).unwrap();
    assert_eq!(native_status["upstream_commit"], tip);
    assert_eq!(native_status["origin_url"], previous_url);
    let backup = fs::read_dir(history)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("repo.git.pre-restore.")
        })
        .unwrap();
    assert_eq!(
        fixture.tip(&backup.join("repo.git"), "refs/heads/main"),
        old_tip
    );
    assert!(!backup.join("sync.json").exists());
    assert!(!staging.exists());
    assert!(!store.join("dots-restore-pending").exists());
    assert_eq!(
        fs::read(unknown_stage.join("keep")).unwrap(),
        b"unrelated fixture"
    );
    assert_eq!(seed.tip(&remote, REF), tip);
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);

    let local_file = fixture.world.root().join("config.local.toml");
    let local = fs::read_to_string(&local_file).unwrap();
    let head = fixture.tip(&store, "refs/heads/main");
    let native_health = fs::read(&health).unwrap();
    for (git_origin, native_branch) in [(&previous_url, BRANCH), (&url, "host-other")] {
        ok(fixture
            .git(Some(&store))
            .args(["config", "remote.origin.url", git_origin]));
        let mut config: toml_edit::DocumentMut = local.parse().unwrap();
        config["history"]["origin"]["branch"] = toml_edit::value(native_branch);
        fs::write(&local_file, config.to_string()).unwrap();
        let mismatch = fixture.dots(&["restore"], &remote).output().unwrap();
        assert_eq!(mismatch.status.code(), Some(2), "{}", messages(&mismatch));
        assert!(messages(&mismatch).contains("local history is connected elsewhere"));
        assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
        assert_eq!(fixture.tip(&store, "refs/heads/main"), head);
        assert!(fs::read(&health).unwrap() == native_health);
    }
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
    let previous = format!(
        "file://{}",
        fixture.world.bare_remote("previous.git").display()
    );
    let wrong = format!(
        "file://{}",
        fixture.world.root().join("no-bare-remote").display()
    );
    fixture.configure(&wrong, &wrong, &key, &recipients);
    fixture.write(DEFAULT, 0o600);
    ok(&mut fixture.mise(&["bootstrap", "dotfiles", "save"]));
    let store = fixture.store();
    let history = store.parent().unwrap();
    // An old store connected to the requested URL would use established restore.
    ok(fixture
        .git(Some(&store))
        .args(["config", "remote.origin.url", &previous]));
    let original = fixture.tip(&store, "HEAD");
    let failed = fixture
        .dots(&["restore", "--initialize-history"], &remote)
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1), "{}", messages(&failed));
    assert!(
        messages(&failed).contains("could not stage the selected remote history branch"),
        "{}",
        messages(&failed)
    );
    assert_eq!(fixture.tip(&store, "HEAD"), original);
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    assert!(!store.join("dots-restore-pending").exists());
    assert!(!history.join(".dots-restore-install").exists());
    fixture.configure(&origin, &origin, &key, &recipients);
    let no_flag = fixture.dots(&["restore"], &remote).output().unwrap();
    assert_eq!(no_flag.status.code(), Some(2), "{}", messages(&no_flag));
    assert!(messages(&no_flag).contains("local history is connected elsewhere"));
    ok(&mut fixture.dots(&["restore", "--initialize-history"], &remote));
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

#[test]
fn new_initialize_retries_the_replacement_after_staging_fails_without_git_origin() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    seed.seed(&remote, FIRST);
    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    fixture.write(DEFAULT, 0o600);
    ok(&mut fixture.mise(&["bootstrap", "dotfiles", "save"]));
    let store = fixture.store();
    let old_head = fixture.tip(&store, "refs/heads/main");
    assert!(
        fixture
            .git(Some(&store))
            .args(["config", "--get", "remote.origin.url"])
            .output()
            .unwrap()
            .status
            .code()
            == Some(1)
    );
    assert!(!store.parent().unwrap().join("sync.json").exists());
    let local = fixture.world.root().join("config.local.toml");
    let old_config = fs::read_to_string(&local)
        .unwrap()
        .replace("branch = \"host-fixture\"", "branch = \"host-old\"");
    fs::write(&local, &old_config).unwrap();
    fixture.script(
        "git",
        r#"#!/bin/sh
if [ "${FIXTURE_FAIL_RESTORE_FETCH-}" = 1 ] && [ "${3-}" = fetch ]; then exit 92; fi
exec "$FIXTURE_GIT" "$@"
"#,
    );
    let failed = fixture
        .dots(&["restore", "--initialize-history"], &remote)
        .env("FIXTURE_FAIL_RESTORE_FETCH", "1")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1), "{}", messages(&failed));
    assert!(messages(&failed).contains("existing store was left unchanged"));
    assert_eq!(fixture.tip(&store, "refs/heads/main"), old_head);
    assert_eq!(fs::read_to_string(local).unwrap(), old_config);
    assert_eq!(fs::read(fixture.active()).unwrap(), DEFAULT);
    ok(&mut fixture.dots(&["restore", "--initialize-history"], &remote));
    let previous = fs::read_dir(store.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("repo.git.pre-restore.")
        })
        .unwrap();
    assert_eq!(
        fixture.tip(&previous.join("repo.git"), "refs/heads/main"),
        old_head
    );
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
}

#[test]
fn new_restore_creates_only_the_missing_private_parent() {
    let seed = Fixture::new();
    let remote = seed.world.bare_remote("private.git");
    let (key, public) = seed.key("recovery");
    let url = format!("file://{}", remote.display());
    seed.configure(&url, &url, &key, std::slice::from_ref(&public));
    fs::set_permissions(
        seed.active().parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    seed.seed(&remote, FIRST);
    let tip = seed.tip(&remote, REF);
    let fixture = Fixture::new();
    fixture.configure(&url, &url, &key, &[public]);
    let app = fixture.active().parent().unwrap().to_owned();
    fs::remove_file(fixture.inactive()).unwrap();
    fs::remove_dir(&app).unwrap();
    ok(&mut fixture.dots(&["restore"], &remote));
    assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
    assert_eq!(
        fs::metadata(app).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(fixture.active()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!fixture.store().join("dots-restore-pending").exists());
    assert_eq!(seed.tip(&remote, REF), tip);
}

#[test]
fn new_restore_keeps_an_unrelated_native_pending_record_with_the_old_store() {
    let fixture = Fixture::new();
    let remote = fixture.world.bare_remote("private.git");
    let (key, public) = fixture.key("recovery");
    let url = format!("file://{}", remote.display());
    fixture.configure(&url, &url, &key, &[public]);
    fixture.seed(&remote, FIRST);
    let store = fixture.store();
    let head = fixture.tip(&store, "refs/heads/main");
    fixture.write(LOCAL_EDIT, 0o644);
    let native = store.parent().unwrap().join("index/pending");
    fs::create_dir_all(&native).unwrap();
    let uuid = "00000000-0000-4000-8000-000000000000";
    let record = native.join(format!("{uuid}.json"));
    let bytes = serde_json::to_vec(&serde_json::json!({
        "id": 42, "recovery": "pending",
        "checkpoint": {"uuid": uuid, "operation": {"id": uuid}}
    }))
    .unwrap();
    fs::write(&record, &bytes).unwrap();
    for args in [&["backup"][..], &["restore", "--initialize-history"]] {
        let output = fixture.dots(args, &remote).output().unwrap();
        assert!(!output.status.success(), "{}", messages(&output));
        assert!(
            messages(&output).contains("unresolved settings history operation"),
            "{}",
            messages(&output)
        );
        assert_eq!(fs::read(&record).unwrap(), bytes);
        assert_eq!(fs::read(fixture.active()).unwrap(), LOCAL_EDIT);
        assert_eq!(
            fs::metadata(fixture.active()).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fixture.tip(&store, "refs/heads/main"), head);
    }
}
