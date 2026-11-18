use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::{ChangeGroup, Issue, Receipt, metadata, page, pager, pager_metadata, patch, store};
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
        let env = isolated_env(dir.path());
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
        std::mem::take(
            &mut patch::render_until(
                &self.env,
                &self.root,
                receipt,
                replay,
                patch::MAX_BYTES,
                Instant::now() + Duration::from_secs(15),
            )
            .text,
        )
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

pub(super) fn isolated_env(dir: &Path) -> Env {
    let path = std::env::join_paths(
        std::iter::once(dir.join("bin"))
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut env = Env {
        vars: Default::default(),
        cwd: dir.to_owned(),
    };
    env.vars.insert("PATH".into(), path);
    for (key, relative) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
        ("CODEX_HOME", "config/codex"),
        ("CLAUDE_CONFIG_DIR", "config/claude"),
        ("JJ_CONFIG", "config/jj/config.toml"),
        ("GIT_CONFIG_GLOBAL", "config/git/config"),
        ("GIT_CONFIG_SYSTEM", "config/git/system"),
        ("DOTS_ROOT", "checkout"),
        ("MISE_CONFIG_DIR", "config/mise"),
        ("MISE_GLOBAL_CONFIG_FILE", "config/mise/config.toml"),
        ("MISE_SYSTEM_CONFIG_DIR", "config/mise-system"),
        ("MISE_DATA_DIR", "data/mise"),
        ("MISE_STATE_DIR", "state/mise"),
        ("MISE_CACHE_DIR", "cache/mise"),
        ("CARGO_HOME", "cache/cargo"),
        ("RUSTUP_HOME", "data/rustup"),
        ("TMPDIR", ""),
    ] {
        env.vars
            .insert(key.into(), dir.join(relative).into_os_string());
    }
    env
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
fn recovery_and_push_outcomes_are_visible_in_saved_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("checkout");
    let env = Env {
        vars: [(
            "XDG_STATE_HOME".into(),
            directory.path().join("state").into_os_string(),
        )]
        .into(),
        cwd: directory.path().to_owned(),
    };
    let recovery = directory.path().join("private/recovery\x1b\n\\u{1b}");
    let mut pushed = Receipt::new(Some(root.clone()), "pushed dev");
    pushed.recovery_files.push(recovery.clone());
    let recovery_line = format!(
        "Recovery copy at {}/private/recovery\\u{{1b}}\\n\\\\u{{1b}}\n",
        directory.path().display()
    );
    assert!(metadata(&pushed, false).ends_with(&recovery_line));
    store::save(&env, &root, &pushed).unwrap();
    let loaded = store::load(&env, &root).unwrap().unwrap();
    assert!(loaded.issues.is_empty());
    assert_eq!(loaded.recovery_files, [recovery]);
    let replay = metadata(&loaded, true);
    assert!(replay.contains(" · pushed dev\n"));
    assert!(replay.ends_with(&recovery_line));

    let mut saved = Receipt::new(Some(root.clone()), "not pushed");
    saved.saved.push(ChangeGroup::new(
        Some("1".repeat(40)),
        Some("2".repeat(40)),
        vec!["changed-file".into()],
    ));
    let summary = metadata(&saved, false);
    assert!(summary.contains("saved 1 locally"));
    assert!(summary.contains(" · not pushed\n"));
    let untouched = Receipt::new(Some(root), "nothing to push");
    let summary = metadata(&untouched, false);
    assert!(summary.contains("saved 0 locally"));
    assert!(summary.ends_with(" · nothing to push\n"));
}

