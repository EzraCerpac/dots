mod support;

use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use support::World;

fn fresh(world: &World) -> PathBuf {
    world.root().with_file_name("fresh")
}

fn init(world: &World, root: &Path) -> Output {
    fixture_dots(world).arg("init").arg(root).output().unwrap()
}

// World clears the caller's environment. Make the optional agent paths and
// each mise directory explicit as well, and fail closed before real mise runs.
fn fixture_dots(world: &World) -> Command {
    let mut command = world.dots();
    let scope = world.home().parent().unwrap();
    for (key, dir) in [
        ("TMPDIR", "tmp"),
        ("CODEX_HOME", "codex"),
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("MISE_SYSTEM_CONFIG_DIR", "mise/system"),
        ("MISE_DATA_DIR", "mise/data"),
        ("MISE_STATE_DIR", "mise/state"),
        ("MISE_CACHE_DIR", "mise/cache"),
        ("MISE_TMP_DIR", "mise/tmp"),
    ] {
        let path = world.home().join(dir);
        fs::create_dir_all(&path).unwrap();
        command.env(key, path);
    }
    let binary = env::split_paths(&env::var_os("PATH").unwrap())
        .map(|dir| dir.join("mise"))
        .find(|path| path.is_absolute() && path.is_file())
        .expect("mise executable on PATH");
    let guard = world.home().join("mise-guard");
    write_executable(
        &guard,
        r#"#!/bin/sh
set -eu
test "${__MISE_DIFF+x}" != x
for dir in "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" \
    "$TMPDIR" "$CODEX_HOME" "$CLAUDE_CONFIG_DIR" "$MISE_CONFIG_DIR" "$MISE_SYSTEM_CONFIG_DIR" \
    "$MISE_DATA_DIR" "$MISE_STATE_DIR" "$MISE_CACHE_DIR" "$MISE_TMP_DIR"; do
    case "$dir" in
        "$DOTS_TEST_SCOPE"/*) ;;
        *) printf '%s\n' 'fixture refused an unisolated mise directory' >&2; exit 96 ;;
    esac
done
exec "$DOTS_TEST_REAL_MISE" "$@"
"#,
    );
    command
        .env("MISE_CONFIG_DIR", world.root())
        .env("MISE_OFFLINE", "1")
        .env("MISE_NO_HOOKS", "1")
        .env("MISE_AUTO_INSTALL", "0")
        .env("DOTS_MISE_BIN", guard)
        .env("DOTS_TEST_SCOPE", scope)
        .env("DOTS_TEST_REAL_MISE", binary);
    command
}

fn success(out: Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn git(world: &World, root: &Path, args: &[&str]) -> String {
    success(world.git().current_dir(root).args(args).output().unwrap())
        .trim_end_matches('\n')
        .to_owned()
}

fn seed_push(output: &str, seed: &str) -> bool {
    output.lines().any(|line| {
        line.starts_with("git -C ")
            && line.ends_with(&format!(
                " push --no-follow-tags --recurse-submodules=no origin '{seed}^{{commit}}:refs/heads/main' '{seed}^{{commit}}:refs/heads/dev'"
            ))
    })
}

fn enable_fake_gh(world: &World, cmd: &mut Command) {
    let bin = world.home().join("bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("gh"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
case "$1 $2" in
    'auth status') exit 0 ;;
    'api --hostname') printf '%s\n' 'fixture-account' ;;
    'repo create')
        test "$3" = 'fixture-account/dotfiles' && test "$4" = '--private' || exit 42
        if [ "${TEST_ADVANCE_DEV:-}" = 1 ]; then
            "$REAL_GIT" commit --allow-empty -m 'Move dev after the seed was verified' >/dev/null
        fi
        ;;
    *) exit 42 ;;
esac
"#,
    );
    let real_git = env::split_paths(&env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file() && path.is_absolute())
        .unwrap();
    write_executable(
        &bin.join("git"),
        r#"#!/bin/sh
if [ "$1" = push ]; then
    printf '%s\n' "$*" >> "$HOME/push-calls"
    exit 1
fi
exec "$REAL_GIT" "$@"
"#,
    );
    let path = env::join_paths(
        std::iter::once(bin).chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();
    cmd.env("PATH", path).env("REAL_GIT", real_git);
}

fn write_executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn new_noninteractive_init_seeds_both_branches_locally_and_never_calls_gh() {
    let world = World::new();
    let root = fresh(&world);
    fs::remove_file(world.home().join(".gitconfig")).unwrap();
    let mut command = fixture_dots(&world);
    enable_fake_gh(&world, &mut command);
    let output = success(command.arg("init").arg(&root).output().unwrap());
    let id = git(&world, &root, &["rev-parse", "HEAD"]);
    assert_eq!(git(&world, &root, &["branch", "--show-current"]), "dev");
    assert_eq!(git(&world, &root, &["rev-parse", "main"]), id);
    assert_eq!(
        git(&world, &root, &["rev-list", "--max-parents=0", "HEAD"]),
        id
    );
    assert_eq!(
        git(&world, &root, &["show", "-s", "--format=%an <%ae>"]),
        "dots <dots@localhost>"
    );
    assert!(git(&world, &root, &["status", "--porcelain"]).is_empty());
    assert!(git(&world, &root, &["remote"]).is_empty());
    assert!(seed_push(&output, &id), "{output}");
    assert!(!world.home().join("gh-calls").exists());
    assert!(!world.home().join(".gitconfig").exists());

    let saved = ["miserc.toml", "config.local.toml"].map(|name| {
        let path = root.join(name);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        (
            fs::read(&path).unwrap(),
            fs::metadata(path).unwrap().modified().unwrap(),
        )
    });
    let later = success(init(&world, &root));
    assert!(seed_push(&later, &id), "{later}");
    for (name, (bytes, modified)) in ["miserc.toml", "config.local.toml"].into_iter().zip(saved) {
        let path = root.join(name);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
    }
}

#[test]
fn new_seed_excludes_ignored_modified_linked_and_executable_files() {
    let world = World::new();
    let root = fresh(&world);
    fs::create_dir_all(root.join("dots")).unwrap();
    let custom = "[vars]\ndots_profiles = \"server default\"\n";
    fs::write(root.join("config.toml"), custom).unwrap();
    fs::write(root.join("untracked.txt"), "synthetic private fixture\n").unwrap();
    let executable = root.join("dots/retired-dotfiles");
    fs::copy("init/retired-dotfiles", &executable).unwrap();
    fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(root.join("untracked.txt"), root.join("dots/README.md")).unwrap();
    let ignore = world.home().join("ignore");
    fs::write(&ignore, "/.gitleaks.toml\n").unwrap();
    success(
        world
            .git()
            .args(["config", "--global", "core.excludesFile"])
            .arg(&ignore)
            .output()
            .unwrap(),
    );

    let output = success(init(&world, &root));
    assert_eq!(
        git(&world, &root, &["ls-tree", "-r", "--name-only", "HEAD"]),
        ".gitignore\ndots/exceptions.tsv\ndots/retired-declarations"
    );
    assert_eq!(
        fs::read_to_string(root.join("config.toml")).unwrap(),
        custom
    );
    assert!(
        fs::read_to_string(root.join("miserc.toml"))
            .unwrap()
            .contains("\"default\"")
    );
    let seed = git(&world, &root, &["rev-parse", "HEAD"]);
    assert!(seed_push(&output, &seed), "{output}");
}

#[test]
fn new_existing_nonempty_index_is_untouched() {
    let world = World::new();
    let root = world.root();
    git(&world, root, &["init", "-b", "main"]);
    git(&world, root, &["add", "config.toml"]);
    let staged = fs::read(root.join(".git/index")).unwrap();

    let output = success(init(&world, root));
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), staged);
    assert_eq!(git(&world, root, &["branch", "--show-current"]), "main");
    assert!(git(&world, root, &["rev-list", "--all"]).is_empty());
    assert!(output.contains("HEAD has no commit"), "{output}");
    assert!(output.contains(" dots sync"), "{output}");
    assert!(!output.contains(" push "), "{output}");
}

#[test]
fn new_nested_directory_is_rejected_before_creation() {
    let world = World::new();
    git(&world, world.root(), &["init", "-b", "main"]);
    let root = world.root().join("nested/config with spaces");
    let out = init(&world, &root);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(!world.root().join("nested").exists());
}

#[test]
fn new_ambiguity_keeps_scaffold_and_prints_rooted_repair_commands() {
    let world = World::new();
    let root = world.root().with_file_name("dotfiles with 'quotes'");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("config.toml"),
        "[vars]\ndots_profiles = \"laptop server\"\n",
    )
    .unwrap();
    fs::write(
        root.join("miserc.toml"),
        "env = ['laptop', 'server', 'host-work']\n",
    )
    .unwrap();

    let failed = init(&world, &root);
    assert_eq!(failed.status.code(), Some(2), "{failed:?}");
    let text = String::from_utf8(failed.stdout).unwrap();
    let escaped = format!("'{}'", root.display().to_string().replace('\'', "'\\''"));
    assert!(
        text.contains(&format!(
            "DOTS_ROOT={escaped} dots profile --profile PROFILE --machine-id ID"
        )),
        "{text}"
    );
    assert!(text.contains(&format!("dots init {escaped}")), "{text}");
    assert!(!text.contains("dots up"));
    assert!(root.join("dots/README.md").exists());
    assert!(!root.join(".git").exists());

    success(
        fixture_dots(&world)
            .env("DOTS_ROOT", &root)
            .args(["profile", "--profile", "server", "--machine-id", "work"])
            .output()
            .unwrap(),
    );
    success(init(&world, &root));
    assert_eq!(git(&world, &root, &["branch", "--show-current"]), "dev");
    assert!(
        fs::read_to_string(root.join("miserc.toml"))
            .unwrap()
            .contains("server")
    );
}

#[test]
fn new_older_committed_template_requires_sync_even_with_current_working_bytes() {
    let world = World::new();
    let root = fresh(&world);
    success(init(&world, &root));
    let path = root.join("dots/README.md");
    let current = fs::read(&path).unwrap();

    fs::write(&path, "An older template\n").unwrap();
    git(&world, &root, &["add", "dots/README.md"]);
    git(&world, &root, &["commit", "--amend", "--no-edit"]);
    git(&world, &root, &["branch", "-f", "main", "HEAD"]);
    fs::write(&path, current).unwrap();

    let later = success(init(&world, &root));
    assert!(later.contains(" remote add origin URL"), "{later}");
    assert!(
        later
            .lines()
            .any(|line| line.starts_with("DOTS_ROOT=") && line.ends_with(" dots sync")),
        "{later}"
    );
    assert!(!later.contains(" push "), "{later}");
}

#[test]
fn new_interactive_push_failure_prints_verified_seed_retry_only_once() {
    let world = World::new();
    let root = fresh(&world);
    let mut cmd = fixture_dots(&world);
    enable_fake_gh(&world, &mut cmd);
    cmd.env("TEST_ADVANCE_DEV", "1");
    let output = success(terminal(cmd.arg("init").arg(&root), b"yes\n"));
    let seed = git(&world, &root, &["rev-parse", "main"]);
    assert_ne!(git(&world, &root, &["rev-parse", "dev"]), seed);
    assert!(
        output.contains("Create a private GitHub repository fixture-account/dotfiles"),
        "{output}"
    );
    assert!(
        output.contains("created private GitHub repository fixture-account/dotfiles"),
        "{output}"
    );
    assert!(output.contains("added remote origin"), "{output}");
    assert!(output.contains("Could not finish pushing"), "{output}");
    assert!(!output.contains(" remote add origin "), "{output}");
    assert!(seed_push(&output, &seed), "{output}");
    let push = fs::read_to_string(world.home().join("push-calls")).unwrap();
    assert!(
        push.trim_end().ends_with(&format!(
            "origin {seed}^{{commit}}:refs/heads/main {seed}^{{commit}}:refs/heads/dev"
        )),
        "{push}"
    );
    assert_eq!(
        git(&world, &root, &["remote", "get-url", "origin"]),
        "https://github.com/fixture-account/dotfiles.git"
    );

    let calls = fs::read(world.home().join("gh-calls")).unwrap();
    let mut cmd = fixture_dots(&world);
    enable_fake_gh(&world, &mut cmd);
    let rerun = success(terminal(cmd.arg("init").arg(&root), b"yes\n"));
    assert!(rerun.contains("kept remote origin"), "{rerun}");
    assert!(!rerun.contains("Create a private"), "{rerun}");
    assert!(!rerun.contains(" push "), "{rerun}");
    assert_eq!(fs::read(world.home().join("gh-calls")).unwrap(), calls);
    assert_eq!(
        fs::read_to_string(world.home().join("push-calls")).unwrap(),
        push
    );
}

#[test]
fn new_interactive_enter_declines_and_rerun_does_not_offer() {
    let world = World::new();
    let root = fresh(&world);
    let mut cmd = fixture_dots(&world);
    enable_fake_gh(&world, &mut cmd);
    let first = success(terminal(cmd.arg("init").arg(&root), b"\n"));
    let seed = git(&world, &root, &["rev-parse", "HEAD"]);
    assert!(first.contains("[y/N]"), "{first}");
    assert!(seed_push(&first, &seed), "{first}");
    let calls = fs::read_to_string(world.home().join("gh-calls")).unwrap();
    assert!(!calls.contains("repo create"));
    drop(cmd);

    let mut cmd = fixture_dots(&world);
    enable_fake_gh(&world, &mut cmd);
    let later = success(terminal(cmd.arg("init").arg(&root), b"yes\n"));
    assert!(!later.contains("[y/N]"), "{later}");
    assert!(seed_push(&later, &seed), "{later}");
    assert_eq!(
        fs::read_to_string(world.home().join("gh-calls")).unwrap(),
        calls
    );
}

fn terminal(cmd: &mut Command, reply: &[u8]) -> Output {
    use nix::libc::{EIO, openpty};
    let mut master = -1;
    let mut slave = -1;
    // openpty initializes the two descriptor slots. Default termios is enough
    // for this test because no child needs to open a controlling terminal.
    assert_eq!(
        unsafe {
            openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // On success both descriptors are newly owned by this test.
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    let child = cmd
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    master.write_all(reply).unwrap();
    let mut stdout = Vec::new();
    if let Err(err) = master.read_to_end(&mut stdout) {
        assert_eq!(err.raw_os_error(), Some(EIO), "{err}");
    }
    let status = child.wait_with_output().unwrap().status;
    let stdout = String::from_utf8(stdout)
        .unwrap()
        .replace("\r\n", "\n")
        .into_bytes();
    Output {
        status,
        stdout,
        stderr: Vec::new(),
    }
}

#[test]
fn new_empty_template_subset_still_creates_a_pristine_seed() {
    let world = World::new();
    let root = fresh(&world);
    let ignore = world.home().join("ignore");
    fs::write(&ignore, "*\n").unwrap();
    success(
        world
            .git()
            .args(["config", "--global", "core.excludesFile"])
            .arg(&ignore)
            .output()
            .unwrap(),
    );
    let output = success(init(&world, &root));
    assert!(git(&world, &root, &["ls-tree", "-r", "HEAD"]).is_empty());
    assert!(seed_push(
        &output,
        &git(&world, &root, &["rev-parse", "HEAD"])
    ));
}

#[test]
fn new_clean_main_gets_the_branch_fix_without_moving_head() {
    let world = World::new();
    let root = fresh(&world);
    success(init(&world, &root));
    git(&world, &root, &["switch", "main"]);
    git(&world, &root, &["branch", "-D", "dev"]);
    let head = git(&world, &root, &["rev-parse", "HEAD"]);
    let output = success(init(&world, &root));
    assert!(output.contains(" switch -c dev"), "{output}");
    assert!(!output.contains(" push "), "{output}");
    assert_eq!(git(&world, &root, &["branch", "--show-current"]), "main");
    assert_eq!(git(&world, &root, &["rev-parse", "HEAD"]), head);

    git(
        &world,
        &root,
        &["config", "status.showUntrackedFiles", "no"],
    );
    fs::write(root.join("fixture-edit"), "uncommitted\n").unwrap();
    let output = success(init(&world, &root));
    assert!(!output.contains(" switch -c dev"), "{output}");
}

#[test]
fn new_invalid_lone_selection_uses_defaults_and_keeps_other_selectors() {
    let world = World::new();
    let root = fresh(&world);
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("config.toml"),
        "[vars]\ndots_profiles = \"laptop server\"\n",
    )
    .unwrap();
    fs::write(
        root.join("config.local.toml"),
        "[env]\nSETUP_PROFILE = \"removed\"\nSETUP_MACHINE_ID = 17\n",
    )
    .unwrap();
    fs::write(root.join("miserc.toml"), "env = ['extra']\n").unwrap();

    success(
        fixture_dots(&world)
            .env("SETUP_PROFILE", "server")
            .env("SETUP_MACHINE_ID", "caller-id")
            .arg("init")
            .arg(&root)
            .output()
            .unwrap(),
    );
    let selector = fs::read_to_string(root.join("miserc.toml")).unwrap();
    assert!(
        selector.starts_with("env = [\"laptop\", \"host-"),
        "{selector}"
    );
    assert!(selector.contains("\"extra\""), "{selector}");
    assert!(!selector.contains("caller-id"));
}

#[test]
fn new_printed_seed_push_uses_the_commit_object_and_suppresses_tags() {
    let world = World::new();
    let root = fresh(&world);
    success(init(&world, &root));
    let seed = git(&world, &root, &["rev-parse", "HEAD"]);
    git(&world, &root, &["config", "push.followTags", "true"]);
    git(
        &world,
        &root,
        &["tag", "-a", "fixture-tag", "-m", "Synthetic tag annotation"],
    );
    let output = success(init(&world, &root));
    assert!(seed_push(&output, &seed), "{output}");
    let push = output
        .lines()
        .find(|line| line.starts_with("git -C ") && line.contains(" push "))
        .unwrap();
    let other = git(
        &world,
        &root,
        &["commit-tree", "HEAD^{tree}", "-m", "Other commit"],
    );

    for kind in ["heads", "tags"] {
        let collision = format!("refs/{kind}/{seed}");
        git(&world, &root, &["update-ref", &collision, &other]);
        let remote = world.bare_remote(kind);
        success(
            world
                .git()
                .current_dir(&root)
                .args(["remote", "add", "origin"])
                .arg(&remote)
                .output()
                .unwrap(),
        );
        let environment = world.git();
        success(
            Command::new("/bin/sh")
                .env_clear()
                .envs(
                    environment
                        .get_envs()
                        .filter_map(|(key, value)| Some((key, value?))),
                )
                .args(["-c", push])
                .output()
                .unwrap(),
        );
        for branch in ["main", "dev"] {
            assert_eq!(git(&world, &remote, &["rev-parse", branch]), seed);
        }
        assert!(git(&world, &remote, &["tag", "--list"]).is_empty());
        git(&world, &root, &["remote", "remove", "origin"]);
        git(&world, &root, &["update-ref", "-d", &collision]);
    }
}

#[test]
fn new_seed_retries_after_a_stale_index_lock_without_moving_refs() {
    let world = World::new();
    let root = fresh(&world);
    fs::create_dir(&root).unwrap();
    git(&world, &root, &["init", "-b", "main"]);
    let lock = root.join(".git/index.lock");
    let marker = b"fixture index lock";
    fs::write(&lock, marker).unwrap();

    let refused = init(&world, &root);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Git index is locked"));
    assert_eq!(fs::read(&lock).unwrap(), marker);
    assert_eq!(git(&world, &root, &["branch", "--show-current"]), "main");
    assert!(git(&world, &root, &["for-each-ref", "refs/heads"]).is_empty());
    assert!(!root.join(".git/index").exists());

    fs::remove_file(&lock).unwrap();
    let output = success(init(&world, &root));
    let seed = git(&world, &root, &["rev-parse", "HEAD"]);
    assert!(seed_push(&output, &seed), "{output}");
    assert_eq!(git(&world, &root, &["branch", "--show-current"]), "dev");
    assert!(git(&world, &root, &["status", "--porcelain"]).is_empty());
}

#[test]
fn new_explicit_directory_works_with_an_impossible_ambient_root() {
    let world = World::new();
    let blocker = world.home().join("ordinary-file");
    fs::write(&blocker, b"keep this file").unwrap();
    let root = fresh(&world);
    let output = success(
        fixture_dots(&world)
            .current_dir(root.parent().unwrap())
            .env("DOTS_ROOT", blocker.join("cannot-exist"))
            .arg("init")
            .arg(root.file_name().unwrap())
            .output()
            .unwrap(),
    );
    assert!(
        output.contains("outside the active configuration root"),
        "{output}"
    );
    assert_eq!(git(&world, &root, &["branch", "--show-current"]), "dev");
    assert_eq!(fs::read(blocker).unwrap(), b"keep this file");
    assert!(!world.root().join(".git").exists());
}

#[test]
fn new_template_only_commit_with_private_message_requires_sync() {
    const MEMO: &str = "Synthetic private memo for this fixture";
    let world = World::new();
    let root = fresh(&world);
    success(init(&world, &root));
    assert_eq!(
        git(&world, &root, &["show", "-s", "--format=%an <%ae>", "HEAD"]),
        "Dots Tests <dots-tests@example.invalid>"
    );
    let original_tree = git(&world, &root, &["rev-parse", "HEAD^{tree}"]);
    let fixture = world.home().join("amended-message");
    for message in [
        format!("Initialize dots configuration\n\n{MEMO}\n"),
        "Initialize dots configuration\n\n".to_owned(),
    ] {
        fs::write(&fixture, &message).unwrap();
        success(
            world
                .git()
                .current_dir(&root)
                .args(["commit", "--amend", "--cleanup=verbatim", "-F"])
                .arg(&fixture)
                .output()
                .unwrap(),
        );
        git(&world, &root, &["branch", "-f", "main", "HEAD"]);
        let amended = git(&world, &root, &["rev-parse", "HEAD"]);
        assert_eq!(
            git(&world, &root, &["rev-parse", "HEAD^{tree}"]),
            original_tree
        );
        assert_eq!(
            git(&world, &root, &["rev-list", "--parents", "-1", "HEAD"]),
            amended
        );
        let object = world
            .git()
            .current_dir(&root)
            .args(["cat-file", "commit", "HEAD"])
            .output()
            .unwrap();
        assert!(object.status.success());
        assert!(object.stdout.ends_with(message.as_bytes()));

        let output = init(&world, &root);
        let stderr = String::from_utf8(output.stderr.clone()).unwrap();
        let stdout = success(output);
        assert!(stdout.contains(" remote add origin URL"), "{stdout}");
        assert!(
            stdout
                .lines()
                .any(|line| line.starts_with("DOTS_ROOT=") && line.ends_with(" dots sync")),
            "{stdout}"
        );
        assert!(!stdout.contains(" push ") && !stderr.contains(" push "));
        assert!(!stdout.contains(MEMO) && !stderr.contains(MEMO));
    }
}
