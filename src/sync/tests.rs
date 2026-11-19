use std::fs;
use std::path::Path;

use super::git::Git;
use crate::{Ctx, Env, Flags, config::Root};
use std::process::Command;

const RECEIPT_CHILD: &str = "DOTS_P08_RECEIPT_TEST_CHILD";

fn isolated(world: &Path, program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", world.join("home"))
        .env("XDG_CONFIG_HOME", world.join("xdg-config"))
        .env("XDG_DATA_HOME", world.join("xdg-data"))
        .env("XDG_STATE_HOME", world.join("cannot-create-state"))
        .env("XDG_CACHE_HOME", world.join("xdg-cache"))
        .env("CODEX_HOME", world.join("codex"))
        .env("CLAUDE_CONFIG_DIR", world.join("claude"))
        .env("MISE_CONFIG_DIR", world.join("mise-config"))
        .env("MISE_DATA_DIR", world.join("mise-data"))
        .env("MISE_STATE_DIR", world.join("mise-state"))
        .env("MISE_CACHE_DIR", world.join("mise-cache"))
        .env("MISE_SYSTEM_CONFIG_DIR", world.join("mise-system"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("JJ_CONFIG", world.join("jj"))
        .env("TMPDIR", world.join("tmp"))
        .env("DOTS_ROOT", world.join("checkout"))
        .env("DOTS_SKILL_LINKS", "0")
        .current_dir(world.join("checkout"));
    cmd
}

pub(super) fn git_fixture() -> (tempfile::TempDir, Ctx) {
    git_fixture_with_format("sha1")
}

fn git_fixture_with_format(format: &str) -> (tempfile::TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let world = tmp.path().canonicalize().unwrap();
    for directory in ["home", "checkout", "tmp", "jj"] {
        fs::create_dir(world.join(directory)).unwrap();
    }
    fs::write(
        world.join("checkout/config.toml"),
        "[vars]\ndots_profiles = 'default'\n",
    )
    .unwrap();
    let command = isolated(&world, "git");
    let env = Env {
        cwd: world.join("checkout"),
        vars: command
            .get_envs()
            .filter_map(|(key, value)| Some((key.to_owned(), value?.to_owned())))
            .collect(),
    };
    let ctx = Ctx {
        root: Root::resolve(&env).unwrap(),
        env,
        flags: Flags::default(),
    };
    let git = Git::new(&ctx, ctx.root.path());
    git.get([
        "init",
        "--quiet",
        "-b",
        "dev",
        &format!("--object-format={format}"),
    ])
    .unwrap();
    git.get(["add", "config.toml"]).unwrap();
    (tmp, ctx)
}

pub(super) fn fixture_commit(git: &Git<'_>, label: &str) -> String {
    git.get([
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "commit.gpgSign=false",
        "commit",
        "--allow-empty",
        "-qm",
        label,
    ])
    .unwrap();
    git.id(["rev-parse", "--verify", "HEAD^{commit}"]).unwrap()
}

#[test]
fn failed_receipt_save_prints_once_and_does_not_change_sync_status() {
    let (fixture, ctx) = git_fixture();
    let world = fixture.path().canonicalize().unwrap();
    fs::write(
        world.join("cannot-create-state"),
        b"a file, not a directory",
    )
    .unwrap();
    fixture_commit(&Git::new(&ctx, ctx.root.path()), "seed");
    let output = isolated(&world, std::env::current_exe().unwrap())
        .env(RECEIPT_CHILD, "1")
        .args([
            "--exact",
            "sync::tests::receipt_save_failure_child",
            "--nocapture",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.starts_with("dots sync:"))
            .count(),
        1
    );
    assert!(stdout.contains("local-only; add a remote"));
    assert_eq!(
        stderr
            .matches("could not save the sync receipt for replay")
            .count(),
        1
    );
}

#[test]
fn receipt_save_failure_child() {
    match std::env::var(RECEIPT_CHILD).ok().as_deref() {
        Some("1") => assert_eq!(
            crate::cli::run(vec!["dots".into(), "sync".into()]).unwrap(),
            0
        ),
        Some("embedded") => {
            use crate::{
                receipt::Display,
                run::{Outcome, Run, Sink},
            };
            let env = Env::current().unwrap();
            let ctx = Ctx {
                root: Root::resolve(&env).unwrap(),
                env,
                flags: Flags::default(),
            };
            let run = Run::begin(&ctx, Sink::Unreported);
            let stage = run.start("Sync");
            let receipt = super::run(&run, stage, super::load(&ctx), Display::Embedded);
            assert_eq!(receipt.stage_outcome, Outcome::Skipped);
            assert!(receipt.local_only);
        }
        Some("replay") => {
            assert_eq!(
                crate::cli::run(vec!["dots".into(), "sync".into(), "--last".into()]).unwrap(),
                0
            );
        }
        _ => (),
    }
}

#[test]
fn embedded_local_only_sync_emits_once_and_can_be_replayed() {
    let (fixture, ctx) = git_fixture();
    let world = fixture.path().canonicalize().unwrap();
    fixture_commit(&Git::new(&ctx, ctx.root.path()), "seed");
    let run_child = |mode| {
        isolated(&world, std::env::current_exe().unwrap())
            .env(RECEIPT_CHILD, mode)
            .args([
                "--exact",
                "sync::tests::receipt_save_failure_child",
                "--nocapture",
            ])
            .output()
            .unwrap()
    };
    let live = run_child("embedded");
    let output = String::from_utf8(live.stdout).unwrap();
    assert!(live.status.success(), "{output}");
    assert_eq!(
        output
            .lines()
            .filter(|line| line.starts_with("dots sync:"))
            .count(),
        1
    );
    assert!(output.contains("local-only; add a remote"));
    assert!(live.stderr.is_empty());
    let replay = run_child("replay");
    let output = String::from_utf8(replay.stdout).unwrap();
    assert!(replay.status.success(), "{output}");
    assert!(output.contains("Latest sync"));
    assert!(output.contains("local-only; add a remote"));
}

#[test]
fn push_source_ignores_a_branch_named_like_the_verified_oid() {
    use super::remote::{self, Origin};
    use crate::run::{Run, Sink};

    let (fixture, ctx) = git_fixture();
    let world = fixture.path().canonicalize().unwrap();
    let git = Git::new(&ctx, ctx.root.path());
    let approved = fixture_commit(&git, "approved");
    let different = fixture_commit(&git, "different");
    git.get(["branch", &approved, &different]).unwrap();

    let remote_path = world.join("remote");
    let remote_url = format!("file://{}", remote_path.display());
    git.get(["init", "--quiet", "--bare", remote_path.to_str().unwrap()])
        .unwrap();
    git.get(["remote", "add", "origin", &remote_url]).unwrap();
    let origin = Origin::load(&git, None).unwrap().unwrap();
    let run = Run::begin(&ctx, Sink::Unreported);
    remote::push(&git, &run, &origin, None, &approved).unwrap();

    let remote_git = Git::new(&ctx, &remote_path);
    assert_eq!(
        remote_git
            .id(["show-ref", "--hash", "--verify", "refs/heads/dev"])
            .unwrap(),
        approved
    );
}

#[test]
#[ignore = "requires the hosted mise and gitleaks lane and real history checkpoint"]
fn failed_capture_journal_still_shares_an_unrelated_edit_and_retries() {
    capture_failure_still_shares(super::dotfiles::CaptureFailure::Journal);
}

#[test]
#[ignore = "requires the hosted mise and gitleaks lane and real history checkpoint"]
fn cross_device_capture_still_shares_an_unrelated_edit_and_retries() {
    capture_failure_still_shares(super::dotfiles::CaptureFailure::Device);
}

fn capture_failure_still_shares(fault: super::dotfiles::CaptureFailure) {
    use super::dotfiles::CAPTURE_FAILURE;
    use crate::{
        receipt::Display,
        run::{Run, Sink},
    };

    let (fixture, mut ctx) = git_fixture();
    let world = fixture.path().canonicalize().unwrap();
    let root = ctx.root.path();
    ctx.env.vars.insert(
        "MISE_TRUSTED_CONFIG_PATHS".into(),
        root.as_os_str().to_owned(),
    );
    ctx.env
        .vars
        .insert("MISE_CEILING_PATHS".into(), root.as_os_str().to_owned());
    let deployed = world.join("home/deployed");
    fs::write(root.join("source"), b"tracked baseline\n").unwrap();
    fs::write(&deployed, b"deployed edit\n").unwrap();
    fs::write(
        root.join("config.toml"),
        "[vars]\ndots_profiles = 'default'\n[dotfiles]\n'~/deployed' = { mode = 'copy', source = 'source' }\n",
    )
    .unwrap();
    fs::write(
        root.join(".gitignore"),
        "/config.local.toml\n/config.host-*.toml\n/miserc.toml\n/mise.lock\n/mise.*.lock\n/locks/\n/.setup-state/\n",
    )
    .unwrap();
    let git = Git::new(&ctx, ctx.root.path());
    git.get(["config", "user.name", "Fixture"]).unwrap();
    git.get(["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    git.get(["add", ".gitignore", "config.toml", "source"])
        .unwrap();
    fixture_commit(&git, "seed");
    let remote = world.join("bare-origin");
    git.get(["init", "--bare", "--quiet", remote.to_str().unwrap()])
        .unwrap();
    git.get([
        "remote",
        "add",
        "origin",
        &format!("file://{}", remote.display()),
    ])
    .unwrap();
    fs::write(root.join("public.txt"), b"unrelated public change\n").unwrap();

    let sync = || {
        let run = Run::begin(&ctx, Sink::Unreported);
        let stage = run.start("Sync");
        super::run(&run, stage, super::load(&ctx), Display::Embedded)
    };
    CAPTURE_FAILURE.set(Some(fault));
    let first = sync();
    assert!(
        CAPTURE_FAILURE.take().is_none(),
        "capture was never attempted"
    );
    assert_eq!(
        first.outcome,
        "pushed dev",
        "{:?}",
        first
            .issues
            .iter()
            .map(|issue| &issue.diagnostic)
            .collect::<Vec<_>>()
    );
    assert!(
        first
            .issues
            .iter()
            .any(|issue| issue.diagnostic.contains("Could not capture deployed copy"))
    );
    assert_eq!(
        fs::read(root.join("source")).unwrap(),
        b"tracked baseline\n"
    );
    assert_eq!(fs::read(&deployed).unwrap(), b"deployed edit\n");
    let published = Git::new(&ctx, &remote);
    assert_eq!(
        published
            .get(["show", "refs/heads/dev:public.txt"])
            .unwrap(),
        b"unrelated public change\n"
    );
    assert_eq!(
        published.get(["show", "refs/heads/dev:source"]).unwrap(),
        b"tracked baseline\n"
    );

    let retry = sync();
    assert_eq!(retry.outcome, "pushed dev");
    assert_eq!(fs::read(root.join("source")).unwrap(), b"deployed edit\n");
    assert_eq!(fs::read(deployed).unwrap(), b"deployed edit\n");
    assert_eq!(
        published.get(["show", "refs/heads/dev:source"]).unwrap(),
        b"deployed edit\n"
    );
}

fn shared_mise_fixture(declarations: &str) -> (tempfile::TempDir, Ctx) {
    shared_mise_fixture_with_format(declarations, "sha1")
}

fn shared_mise_fixture_with_format(declarations: &str, format: &str) -> (tempfile::TempDir, Ctx) {
    let (fixture, mut ctx) = git_fixture_with_format(format);
    let world = fixture.path().canonicalize().unwrap();
    let root = ctx.root.path();
    for key in ["MISE_TRUSTED_CONFIG_PATHS", "MISE_CEILING_PATHS"] {
        ctx.env.vars.insert(key.into(), root.as_os_str().to_owned());
    }
    ctx.env
        .vars
        .insert("SETUP_PROFILE".into(), "default".into());
    fs::write(
        root.join("config.toml"),
        format!("[vars]\ndots_profiles = 'default'\n{declarations}"),
    )
    .unwrap();
    fs::write(root.join(".gitignore"), "/config.local.toml\n/config.host-*.toml\n/miserc.toml\n/mise.lock\n/mise.*.lock\n/locks/\n/.setup-state/\n").unwrap();
    let git = Git::new(&ctx, root);
    git.get(["config", "user.name", "Fixture"]).unwrap();
    git.get(["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    let remote = world.join("bare-origin");
    git.get([
        "init",
        "--bare",
        "--quiet",
        &format!("--object-format={format}"),
        remote.to_str().unwrap(),
    ])
    .unwrap();
    git.get([
        "remote",
        "add",
        "origin",
        &format!("file://{}", remote.display()),
    ])
    .unwrap();
    (fixture, ctx)
}

fn shared_mise_sync(ctx: &Ctx) -> crate::receipt::Receipt {
    use crate::{
        receipt::Display,
        run::{Run, Sink},
    };
    let run = Run::begin(ctx, Sink::Unreported);
    let stage = run.start("Sync");
    super::run(&run, stage, super::load(ctx), Display::Embedded)
}

fn native_dotfile_apply(ctx: &Ctx) {
    use crate::run::{Run, Sink};

    let run = Run::begin(ctx, Sink::Unreported);
    let command = super::validation::command(
        &run,
        ctx.root.path(),
        ["bootstrap", "dotfiles", "apply", "--yes"],
        false,
    )
    .unwrap();
    let output = run.probe(command).unwrap();
    assert!(
        output.status.success(),
        "native apply returned {}",
        output.status
    );
    let _ = run.end(0);
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn absent_keeps_an_edited_copy_known_only_to_mise() {
    let (fixture, ctx) = shared_mise_fixture(
        "[dotfiles]\n'~/edited' = { mode = 'copy', source = 'source' }\n'~/clean' = { mode = 'copy', source = 'source' }\n",
    );
    let git = Git::new(&ctx, ctx.root.path());
    let root = git.root();
    fs::write(root.join("source"), "native copy\n").unwrap();
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "native-only copies fixture");
    native_dotfile_apply(&ctx);

    let home = fixture.path().join("home");
    for name in ["edited", "clean"] {
        assert_eq!(fs::read(home.join(name)).unwrap(), b"native copy\n");
    }
    assert!(!root.join(".git/dots-sync-copies.json").exists());
    fs::write(home.join("edited"), "keep my copy\n").unwrap();
    let config = root.join("config.toml");
    let absent = fs::read_to_string(&config)
        .unwrap()
        .replace("mode = 'copy', source = 'source'", "mode = 'absent'");
    fs::write(&config, &absent).unwrap();

    for _ in 0..2 {
        let receipt = shared_mise_sync(&ctx);
        assert!(
            receipt.issues.iter().any(|issue| {
                issue.diagnostic.contains("edited former copy")
                    && issue.diagnostic.contains("/edited")
                    && issue
                        .next_action
                        .contains("Move the deployed file somewhere safe")
            }),
            "{:?}",
            receipt.issues
        );
        assert_eq!(fs::read(home.join("edited")).unwrap(), b"keep my copy\n");
        assert!(!home.join("clean").exists());
    }
    let remote = git.at(&fixture.path().join("bare-origin"));
    assert_eq!(
        remote.get(["show", "refs/heads/dev:config.toml"]).unwrap(),
        absent.as_bytes()
    );
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn sha256_preview_preserves_ignores_and_manifest_with_sha1_default() {
    let (fixture, mut ctx) = shared_mise_fixture_with_format(
        "[dotfiles]\n'~/tree' = { mode = 'copy', source = 'tree', manifest = 'git' }\n",
        "sha256",
    );
    ctx.env
        .vars
        .insert("GIT_DEFAULT_HASH".into(), "sha1".into());
    let git = Git::new(&ctx, ctx.root.path());
    assert_eq!(
        git.text(["rev-parse", "--show-object-format"]).unwrap(),
        "sha256"
    );
    let default = fixture.path().join("default-format");
    fs::create_dir(&default).unwrap();
    let default = git.at(&default);
    default.get(["init", "--quiet", "--template="]).unwrap();
    assert_eq!(
        default.text(["rev-parse", "--show-object-format"]).unwrap(),
        "sha1"
    );

    let root = git.root();
    fs::create_dir(root.join("tree")).unwrap();
    fs::write(root.join("tree/included"), "tracked leaf\n").unwrap();
    fs::write(root.join("tree/private"), "ignored leaf\n").unwrap();
    fs::write(root.join("tree/.gitignore"), "/private\n").unwrap();
    let local = b"[vars]\np08_local = 'machine only'\n";
    fs::write(root.join("config.local.toml"), local).unwrap();
    git.get(["add", "."]).unwrap();
    let expected = fixture_commit(&git, "SHA-256 fixture");
    let receipt = shared_mise_sync(&ctx);
    assert!(receipt.issues.is_empty(), "{:?}", receipt.issues);
    assert_eq!(receipt.outcome, "pushed dev");
    let remote = git.at(&fixture.path().join("bare-origin"));
    assert_eq!(
        remote.id(["rev-parse", "refs/heads/dev"]).unwrap(),
        expected
    );
    let paths = remote
        .get(["ls-tree", "-r", "--name-only", "refs/heads/dev"])
        .unwrap();
    assert!(
        !paths.split(|byte| *byte == b'\n').any(|path| [
            b"config.local.toml".as_slice(),
            b"tree/private"
        ]
        .contains(&path))
    );
    assert_eq!(fs::read(root.join("config.local.toml")).unwrap(), local);
    let target = fixture.path().join("home/tree");
    assert_eq!(
        fs::read(target.join("included")).unwrap(),
        b"tracked leaf\n"
    );
    assert!(!target.join("private").exists());
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn validated_crlf_config_deploys_the_copy() {
    let (fixture, ctx) =
        shared_mise_fixture("[dotfiles]\n'~/deployed' = { mode = 'copy', source = 'source' }\n");
    let git = Git::new(&ctx, ctx.root.path());
    fs::write(
        git.root().join(".gitattributes"),
        "config.toml text eol=crlf\n",
    )
    .unwrap();
    fs::write(git.root().join("source"), "approved source\n").unwrap();
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "checkout representation fixture");
    fs::remove_file(git.root().join("config.toml")).unwrap();
    git.get(["checkout-index", "--force", "--", "config.toml"])
        .unwrap();
    assert!(
        fs::read(git.root().join("config.toml"))
            .unwrap()
            .windows(2)
            .any(|bytes| bytes == b"\r\n")
    );
    let receipt = shared_mise_sync(&ctx);
    assert!(receipt.issues.is_empty(), "{:?}", receipt.issues);
    assert_eq!(
        fs::read(fixture.path().join("home/deployed")).unwrap(),
        b"approved source\n"
    );
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn ordinary_crlf_sources_deploy_as_checked_out() {
    let (fixture, ctx) = shared_mise_fixture(
        "[dotfiles]\n'~/copied' = { mode = 'copy', source = 'source' }\n'~/linked' = { mode = 'symlink', source = 'source' }\n",
    );
    let git = Git::new(&ctx, ctx.root.path());
    let source = git.root().join("source");
    let lf = b"first\nsecond\n";
    let crlf = b"first\r\nsecond\r\n";
    fs::write(git.root().join(".gitattributes"), "source text eol=crlf\n").unwrap();
    fs::write(&source, lf).unwrap();
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "filtered dotfile source fixture");
    assert_eq!(git.get(["show", "HEAD:source"]).unwrap(), lf);

    fs::remove_file(&source).unwrap();
    git.get(["checkout-index", "-u", "--", "source"]).unwrap();
    assert_eq!(fs::read(&source).unwrap(), crlf);
    let status = git
        .get(["status", "--porcelain=v1", "-z", "--no-renames"])
        .unwrap();
    assert!(
        status.is_empty(),
        "dirty fixture (first four entries, paths limited to 160 bytes): {:?}",
        status
            .split(|byte| *byte == 0)
            .filter_map(|entry| entry.split_at_checked(3))
            .take(4)
            .map(|(code, path)| (
                String::from_utf8_lossy(&code[..2]),
                String::from_utf8_lossy(&path[..path.len().min(160)]),
            ))
            .collect::<Vec<_>>()
    );

    let receipt = shared_mise_sync(&ctx);
    assert!(receipt.issues.is_empty(), "{:?}", receipt.issues);
    let home = fixture.path().join("home");
    let copied = home.join("copied");
    assert_eq!(fs::read(&source).unwrap(), crlf);
    assert_eq!(fs::read(&copied).unwrap(), crlf);
    let link = home.join("linked");
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(link.canonicalize().unwrap(), source);
    assert_eq!(fs::read(&link).unwrap(), crlf);

    let edited = b"first\r\nchanged copy\r\n";
    fs::write(&copied, edited).unwrap();
    let remote = git.at(&fixture.path().join("bare-origin"));
    let receipt = shared_mise_sync(&ctx);
    assert!(receipt.issues.is_empty(), "{:?}", receipt.issues);
    assert_eq!(fs::read(&source).unwrap(), edited);
    assert_eq!(fs::read(&copied).unwrap(), edited);
    assert_eq!(fs::read(&link).unwrap(), edited);
    assert_eq!(
        remote.get(["show", "refs/heads/dev:source"]).unwrap(),
        b"first\nchanged copy\n"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(git.root().join(".git/dots-sync-copies.json")).unwrap())
            .unwrap();
    assert_eq!(state["captured"], serde_json::json!({}));

    let published = remote.id(["rev-parse", "refs/heads/dev"]).unwrap();
    let repeat = shared_mise_sync(&ctx);
    assert!(repeat.issues.is_empty(), "{:?}", repeat.issues);
    assert!(repeat.saved.is_empty());
    assert_eq!(
        remote.id(["rev-parse", "refs/heads/dev"]).unwrap(),
        published
    );
    assert_eq!(fs::read(source).unwrap(), edited);
    assert_eq!(fs::read(copied).unwrap(), edited);
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn native_copy_deployment_supplies_the_provisional_baseline() {
    use crate::run::{Run, Sink};
    use std::os::unix::fs::PermissionsExt;

    let a = b"committed A\n";
    let b = b"native B\n";
    let c = b"source C\n";
    for case in ["source", "copy", "two-sided", "mode", "late"] {
        let (fixture, ctx) = shared_mise_fixture(
            "[dotfiles]\n'~/deployed' = { mode = 'copy', source = 'source', permissions = '0644' }\n",
        );
        let git = Git::new(&ctx, ctx.root.path());
        let source = git.root().join("source");
        let target = fixture.path().join("home/deployed");
        fs::write(&source, a).unwrap();
        git.get(["add", "."]).unwrap();
        fixture_commit(&git, "native copy baseline A");

        fs::write(&source, b).unwrap();
        let native = Run::begin(&ctx, Sink::Unreported);
        let command = super::validation::command(
            &native,
            git.root(),
            ["bootstrap", "dotfiles", "apply", "--yes"],
            false,
        )
        .unwrap();
        let deployed = native.probe(command).unwrap();
        assert!(
            deployed.status.success(),
            "native apply returned {}",
            deployed.status
        );
        let _ = native.end(0);
        assert_eq!(fs::read(&target).unwrap(), b);
        assert_eq!(git.get(["show", "HEAD:source"]).unwrap(), a);
        let records = fixture.path().join("mise-state/dotfiles/targets");
        let record = fs::read_dir(records)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .unwrap();
        let native_record = fs::read(&record).unwrap();

        let copy_edit = b"edited copy\n";
        let late_edit = b"late competing copy\n";
        if case != "copy" {
            fs::write(&source, c).unwrap();
        }
        match case {
            "copy" => fs::write(&target, copy_edit).unwrap(),
            "two-sided" => fs::write(&target, a).unwrap(),
            "mode" => {
                fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            }
            "late" => {
                let hook = git.root().join(".git/hooks/pre-push");
                fs::write(
                    &hook,
                    "#!/bin/sh\nprintf 'late competing copy\\n' > \"$HOME/deployed\"\n",
                )
                .unwrap();
                fs::set_permissions(hook, fs::Permissions::from_mode(0o700)).unwrap();
            }
            _ => (),
        }
        let receipt = shared_mise_sync(&ctx);
        let (expected_source, expected_target, issue): (&[u8], &[u8], _) = match case {
            "copy" => (copy_edit, copy_edit, None),
            "two-sided" => (c, a, Some("Both the copy and source changed")),
            "mode" => (c, b, Some("Kept a locally changed copy mode")),
            "late" => (c, late_edit, Some("Kept a changed or unmanaged dotfile")),
            _ => (c, c, None),
        };
        match issue {
            Some(message) => assert!(
                receipt
                    .issues
                    .iter()
                    .any(|issue| issue.diagnostic.contains(message)),
                "{case}: {:?}",
                receipt.issues
            ),
            None => assert!(receipt.issues.is_empty(), "{case}: {:?}", receipt.issues),
        }
        assert_eq!(fs::read(&source).unwrap(), expected_source, "{case}");
        assert_eq!(fs::read(&target).unwrap(), expected_target, "{case}");
        let remote = git.at(&fixture.path().join("bare-origin"));
        assert_eq!(
            remote.get(["show", "refs/heads/dev:source"]).unwrap(),
            expected_source,
            "{case}"
        );
        if case == "mode" {
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        if case == "source" {
            // Native B still exists on disk as metadata. It cannot override
            // the confirmed sync deployment C if the user writes B again.
            assert_eq!(fs::read(&record).unwrap(), native_record);
            fs::write(&target, b).unwrap();
            fs::write(&source, b"source D\n").unwrap();
            let stale = shared_mise_sync(&ctx);
            assert!(
                stale.issues.iter().any(|issue| issue
                    .diagnostic
                    .contains("Both the copy and source changed")),
                "{:?}",
                stale.issues
            );
            assert_eq!(fs::read(&target).unwrap(), b);
            assert_eq!(fs::read(&source).unwrap(), b"source D\n");
            assert_eq!(
                remote.get(["show", "refs/heads/dev:source"]).unwrap(),
                b"source D\n"
            );
        }
    }
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn rendered_variant_preserves_declaring_rules() {
    let (fixture, ctx) = shared_mise_fixture(
        r#"[dotfiles]
'~/declaration-key' = { mode = "copy", source = "./tree", exclude = ["skip"], dot_prefix = true, manifest = "git", variants = [{ profile = "default", target = "{{ env.HOME }}/variant" }] }
"#,
    );
    let git = Git::new(&ctx, ctx.root.path());
    fs::create_dir(git.root().join("tree")).unwrap();
    for name in ["dot-leaf", "skip"] {
        fs::write(git.root().join("tree").join(name), name).unwrap();
    }
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "variant declaration fixture");
    let receipt = shared_mise_sync(&ctx);
    assert!(receipt.issues.is_empty(), "{:?}", receipt.issues);
    let home = fixture.path().join("home");
    assert_eq!(fs::read(home.join("variant/.leaf")).unwrap(), b"dot-leaf");
    assert!(!home.join("variant/skip").exists());
    assert!(!home.join("declaration-key").exists());
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn adopted_directory_copy_keeps_a_locally_changed_leaf_mode() {
    use std::os::unix::fs::PermissionsExt;

    let (fixture, ctx) =
        shared_mise_fixture("[dotfiles]\n'~/tree' = { mode = 'copy', source = 'source' }\n");
    let git = Git::new(&ctx, ctx.root.path());
    let root = git.root();
    fs::create_dir(root.join("source")).unwrap();
    for leaf in ["clean", "edited"] {
        let source = root.join("source").join(leaf);
        fs::write(&source, "initial\n").unwrap();
        fs::set_permissions(source, fs::Permissions::from_mode(0o600)).unwrap();
    }
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "directory mode preservation fixture");
    native_dotfile_apply(&ctx);
    assert!(!root.join(".git/dots-sync-copies.json").exists());

    let target = fixture.path().join("home/tree");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    for leaf in ["clean", "edited"] {
        assert_eq!(mode(&target.join(leaf)), 0o600);
        assert_eq!(fs::read(target.join(leaf)).unwrap(), b"initial\n");
        fs::write(root.join("source").join(leaf), "source updated\n").unwrap();
    }
    fs::set_permissions(target.join("edited"), fs::Permissions::from_mode(0o644)).unwrap();
    let receipt = shared_mise_sync(&ctx);
    assert!(
        receipt.issues.iter().any(|issue| {
            issue.diagnostic.contains("mode") && issue.diagnostic.contains("tree/edited")
        }),
        "{:?}",
        receipt.issues
    );
    assert_eq!(fs::read(target.join("clean")).unwrap(), b"source updated\n");
    assert_eq!(mode(&target.join("clean")), 0o600);
    assert_eq!(fs::read(target.join("edited")).unwrap(), b"initial\n");
    assert_eq!(mode(&target.join("edited")), 0o644);
    let remote = git.at(&fixture.path().join("bare-origin"));
    for leaf in ["clean", "edited"] {
        assert_eq!(
            remote
                .get(["show", &format!("refs/heads/dev:source/{leaf}")])
                .unwrap(),
            b"source updated\n"
        );
    }
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn create_only_sources_and_existing_targets_are_preserved() {
    let (fixture, ctx) = shared_mise_fixture(
        "[dotfiles]\n'~/copy' = { mode = 'copy', source = 'source', create_if_missing = true }\n'~/template' = { mode = 'template', source = 'template.tera', create_if_missing = true }\n'~/missing' = { mode = 'copy', source = 'source', create_if_missing = true }\n",
    );
    let git = Git::new(&ctx, ctx.root.path());
    let home = fixture.path().join("home");
    fs::write(home.join("copy"), "existing copy\n").unwrap();
    fs::write(home.join("template"), "existing template\n").unwrap();
    fs::write(git.root().join("source"), "approved source\n").unwrap();
    fs::write(
        git.root().join("template.tera"),
        "{{ 'incoming template' }}\n",
    )
    .unwrap();
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "create-only fixture");
    let receipt = shared_mise_sync(&ctx);
    assert!(receipt.issues.is_empty(), "{:?}", receipt.issues);
    assert_eq!(
        fs::read(git.root().join("source")).unwrap(),
        b"approved source\n"
    );
    assert_eq!(fs::read(home.join("copy")).unwrap(), b"existing copy\n");
    assert_eq!(
        fs::read(home.join("template")).unwrap(),
        b"existing template\n"
    );
    assert_eq!(
        fs::read(home.join("missing")).unwrap(),
        b"approved source\n"
    );
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn a_flagged_live_source_is_never_deployed() {
    let (fixture, ctx) =
        shared_mise_fixture("[dotfiles]\n'~/deployed' = { mode = 'copy', source = 'source' }\n");
    let git = Git::new(&ctx, ctx.root.path());
    let source = git.root().join("source");
    fs::write(&source, "approved source\n").unwrap();
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "approved source");
    let flagged = ["ghp_", "Ab3De6Gh9Jk2Mn5Pq8St1Uv4Wx7Yz0Bc3Ef6"].concat();
    fs::write(&source, format!("{flagged}\n")).unwrap();
    let receipt = shared_mise_sync(&ctx);
    assert!(
        receipt
            .issues
            .iter()
            .any(|issue| issue.diagnostic.contains("gitleaks flagged")),
        "{:?}",
        receipt.issues
    );
    assert_eq!(fs::read_to_string(source).unwrap(), format!("{flagged}\n"));
    assert_eq!(
        fs::read(fixture.path().join("home/deployed")).unwrap(),
        b"approved source\n"
    );
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn flagged_staged_entries_survive_sharing_the_clean_remainder() {
    use std::os::unix::fs::PermissionsExt;

    for distinct in [false, true] {
        let (fixture, ctx) = shared_mise_fixture("");
        let git = Git::new(&ctx, ctx.root.path());
        let path = git.root().join("kept");
        fs::write(&path, "public baseline\n").unwrap();
        git.get(["add", "."]).unwrap();
        fixture_commit(&git, "staged secret preservation fixture");
        let token = ["ghp_", "Ab3De6Gh9Jk2Mn5Pq8St1Uv4Wx7Yz0Bc3Ef6"].concat();
        let staged = format!("token = {token}\n");
        fs::write(&path, &staged).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        git.get(["add", "kept"]).unwrap();
        let index_entry = git
            .get(["ls-files", "--stage", "-z", "--", "kept"])
            .unwrap();
        assert!(index_entry.starts_with(b"100755 "));
        let live = if distinct {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            format!("different working bytes\n{staged}")
        } else {
            staged.clone()
        };
        fs::write(&path, &live).unwrap();
        fs::write(git.root().join("clean"), "share this change\n").unwrap();

        let receipt = shared_mise_sync(&ctx);
        assert_eq!(receipt.outcome, "pushed dev", "{:?}", receipt.issues);
        assert_eq!(receipt.kept, 1);
        assert!(receipt.issues.iter().any(|issue| {
            issue.diagnostic.contains("gitleaks flagged") && issue.diagnostic.contains("kept")
        }));
        assert_eq!(
            git.get(["ls-files", "--stage", "-z", "--", "kept"])
                .unwrap(),
            index_entry
        );
        assert_eq!(git.get(["show", ":kept"]).unwrap(), staged.as_bytes());
        assert_eq!(fs::read(&path).unwrap(), live.as_bytes());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            if distinct { 0o644 } else { 0o755 }
        );
        let remote = git.at(&fixture.path().join("bare-origin"));
        assert_eq!(
            remote.get(["show", "refs/heads/dev:clean"]).unwrap(),
            b"share this change\n"
        );
        assert_eq!(
            remote.get(["show", "refs/heads/dev:kept"]).unwrap(),
            b"public baseline\n"
        );
    }
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn generated_copy_contents_are_published_on_the_next_sync() {
    let (fixture, ctx) = shared_mise_fixture(
        r#"[dotfiles]
'~/deployed' = { mode = 'copy', source = 'source' }
[bootstrap.hooks]
pre-dotfiles = "printf 'generated\\n' > source"
"#,
    );
    let git = Git::new(&ctx, ctx.root.path());
    let source = git.root().join("source");
    fs::write(&source, "public source\n").unwrap();
    git.get(["add", "."]).unwrap();
    let expected = fixture_commit(&git, "generated copy baseline");
    let first = shared_mise_sync(&ctx);
    assert!(first.issues.is_empty(), "{:?}", first.issues);
    assert_eq!(
        fs::read(fixture.path().join("home/deployed")).unwrap(),
        b"generated\n"
    );
    assert_eq!(fs::read(&source).unwrap(), b"generated\n");
    assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), expected);
    let second = shared_mise_sync(&ctx);
    assert!(second.issues.is_empty(), "{:?}", second.issues);
    let remote = Git::new(&ctx, &fixture.path().join("bare-origin"));
    assert_eq!(
        remote.get(["show", "refs/heads/dev:source"]).unwrap(),
        b"generated\n"
    );
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn rejected_group_blocks_its_dependent_by_group_name() {
    let (fixture, ctx) = shared_mise_fixture(
        r#"[dotfiles]
'~/consumer' = { mode = "copy", source = "consumer.txt", depends = ["prerequisite"] }
'~/independent' = { mode = "copy", source = "independent.txt" }
[dotfile_groups.prerequisite]
root = "tree"
target = "~/predecessor"
mode = "copy"
[settings.dotfiles]
root = "{{ mise_config_dir }}"
[bootstrap.hooks]
pre-dotfiles = '''
if test -e "$HOME/reject"; then
    sed 's/root = "tree"/root = "other-tree"/' config.toml > new-config
    mv new-config config.toml
fi
'''
"#,
    );
    let git = Git::new(&ctx, ctx.root.path());
    let root = git.root();
    for directory in ["tree", "other-tree"] {
        fs::create_dir(root.join(directory)).unwrap();
    }
    for name in [
        "tree/leaf",
        "consumer.txt",
        "independent.txt",
        "other-tree/leaf",
    ] {
        fs::write(root.join(name), format!("original {name}\n")).unwrap();
    }
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "named group dependency fixture");
    let first = shared_mise_sync(&ctx);
    assert!(first.issues.is_empty(), "{:?}", first.issues);
    let home = fixture.path().join("home");
    assert_eq!(
        fs::read(home.join("predecessor/leaf")).unwrap(),
        b"original tree/leaf\n"
    );
    assert_eq!(
        fs::read(home.join("consumer")).unwrap(),
        b"original consumer.txt\n"
    );

    fs::write(home.join("reject"), "").unwrap();
    for name in ["tree/leaf", "consumer.txt", "independent.txt"] {
        fs::write(root.join(name), format!("updated {name}\n")).unwrap();
    }
    let receipt = shared_mise_sync(&ctx);
    assert!(
        receipt.issues.iter().any(|issue| {
            issue.diagnostic.contains("validated selector changed")
                && issue.diagnostic.contains("predecessor")
        }),
        "{:?}",
        receipt.issues
    );
    assert!(
        receipt.issues.iter().any(|issue| {
            issue.diagnostic.contains("dependency was blocked")
                && issue.diagnostic.contains("consumer")
        }),
        "{:?}",
        receipt.issues
    );
    assert!(
        fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("root = \"other-tree\"")
    );
    assert_eq!(
        fs::read(home.join("predecessor/leaf")).unwrap(),
        b"original tree/leaf\n"
    );
    assert_eq!(
        fs::read(home.join("consumer")).unwrap(),
        b"original consumer.txt\n"
    );
    assert_eq!(
        fs::read(home.join("independent")).unwrap(),
        b"updated independent.txt\n"
    );
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn guarded_hook_replay_does_not_displace_a_competing_live_source() {
    use std::os::unix::fs::PermissionsExt;

    let (fixture, ctx) = shared_mise_fixture(
        r#"[dotfiles]
'~/deployed' = { mode = 'copy', source = 'source' }
[bootstrap.hooks]
pre-dotfiles = "git ls-files --error-unmatch source >/dev/null; printf 'generated\n' > marker"
"#,
    );
    let git = Git::new(&ctx, ctx.root.path());
    let source = git.root().join("source");
    fs::write(&source, "approved source\n").unwrap();
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "guarded hook fixture");
    let hook = git.root().join(".git/hooks/pre-push");
    fs::write(&hook, "#!/bin/sh\nprintf 'competing\\n' > source\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let receipt = shared_mise_sync(&ctx);
    assert_eq!(receipt.outcome, "pushed dev");
    assert_eq!(fs::read(source).unwrap(), b"competing\n");
    assert_eq!(fs::read(git.root().join("marker")).unwrap(), b"generated\n");
    assert_eq!(
        fs::read(fixture.path().join("home/deployed")).unwrap(),
        b"approved source\n"
    );
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn a_new_target_claimed_during_push_keeps_the_competing_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let (fixture, ctx) = shared_mise_fixture(
        "[dotfiles]\n'~/new-target' = { mode = 'template', source = 'source.tera' }\n",
    );
    let git = Git::new(&ctx, ctx.root.path());
    fs::write(git.root().join("source.tera"), "validated contents\n").unwrap();
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "validated new target");
    let hook = git.root().join(".git/hooks/pre-push");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf 'competing edit\\n' > \"$HOME/new-target\"\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let receipt = shared_mise_sync(&ctx);
    assert_eq!(receipt.outcome, "pushed dev");
    assert!(
        receipt
            .issues
            .iter()
            .any(|issue| issue.diagnostic.contains("new-target")),
        "{:?}",
        receipt.issues
    );
    assert_eq!(
        fs::read(fixture.path().join("home/new-target")).unwrap(),
        b"competing edit\n"
    );
}

#[test]
#[ignore = "requires real mise, gitleaks and the shared history checkpoint"]
fn failed_reload_remains_pending_after_a_completed_copy_write() {
    let (fixture, ctx) = shared_mise_fixture(
        r#"[dotfiles]
'~/deployed' = { mode = 'copy', source = 'source' }
[history.reload]
'~/deployed' = "test -e \"$HOME/reload-ready\" || exit 12; touch \"$HOME/reloaded\""
"#,
    );
    let git = Git::new(&ctx, ctx.root.path());
    fs::write(git.root().join("source"), "approved source\n").unwrap();
    git.get(["add", "."]).unwrap();
    fixture_commit(&git, "reload fixture");
    let home = fixture.path().join("home");
    let first = shared_mise_sync(&ctx);
    assert!(
        first
            .issues
            .iter()
            .any(|issue| issue.diagnostic.contains("reload failed (exit 12)")),
        "{:?}",
        first.issues
    );
    assert_eq!(
        fs::read(home.join("deployed")).unwrap(),
        b"approved source\n"
    );
    assert!(!home.join("reloaded").exists());
    fs::write(home.join("reload-ready"), "").unwrap();
    let unchanged = shared_mise_sync(&ctx);
    assert!(unchanged.issues.is_empty(), "{:?}", unchanged.issues);
    assert!(!home.join("reloaded").exists());
    fs::write(git.root().join("source"), "new approved source\n").unwrap();
    let second = shared_mise_sync(&ctx);
    assert!(second.issues.is_empty(), "{:?}", second.issues);
    assert!(home.join("reloaded").is_file());
}

#[test]
fn dev_binding_rejects_a_clean_switch_to_the_same_commit() {
    let (_fixture, ctx) = git_fixture();
    let git = Git::new(&ctx, ctx.root.path());
    let expected = fixture_commit(&git, "dev baseline");
    super::verify_dev(&git, &expected).unwrap();
    git.get(["checkout", "-qb", "other"]).unwrap();
    assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), expected);
    assert!(super::verify_dev(&git, &expected).is_err());
}

#[test]
fn unconfirmed_push_record_retains_both_exact_tips_and_expected_absence() {
    let candidate = "a".repeat(40);
    let previous = "b".repeat(40);
    for expected in [None, Some(previous.as_str())] {
        let mut receipt = crate::receipt::Receipt::new(None, "not pushed");
        super::unconfirmed_push(&mut receipt, expected, &candidate);
        assert!(receipt.shared.is_empty());
        assert_eq!(receipt.outcome, "dev push unconfirmed");
        let expected = expected.unwrap_or("(absent)");
        assert!(
            receipt.issues[0]
                .diagnostic
                .contains(&format!("candidate {candidate} · expected {expected}"))
        );
        assert!(
            receipt.issues[0]
                .next_action
                .contains("Compare refs/heads/dev")
        );
    }
}

#[test]
fn completed_and_resumed_checkout_name_the_final_backup_in_live_and_replay_receipts() {
    use std::os::unix::fs::PermissionsExt;

    for displaced in [false, true] {
        let (fixture, ctx) = git_fixture();
        let world = fixture.path().canonicalize().unwrap();
        let git = Git::new(&ctx, ctx.root.path());
        let target = git.root().join("file");
        fs::write(&target, b"old working version").unwrap();
        git.get(["add", "file"]).unwrap();
        let before = fixture_commit(&git, "before");
        fs::write(&target, b"incoming version").unwrap();
        git.get(["add", "file"]).unwrap();
        let after = fixture_commit(&git, "incoming");
        git.get(["reset", "--hard", &before]).unwrap();

        let dir = git.root().join(".git");
        super::checkout::begin(
            &git,
            &dir,
            &super::checkout::index_snapshot(&dir).unwrap(),
            &before,
            &after,
            true,
        )
        .unwrap();
        if displaced {
            fs::rename(&target, dir.join("dots-sync-pending/old-0")).unwrap();
        }

        let run_child = |mode| {
            let output = isolated(&world, std::env::current_exe().unwrap())
                .env(RECEIPT_CHILD, mode)
                .args([
                    "--exact",
                    "sync::tests::receipt_save_failure_child",
                    "--nocapture",
                ])
                .output()
                .unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(
                output.status.success(),
                "{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            stdout
        };
        let live = run_child("1");
        let backup = live
            .lines()
            .find_map(|line| line.strip_prefix("Recovery copy at "))
            .unwrap();
        let backup = Path::new(backup);
        assert!(backup.starts_with(&dir));
        assert!(
            backup
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("dots-sync-recovery-")
        );
        assert_eq!(
            fs::metadata(backup.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        assert_eq!(fs::read(backup).unwrap(), b"old working version");
        assert_eq!(fs::read(&target).unwrap(), b"incoming version");
        assert!(!dir.join("dots-sync-pending").exists());

        let replay = run_child("replay");
        assert!(
            replay
                .lines()
                .any(|line| line.strip_prefix("Recovery copy at ") == backup.to_str())
        );
        assert_eq!(fs::read(backup).unwrap(), b"old working version");
    }
}

#[test]
fn started_checkout_conflicts_report_both_revisions_and_preserve_recovery() {
    use std::os::unix::fs::MetadataExt;

    for conflict in ["staged index", "late file"] {
        let (fixture, ctx) = git_fixture();
        let world = fixture.path().canonicalize().unwrap();
        let git = Git::new(&ctx, ctx.root.path());
        let early = git.root().join("alpha");
        let late = git.root().join("zeta");
        fs::write(&early, b"original alpha\n").unwrap();
        fs::write(&late, b"original zeta\n").unwrap();
        git.get(["add", "alpha", "zeta"]).unwrap();
        let before = fixture_commit(&git, "private before");
        fs::write(&early, b"incoming alpha\n").unwrap();
        fs::write(&late, b"incoming zeta\n").unwrap();
        git.get(["add", "alpha", "zeta"]).unwrap();
        let after = fixture_commit(&git, "private after");
        git.get(["reset", "--hard", &before]).unwrap();

        let dir = git.root().join(".git");
        super::checkout::begin(
            &git,
            &dir,
            &super::checkout::index_snapshot(&dir).unwrap(),
            &before,
            &after,
            true,
        )
        .unwrap();
        // Enter the real checkout and stop only after alpha was replaced.
        let interrupted = super::checkout::finish(&git, &dir, || {
            fs::read(&early).is_ok_and(|bytes| bytes == b"incoming alpha\n")
        });
        assert!(interrupted.is_err());
        let pending = dir.join("dots-sync-pending");
        assert!(pending.join("started").is_file());
        assert!(pending.join("done-0").is_file());
        assert!(!pending.join("done-1").exists());
        let original = pending.join("old-0");
        assert_eq!(fs::read(&original).unwrap(), b"original alpha\n");

        if conflict == "staged index" {
            // Simulate the owner releasing its reserved lock to allow a real
            // competing git add. Only release our fixture's hard-linked lock.
            let lock = dir.join("index.lock");
            let held = fs::metadata(&lock).unwrap();
            let staged = fs::metadata(pending.join("index.after")).unwrap();
            assert_eq!((held.dev(), held.ino()), (staged.dev(), staged.ino()));
            fs::remove_file(lock).unwrap();
            fs::write(&late, b"competing staged contents\n").unwrap();
            git.get(["add", "zeta"]).unwrap();
        }
        fs::write(&late, b"competing live contents\n").unwrap();
        let staging = git
            .get(["ls-files", "--stage", "--", "alpha", "zeta"])
            .unwrap();
        let snapshot = super::checkout::index_snapshot(&dir).unwrap();
        let expected_staged: &[u8] = if conflict == "staged index" {
            b"competing staged contents\n"
        } else {
            b"original zeta\n"
        };
        assert_eq!(git.get(["show", ":zeta"]).unwrap(), expected_staged);
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), before);

        let cli = |mode| {
            let child = isolated(&world, std::env::current_exe().unwrap())
                .env(RECEIPT_CHILD, mode)
                .args([
                    "--exact",
                    "sync::tests::receipt_save_failure_child",
                    "--nocapture",
                ])
                .output()
                .unwrap();
            assert!(child.status.success(), "{child:?}");
            String::from_utf8(child.stdout).unwrap()
        };
        for receipt in [cli("1"), cli("replay")] {
            assert!(
                receipt.contains(&format!("recorded revisions {before} to {after}")),
                "{receipt}"
            );
            assert!(
                receipt.contains("reconcile the index and checkout"),
                "{receipt}"
            );
            assert!(
                receipt.contains("A guarded checkout is pending"),
                "{receipt}"
            );
            assert!(receipt.contains("rerun dots sync"), "{receipt}");
            assert_eq!(
                receipt.contains("Kept a late edit"),
                conflict == "late file",
                "{receipt}"
            );
            assert!(
                receipt
                    .lines()
                    .any(|line| { line.strip_prefix("Recovery copy at ") == original.to_str() }),
                "{receipt}"
            );
        }
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), before);
        assert_eq!(super::checkout::index_snapshot(&dir).unwrap(), snapshot);
        assert_eq!(
            git.get(["ls-files", "--stage", "--", "alpha", "zeta"])
                .unwrap(),
            staging
        );
        assert_eq!(git.get(["show", ":zeta"]).unwrap(), expected_staged);
        assert_eq!(fs::read(&early).unwrap(), b"incoming alpha\n");
        assert_eq!(fs::read(&late).unwrap(), b"competing live contents\n");
        assert_eq!(fs::read(original).unwrap(), b"original alpha\n");
        assert!(pending.join("started").is_file());
    }
}
