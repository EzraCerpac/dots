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
    git.get(["init", "--quiet", "-b", "dev"]).unwrap();
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
    let (fixture, mut ctx) = git_fixture();
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
    git.get(["init", "--bare", "--quiet", remote.to_str().unwrap()])
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
