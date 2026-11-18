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