#[test]
fn bounded_replay_prioritizes_escaped_recovery_locations() {
    let paths = |group| {
        (0..1500)
            .map(|i| PathBuf::from(format!("{group}/{i:04}/{}", "é".repeat(240))))
            .collect()
    };
    let mut receipt = Receipt::new(None, "not pushed");
    receipt
        .saved
        .push(ChangeGroup::new(None, None, paths("saved")));
    receipt
        .updated
        .push(ChangeGroup::new(None, None, paths("updated")));
    receipt
        .recovery_files
        .push("/private/copy\n\x1b[31m\\u{1b}".into());
    let metadata = metadata(&receipt, true);
    assert!(metadata.len() > patch::MAX_BYTES);
    let (output, truncated) = pager_metadata(&metadata);
    assert!(truncated);
    assert!(output.len() <= patch::MAX_BYTES);
    let recovery_line = "Recovery copy at /private/copy\\n\\u{1b}[31m\\\\u{1b}\n";
    assert!(output.find(recovery_line).unwrap() < output.find("Saved locally\n").unwrap());
    assert!(output.contains("Updated in this checkout\n"));
    assert!(!output.contains("updated/1499"));
    assert!(!output.contains('\x1b'));
    assert!(output.ends_with("Additional receipt metadata omitted.\n"));
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
        if [ "$1" = stdin ]; then
            /bin/cat > '{quoted}.display'
        else
            for file do :; done
            case "$(/bin/cat "$file")" in
                'before body') /bin/cat "$file" > '{quoted}.before' ;;
                'after body') /bin/cat "$file" > '{quoted}.after' ;;
                *) exit 2 ;;
            esac
        fi
        printf '[]\n'
    "#
    ));
    fs::write(fixture.root.join("file"), "before body\n").unwrap();
    let before = fixture.commit("older");
    fs::write(fixture.root.join("file"), "after body\n").unwrap();
    let after = fixture.commit("newer");
    fixture.git(&["push", "-q", "origin", "dev"]);
    fs::write(fixture.root.join("file"), "uncommitted content\n").unwrap();
    fs::write(fixture.root.join(".gitattributes"), "* -diff\n").unwrap();
    fixture.git(&["config", "diff.external", "/does-not-exist"]);
    let receipt = fixture.receipt(before, after, &["file"]);
    let output = fixture.render(&receipt, false);
    assert_eq!(output, "\n  file\n    -before body\n    +after body\n");
    assert_eq!(
        fs::read_to_string(log.with_extension("before")).unwrap(),
        "before body\n"
    );
    assert_eq!(
        fs::read_to_string(log.with_extension("after")).unwrap(),
        "after body\n"
    );
    assert_eq!(
        fs::read_to_string(log.with_extension("display")).unwrap(),
        output
    );
    fixture.scanner("if [ \"$1\" = stdin ]; then printf '[{}]\\n'; else printf '[]\\n'; fi");
    assert!(fixture.render(&receipt, false).is_empty());
}

