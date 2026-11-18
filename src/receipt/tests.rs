use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::{ChangeGroup, Issue, Receipt, metadata, patch, store};
use crate::Env;

struct Fixture {
    dir: TempDir,
    env: Env,
    root: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        let bin = dir.path().join("bin");
        let home = dir.path().join("home");
        for path in [&root, &bin, &home] {
            fs::create_dir(path).unwrap();
        }
        let path = std::env::join_paths(
            std::iter::once(bin.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut env = Env {
            vars: Default::default(),
            cwd: root.clone(),
        };
        for (key, value) in [
            ("HOME", home.into_os_string()),
            ("PATH", path),
            (
                "XDG_CONFIG_HOME",
                dir.path().join("config").into_os_string(),
            ),
            ("XDG_STATE_HOME", dir.path().join("state").into_os_string()),
            ("XDG_CACHE_HOME", dir.path().join("cache").into_os_string()),
            ("XDG_DATA_HOME", dir.path().join("data").into_os_string()),
            ("JJ_CONFIG", dir.path().join("jj.toml").into_os_string()),
            (
                "MISE_CONFIG_DIR",
                dir.path().join("config/mise").into_os_string(),
            ),
        ] {
            env.vars.insert(key.into(), value);
        }
        let fixture = Self {
            dir,
            env,
            root: root.canonicalize().unwrap(),
            bin,
        };
        fixture.git(&["init", "-q", "-b", "dev"]);
        fixture.git(&["config", "user.name", "Receipt Fixture"]);
        fixture.git(&["config", "user.email", "receipt@example.invalid"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        let result = Command::new("git")
            .env_clear()
            .envs(&self.env.vars)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .current_dir(&self.root)
            .args(args)
            .output()
            .unwrap();
        assert!(result.status.success(), "git {args:?}: {:?}", result.stderr);
        String::from_utf8(result.stdout).unwrap().trim().into()
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn scanner(&self, script: &str) {
        let file = self.bin.join("gitleaks");
        fs::write(&file, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn receipt(&self, before: String, after: String, paths: &[&str]) -> Receipt {
        let mut receipt = Receipt::new(Some(self.root.clone()), "not pushed");
        receipt.updated.push(ChangeGroup::new(
            Some(before),
            Some(after),
            paths.iter().map(PathBuf::from).collect(),
        ));
        receipt
    }

    fn render(&self, receipt: &Receipt, replay: bool) -> String {
        patch::render(&self.env, &self.root, receipt, replay, patch::MAX_BYTES)
    }

    fn stored_file(&self) -> PathBuf {
        fs::read_dir(self.env.state_dir().unwrap().join("receipts"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
}

#[test]
fn storage_is_private_atomic_current_and_checkout_scoped() {
    let fixture = Fixture::new();
    let receipt = Receipt::new(Some(fixture.root.clone()), "nothing to push");
    store::save(&fixture.env, &fixture.root, &receipt).unwrap();
    let path = fixture.stored_file();
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        path.parent()
            .unwrap()
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(
        store::load(&fixture.env, &fixture.root.join("other"))
            .unwrap()
            .is_none()
    );

    let mut stopped = Receipt::new(Some(fixture.root.clone()), "not pushed");
    stopped
        .issues
        .push(Issue::new("NOT SHARED: fixture", "Retry the fixture"));
    let non_utf8 = PathBuf::from(OsString::from_vec(b"bad-\xff".to_vec()));
    stopped
        .saved
        .push(ChangeGroup::new(None, None, vec![non_utf8.clone()]));
    store::save(&fixture.env, &fixture.root, &stopped).unwrap();
    let loaded = store::load(&fixture.env, &fixture.root).unwrap().unwrap();
    assert_eq!(loaded.outcome, "not pushed");
    assert_eq!(loaded.saved[0].paths, vec![non_utf8]);
    assert_eq!(loaded.issues[0].next_action, "Retry the fixture");
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    assert!(!fs::read_to_string(&path).unwrap().contains("patch"));

    let mut previous: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    previous["version"] = 0.into();
    fs::write(&path, serde_json::to_vec(&previous).unwrap()).unwrap();
    assert!(store::load(&fixture.env, &fixture.root).unwrap().is_none());

    let outside = fixture.dir.path().join("outside");
    fs::write(&outside, "do not touch").unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(store::load(&fixture.env, &fixture.root).is_err());
    store::save(&fixture.env, &fixture.root, &receipt).unwrap();
    assert_eq!(fs::read_to_string(outside).unwrap(), "do not touch");
    assert!(store::load(&fixture.env, &fixture.root).unwrap().is_some());
}

#[test]
fn metadata_leads_with_recovery_and_preserves_paths_in_replay() {
    let fixture = Fixture::new();
    let mut receipt = Receipt::new(Some(fixture.root.clone()), "not pushed");
    receipt.issues.push(Issue::new(
        "NOT SHARED: stopped",
        "Retry after fixing the issue",
    ));
    receipt.kept = 2;
    receipt.updated.push(ChangeGroup::new(
        None,
        None,
        (0..6).map(|i| PathBuf::from(format!("file-{i}"))).collect(),
    ));
    receipt
        .recovery_files
        .push(PathBuf::from("/private/copy\x1b[1m"));
    let live = metadata(&receipt, false);
    assert!(live.starts_with("NOT SHARED: stopped\n  Retry after fixing the issue\n"));
    assert!(live.contains("kept 2 files on disk"));
    assert!(live.contains("… and 1 more"));
    assert!(!live.contains("file-5"));
    assert!(!live.contains('\x1b'));
    assert!(metadata(&receipt, true).contains("file-5"));
    let mut local = Receipt::new(None, "add an origin remote to share changes");
    local.local_only = true;
    assert_eq!(
        metadata(&local, false),
        "dots sync: add an origin remote to share changes\n"
    );
}

#[test]
fn patch_uses_verified_objects_and_screens_both_versions_and_exact_display() {
    let fixture = Fixture::new();
    let remote = fixture.dir.path().join("remote.git");
    fixture.git(&["init", "--bare", "-q", remote.to_str().unwrap()]);
    fixture.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    let log = fixture.dir.path().join("scanner-input");
    let quoted = log.to_string_lossy().replace('\'', "'\"'\"'");
    fixture.scanner(&format!(
        r#"
        for file do :; done
        /bin/cat "$file" >> '{quoted}'
        printf '\n--end--\n' >> '{quoted}'
        printf '[]\n'
    "#
    ));
    fs::write(fixture.root.join("file"), "before body\n").unwrap();
    let before = fixture.commit("older");
    fs::write(fixture.root.join("file"), "after body\n").unwrap();
    let after = fixture.commit("newer");
    fixture.git(&["push", "-q", "origin", "dev"]);
    fs::write(fixture.root.join("file"), "uncommitted content\n").unwrap();
    let receipt = fixture.receipt(before, after, &["file"]);
    let output = fixture.render(&receipt, false);
    assert_eq!(output, "\n  file\n    -before body\n    +after body\n");
    let inputs = fs::read_to_string(log).unwrap();
    assert_eq!(
        inputs,
        format!("before body\n\n--end--\nafter body\n\n--end--\n{output}\n--end--\n")
    );
}

#[test]
fn invalid_or_unavailable_patches_leave_metadata() {
    let fixture = Fixture::new();
    fixture.scanner("printf '[]\n'");
    fs::write(fixture.root.join("binary"), [0]).unwrap();
    fs::write(fixture.root.join("sealed.age"), "old sealed\n").unwrap();
    fs::write(fixture.root.join("regular"), "old\n").unwrap();
    let before = fixture.commit("older");
    fs::write(fixture.root.join("binary"), [1]).unwrap();
    fs::write(fixture.root.join("sealed.age"), "new sealed\n").unwrap();
    fs::write(fixture.root.join("regular"), "new\n").unwrap();
    let after = fixture.commit("newer");
    let mut receipt = fixture.receipt(
        before,
        after,
        &["binary", "sealed.age", "regular", "../regular"],
    );
    assert!(fixture.render(&receipt, false).is_empty());
    assert!(
        fixture
            .render(&receipt, true)
            .contains("Patch omitted for 3 files.")
    );
    fixture.scanner("printf '{}\n'");
    assert_eq!(
        fixture.render(&receipt, true),
        "\nPatch omitted for 4 files.\n"
    );
    fixture.scanner("printf '[]\n'\nexit 2");
    assert_eq!(
        fixture.render(&receipt, true),
        "\nPatch omitted for 4 files.\n"
    );
    receipt.updated[0].after = Some("0".repeat(40));
    fixture.scanner("printf '[]\n'");
    assert_eq!(
        fixture.render(&receipt, true),
        "\nPatch omitted for 4 files.\n"
    );
}

#[test]
fn replay_times_out_a_scanner_and_rejects_nonlocal_configuration() {
    let fixture = Fixture::new();
    fixture.scanner("sleep 60\nprintf '[]\n'");
    fs::write(fixture.root.join("file"), "old\n").unwrap();
    let before = fixture.commit("older");
    fs::write(fixture.root.join("file"), "new\n").unwrap();
    let after = fixture.commit("newer");
    let receipt = fixture.receipt(before, after, &["file"]);
    let start = Instant::now();
    assert_eq!(
        fixture.render(&receipt, true),
        "\nPatch omitted for 1 file.\n"
    );
    assert!(start.elapsed() < Duration::from_secs(5));

    fixture.scanner("printf '[]\n'");
    let config = fixture.root.join(".gitleaks.toml");
    for path in [
        "https://example.invalid/remote.toml",
        "../outside.toml",
        "/outside.toml",
    ] {
        fs::write(&config, format!("[extend]\npath = {path:?}\n")).unwrap();
        assert_eq!(
            fixture.render(&receipt, true),
            "\nPatch omitted for 1 file.\n"
        );
    }
    fs::write(fixture.root.join("inner.toml"), "title = 'checks'\n").unwrap();
    fs::write(config, "[extend]\npath = 'inner.toml'\n").unwrap();
    assert!(fixture.render(&receipt, true).contains("+new"));
}

#[test]
fn real_gitleaks_honors_the_floor_and_path_scoped_project_detectors() {
    if !Command::new("gitleaks")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return;
    }
    let fixture = Fixture::new();
    fs::write(fixture.root.join("plain.txt"), "old\n").unwrap();
    let before = fixture.commit("older");
    fs::write(fixture.root.join("plain.txt"), "new\n").unwrap();
    let after = fixture.commit("clean");
    let receipt = fixture.receipt(before.clone(), after, &["plain.txt"]);
    assert!(fixture.render(&receipt, true).contains("+new"));

    fs::write(fixture.root.join("plain.txt"), "CUSTOM_MARKER_1390\n").unwrap();
    let after = fixture.commit("custom");
    fs::write(
        fixture.root.join("detectors.toml"),
        r#"
[[rules]]
id = "fixture-path"
path = '''plain\.txt$'''
regex = '''CUSTOM_MARKER_[0-9]+'''
"#,
    )
    .unwrap();
    fs::write(
        fixture.root.join(".gitleaks.toml"),
        "[extend]\npath = 'detectors.toml'\n",
    )
    .unwrap();
    let custom = fixture.receipt(before, after.clone(), &["plain.txt"]);
    assert_eq!(
        fixture.render(&custom, true),
        "\nPatch omitted for 1 file.\n"
    );

    let fake_github_token = format!("ghp_{}", "AbC12".repeat(7) + "D");
    fs::write(
        fixture.root.join("plain.txt"),
        format!("token = '{fake_github_token}' # gitleaks:allow\n"),
    )
    .unwrap();
    let end = fixture.commit("floor");
    fs::write(
        fixture.root.join(".gitleaks.toml"),
        r#"
[extend]
useDefault = true
disabledRules = ["github-pat"]
[allowlist]
regexes = ['''.*''']
"#,
    )
    .unwrap();
    let floor = fixture.receipt(after, end, &["plain.txt"]);
    assert_eq!(
        fixture.render(&floor, true),
        "\nPatch omitted for 1 file.\n"
    );
}
