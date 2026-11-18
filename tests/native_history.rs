mod support;

use std::env;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
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
        let interrupted = fixture.script(
            "interrupt-native-rollback",
            r#"#!/bin/sh
case " $* " in
  *" bootstrap dotfiles rollback "*" --yes "*) export FIXTURE_STOP_NATIVE=1 ;;
esac
exec "$FIXTURE_MISE" "$@"
"#,
        );
        fixture.script(
            "git",
            r#"#!/bin/sh
if [ "${FIXTURE_STOP_NATIVE-}" = 1 ]; then
    for record in "$MISE_STATE_DIR"/history/index/pending/*.json; do
        if [ -f "$record" ] && /usr/bin/grep -q '"kind": *"committed"' "$record"; then
            kill -KILL "$PPID"
            exit 94
        fi
    done
fi
exec "$FIXTURE_GIT" "$@"
"#,
        );
        let output = fixture
            .dots(&["restore"], &remote)
            .env("DOTS_MISE_BIN", interrupted)
            .env("FIXTURE_MISE", &fixture.mise)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{}", messages(&output));
        assert_eq!(fs::read(fixture.active()).unwrap(), FIRST);
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
        assert_eq!(native["checkpoint"]["operation"]["status"], "completed");
        assert_eq!(native["recovery"], "finished");
        assert!(
            native["checkpoint"]["operation"]["journal"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "committed")
        );
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
    ok(fixture
        .git(Some(&fixture.store()))
        .args(["config", "remote.origin.url", &wrong]));
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