#[test]
fn invalid_or_unavailable_patches_leave_metadata() {
    let fixture = Fixture::new();
    fixture.scanner("printf '[]\n'");
    fs::write(fixture.root.join("binary"), [0]).unwrap();
    fs::write(fixture.root.join("sealed.age"), "old sealed\n").unwrap();
    fs::write(fixture.root.join("regular"), "old\n").unwrap();
    fs::write(
        fixture.root.join("config.local.toml"),
        "old private bytes\n",
    )
    .unwrap();
    let before = fixture.commit("older");
    fs::write(fixture.root.join("binary"), [1]).unwrap();
    fs::write(fixture.root.join("sealed.age"), "new sealed\n").unwrap();
    fs::write(fixture.root.join("regular"), "new\n").unwrap();
    fs::write(
        fixture.root.join("config.local.toml"),
        "new private bytes\n",
    )
    .unwrap();
    let after = fixture.commit("newer");
    let mut receipt = fixture.receipt(
        before,
        after,
        &[
            "binary",
            "sealed.age",
            "regular",
            "../regular",
            "config.local.toml",
        ],
    );
    assert!(fixture.render(&receipt, false).is_empty());
    assert!(
        fixture
            .render(&receipt, true)
            .contains("Patch omitted for 4 files.")
    );
    fixture.scanner("printf '{}\n'");
    assert_eq!(
        fixture.render(&receipt, true),
        "\nPatch omitted for 5 files.\n"
    );
    fixture.scanner("printf '[]\n'\nexit 2");
    assert_eq!(
        fixture.render(&receipt, true),
        "\nPatch omitted for 5 files.\n"
    );
    receipt.updated[0].after = Some("0".repeat(40));
    fixture.scanner("printf '[]\n'");
    assert_eq!(
        fixture.render(&receipt, true),
        "\nPatch omitted for 5 files.\n"
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
        patch::render(
            &fixture.env,
            &fixture.root,
            &receipt,
            true,
            patch::MAX_BYTES
        )
        .text,
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
    let fixture = Fixture::new();
    if !super::Tools::new(&fixture.env)
        .command("gitleaks")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return;
    }
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

    let fake_github_token = concat!("ghp_", "A9kV7mQ2xR6cD3nL8pW4sB5zT1fH0jY8eM2q");
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

#[test]
fn oversized_or_unreplaceable_receipt_does_not_publish_partial_metadata() {
    let fixture = Fixture::new();
    let prior = Receipt::new(None, "previous");
    store::save(&fixture.env, &fixture.root, &prior).unwrap();
    let destination = fixture.stored_file();

    let mut oversized = Receipt::new(None, "oversized");
    oversized.outcome = "x".repeat(store::MAX_RECEIPT_BYTES as usize + 1);
    assert!(store::save(&fixture.env, &fixture.root, &oversized).is_err());
    assert_eq!(
        store::load(&fixture.env, &fixture.root)
            .unwrap()
            .unwrap()
            .outcome,
        "previous"
    );
    assert_eq!(
        fs::read_dir(destination.parent().unwrap()).unwrap().count(),
        1
    );

    let previous_file = fixture.dir.path().join("saved-metadata");
    fs::rename(&destination, &previous_file).unwrap();
    fs::create_dir(&destination).unwrap();
    assert!(
        store::save(
            &fixture.env,
            &fixture.root,
            &Receipt::new(None, "failed replacement")
        )
        .is_err()
    );
    assert!(store::load(&fixture.env, &fixture.root).is_err());
    assert_eq!(
        fs::read_dir(destination.parent().unwrap()).unwrap().count(),
        1
    );
    fs::remove_dir(&destination).unwrap();
    fs::rename(previous_file, destination).unwrap();
    assert_eq!(
        store::load(&fixture.env, &fixture.root)
            .unwrap()
            .unwrap()
            .outcome,
        "previous"
    );
}

#[test]
fn replay_byte_budget_keeps_later_small_patches_across_groups() {
    let fixture = Fixture::new();
    fixture.scanner("printf '[]\n'");
    fs::write(fixture.root.join("base"), "base\n").unwrap();
    let before = fixture.commit("before");
    fs::write(fixture.root.join("small-one"), "first small body\n").unwrap();
    fs::write(fixture.root.join("too-long"), "x".repeat(1_200)).unwrap();
    let middle = fixture.commit("middle");
    fs::write(fixture.root.join("small-two"), "second small body\n").unwrap();
    let after = fixture.commit("after");
    let mut receipt = fixture.receipt(before, middle.clone(), &["small-one", "too-long"]);
    receipt.saved.push(ChangeGroup::new(
        Some(middle),
        Some(after),
        vec!["small-two".into()],
    ));
    // Leave enough space for both small patches, even across groups.
    let budget = 180;
    let rendered = patch::render_until(
        &fixture.env,
        &fixture.root,
        &receipt,
        true,
        budget,
        Instant::now() + Duration::from_secs(15),
    );
    assert!(rendered.text.contains("+first small body"));
    assert!(rendered.text.contains("+second small body"));
    assert!(!rendered.text.contains(&"x".repeat(10)));
    assert!(rendered.text.contains("Patch omitted for 1 file."));
    assert!(rendered.text.len() <= budget);
}

#[test]
fn pager_receives_only_restricted_settings() {
    let mut fixture = Fixture::new();
    for name in ["LESS", "LESSOPEN", "LESSCLOSE", "LESSSECURE_ALLOW"] {
        fixture
            .env
            .vars
            .insert(name.into(), "fixture-nonlocal-helper".into());
    }
    let command = pager(&fixture.env);
    assert_eq!(command.get_program(), "less");
    let settings: std::collections::HashMap<_, _> = command.get_envs().collect();
    assert_eq!(
        settings[std::ffi::OsStr::new("LESSSECURE")],
        Some(std::ffi::OsStr::new("1"))
    );
    for name in ["LESS", "LESSOPEN", "LESSCLOSE", "LESSSECURE_ALLOW"] {
        assert!(
            settings
                .get(std::ffi::OsStr::new(name))
                .copied()
                .flatten()
                .is_none()
        );
    }
}

#[test]
fn pager_exit_failure_falls_back_and_user_quit_does_not() {
    let fixture = Fixture::new();
    let executable = fixture.bin.join("less");
    let install = |status| {
        fs::write(&executable, format!("#!/bin/sh\nexit {status}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    };
    let content = "fixture metadata\n".repeat(8192);
    install(2);
    assert!(!page(&fixture.env, &content));
    install(0);
    assert!(page(&fixture.env, &content));
}

#[test]
fn screening_scratch_is_private_under_permissive_umask() {
    let fixture = Fixture::new();
    const CHILD: &str = "DOTS_RECEIPT_UMASK_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new("/bin/sh")
            .env_clear()
            .envs(&fixture.env.vars)
            .env(CHILD, "1")
            .args(["-c", "umask 000; exec \"$@\"", "fixture"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "receipt::tests::screening_scratch_is_private_under_permissive_umask",
                "--exact",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }

    fixture.scanner(
        r#"
        set -eu
        mode=$1
        shift
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --config) config=$2; shift ;;
                --gitleaks-ignore-path) root=$2; shift ;;
                --) shift; target=$1; break ;;
            esac
            shift
        done
        assert_mode() {
            case "$(ls -ld "$2")" in "$1"*) ;; *) exit 2 ;; esac
        }
        assert_mode drwx------ "$root"
        assert_mode drwx------ .
        assert_mode drwx------ nested
        assert_mode -rw------- "$config"
        if [ "$mode" = dir ]; then assert_mode -rw------- "$target"; fi
        printf '[]
'
    "#,
    );
    fs::create_dir(fixture.root.join("nested")).unwrap();
    fs::write(fixture.root.join("nested/file"), "old\n").unwrap();
    let before = fixture.commit("older");
    fs::write(fixture.root.join("nested/file"), "new\n").unwrap();
    let after = fixture.commit("newer");
    fs::write(fixture.root.join("extra.toml"), "title = 'extra'\n").unwrap();
    fs::write(
        fixture.root.join(".gitleaks.toml"),
        "[extend]\npath = 'extra.toml'\n",
    )
    .unwrap();
    let receipt = fixture.receipt(before, after, &["nested/file"]);
    assert!(fixture.render(&receipt, true).contains("+new"));
}

#[test]
fn deadline_keeps_completed_patches_and_removes_stalled_scan_inputs() {
    let fixture = Fixture::new();
    let marker = fixture.dir.path().join("stalled-input");
    let quoted = marker.to_string_lossy().replace('\'', "'\"'\"'");
    fixture.scanner(&format!(
        r#"
        if [ "$1" = stdin ]; then
            body=$(/bin/cat)
        else
            for file do :; done
            body=$(/bin/cat "$file")
        fi
        case "$body" in
            *STALL_THIS_SCAN*)
                pwd > '{quoted}'
                exec sleep 60
                ;;
        esac
        printf '[]\n'
    "#
    ));
    fs::write(fixture.root.join("base"), "base\n").unwrap();
    let before = fixture.commit("before");
    fs::write(fixture.root.join("a-clean"), "safe retained patch\n").unwrap();
    fs::write(fixture.root.join("z-stall"), "STALL_THIS_SCAN\n").unwrap();
    let after = fixture.commit("after");
    let receipt = fixture.receipt(before, after, &["a-clean", "z-stall"]);
    let start = Instant::now();
    let output = patch::render_until(
        &fixture.env,
        &fixture.root,
        &receipt,
        true,
        patch::MAX_BYTES,
        start + Duration::from_secs(12),
    );
    assert!(start.elapsed() < Duration::from_secs(15));
    assert!(output.text.contains("+safe retained patch"));
    assert!(output.text.contains("Patch omitted for 1 file."));
    assert!(!output.text.contains("STALL_THIS_SCAN"));
    drop(output);
    let input = fs::read_to_string(marker).unwrap();
    assert!(!std::path::Path::new(input.trim()).exists());
}
