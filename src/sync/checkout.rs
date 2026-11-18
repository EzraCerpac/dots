use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::time::Duration;

use nix::libc;
use serde::{Deserialize, Serialize};

use super::git::{Git, Tree, invalid, oid};
use super::guard::{CStrName, Dir, Image, private_dir, replace_exact, write_private};
use super::snapshot::changed;

const PENDING: &str = "dots-sync-pending";

pub(super) fn has_pending(directory: &Path) -> bool {
    !matches!(
        fs::symlink_metadata(directory.join(PENDING)),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

#[derive(Deserialize, Serialize)]
struct Plan {
    before: String,
    after: String,
    checkout: bool,
}

#[derive(Debug)]
pub(super) struct Blocked(pub PathBuf);

pub(super) struct Installed {
    pub before: String,
    pub after: String,
    pub paths: Vec<PathBuf>,
    pub index_after: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct Interrupted {
    pub path: Option<PathBuf>,
    pub backups: Vec<PathBuf>,
}

fn fingerprints_match(path: &Path, other: &Path) -> io::Result<bool> {
    let first = fs::symlink_metadata(path)?;
    let second = fs::symlink_metadata(other)?;
    Ok(first.is_file()
        && second.is_file()
        && first.dev() == second.dev()
        && first.ino() == second.ino())
}

pub(super) fn index_snapshot(dir: &Path) -> io::Result<Vec<u8>> {
    let index = dir.join("index");
    if !fs::symlink_metadata(&index)?.is_file() {
        return Err(invalid("checkout index must be a regular file"));
    }
    fs::read(index)
}

/// A ref-only update leaves the checkout's index at the preceding tree.
pub(super) fn previous_index_base(
    git: &Git<'_>,
    index: &[u8],
    temporary: &Path,
    head: &str,
) -> io::Result<Option<String>> {
    if !git
        .text(["reflog", "show", "-1", "--format=%gs", "refs/heads/dev"])?
        .is_empty()
    {
        return Ok(None);
    }
    let Ok(previous) = git.id(["rev-parse", "--verify", "refs/heads/dev@{1}^{commit}"]) else {
        return Ok(None);
    };
    if previous == head {
        return Ok(None);
    }
    fs::write(temporary, index)?;
    let tree = git.indexed(temporary).id(["write-tree"])?;
    let previous_tree = git.id(["rev-parse", &format!("{previous}^{{tree}}")])?;
    let head_tree = git.id(["rev-parse", &format!("{head}^{{tree}}")])?;
    Ok((previous_tree == tree && head_tree != tree).then_some(previous))
}

/// A single checkout cannot retain two independent staged and unstaged
/// versions of a path in one commit without choosing one of them.
pub(super) fn preserves_staged(
    git: &Git<'_>,
    temporary: &Path,
    index: &[u8],
    base: &str,
    candidate: &str,
) -> io::Result<bool> {
    fs::write(temporary, index)?;
    let staged = git.indexed(temporary).id(["write-tree"])?;
    let before = git.tree(base)?;
    let staged = git.tree(&staged)?;
    let candidate = git.tree(candidate)?;
    Ok(changed(&before, &staged)
        .iter()
        .all(|path| staged.get(path) == candidate.get(path)))
}

fn directory_in(tree: &Tree, path: &Path) -> bool {
    tree.keys()
        .any(|leaf| leaf != path && leaf.starts_with(path))
}

fn has_leaf_parent(tree: &Tree, path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .any(|parent| tree.contains_key(parent))
}

fn removing_directory_is_safe(root: &Path, path: &Path, before: &Tree) -> io::Result<bool> {
    for child in fs::read_dir(root.join(path))? {
        let child = child?;
        let relative = path.join(child.file_name());
        let kind = child.file_type()?;
        if kind.is_dir() {
            if !directory_in(before, &relative)
                || !removing_directory_is_safe(root, &relative, before)?
            {
                return Ok(false);
            }
        } else if !before.contains_key(&relative) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn leaf_before(git: &Git<'_>, revision: &str, before: &Tree, path: &Path) -> io::Result<Image> {
    if directory_in(before, path) {
        Ok(Image::Directory)
    } else {
        Image::checkout(git, revision, path, before.get(path))
    }
}

pub(super) fn preflight(
    git: &Git<'_>,
    revision: &str,
    before: &Tree,
    after: &Tree,
) -> io::Result<Vec<Blocked>> {
    let anchored = Dir::open(git.root())?;
    let mut blocked = Vec::new();
    for path in changed(before, after) {
        let expected = leaf_before(git, revision, before, &path)?;
        // A tracked parent due for deletion currently hides this path.
        if has_leaf_parent(before, &path) {
            continue;
        }
        let safe = match anchored.read_path(&path) {
            Ok(actual) if actual.git_eq(&expected) => {
                actual != Image::Directory || removing_directory_is_safe(git.root(), &path, before)?
            }
            Ok(_) if expected == Image::Absent => {
                let folded = |a: &Path, b: &Path| {
                    use std::os::unix::ffi::OsStrExt;
                    a.as_os_str()
                        .as_bytes()
                        .eq_ignore_ascii_case(b.as_os_str().as_bytes())
                };
                before
                    .keys()
                    .filter(|old| !after.contains_key(*old) && folded(old, &path))
                    .any(|old| {
                        fingerprints_match(&git.root().join(old), &git.root().join(&path))
                            .unwrap_or(false)
                    })
            }
            _ => false,
        };
        if !safe {
            blocked.push(Blocked(path));
        }
    }
    Ok(blocked)
}

pub(super) fn begin(
    git: &Git<'_>,
    git_dir: &Path,
    original_index: &[u8],
    before: &str,
    after: &str,
    checkout: bool,
) -> io::Result<()> {
    let pending = git_dir.join(PENDING);
    if fs::symlink_metadata(&pending).is_ok() {
        return Err(invalid("another checkout recovery is pending"));
    }
    let prepared = private_dir(git_dir, "dots-sync-prepare-")?;
    let path = prepared.path();
    let plan = Plan {
        before: before.into(),
        after: after.into(),
        checkout,
    };
    write_synced(path.join("plan.json"), &serde_json::to_vec(&plan)?)?;
    write_synced(path.join("index.before"), original_index)?;
    git.indexed(&path.join("index.after")).get([
        "-c",
        "core.splitIndex=false",
        "read-tree",
        after,
    ])?;
    let permissions = fs::Permissions::from_mode(
        fs::metadata(git_dir.join("index"))?.permissions().mode() & 0o777,
    );
    for name in ["index.after", "index.before"] {
        let file = File::open(path.join(name))?;
        file.set_permissions(permissions.clone())?;
        file.sync_all()?;
    }

    if checkout {
        let from = git.tree(before)?;
        let to = git.tree(after)?;
        let live = Dir::open(git.root())?;
        for (position, item) in mutations(&from, &to).iter().enumerate() {
            if from.contains_key(item) {
                let expected = leaf_before(git, before, &from, item)?;
                let observed = live.read_path(item)?;
                if !observed.git_eq(&expected) {
                    return Err(invalid(
                        "checkout path changed while preparing its replacement",
                    ));
                }
                write_private(&path.join(format!("expected-{position}")), &observed)?;
            }
        }
    }
    File::open(path)?.sync_all()?;
    fs::rename(path, &pending)?;
    File::open(git_dir)?.sync_all()
}

fn write_synced(path: PathBuf, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn mutations(before: &Tree, after: &Tree) -> Vec<PathBuf> {
    let mut leaves = changed(before, after);
    leaves.sort_by_key(|path| (after.contains_key(path), path.clone()));
    leaves
}

fn prepare_leaf(
    git: &Git<'_>,
    pending: &Path,
    revision: &str,
    path: &Path,
    before: &Image,
    name: &Path,
) -> io::Result<Image> {
    // Let Git perform checkout filters in a private directory. Publishing
    // their result still requires the journal's guarded compare and move.
    let work = private_dir(pending, "filter-")?;
    let mut prefix = OsString::from("--prefix=");
    prefix.push(work.path());
    prefix.push("/");
    let attributes = format!("--attr-source={revision}");
    git.indexed(&pending.join("index.after")).get([
        OsStr::new("--literal-pathspecs"),
        attributes.as_ref(),
        "checkout-index".as_ref(),
        &prefix,
        "--".as_ref(),
        path.as_os_str(),
    ])?;
    let mut image = Dir::open(work.path())?.read_path(path)?;
    if let (Image::Regular { mode, .. }, Image::Regular { mode: old, .. }) = (&mut image, before) {
        *mode = (old & !0o111) | (*mode & 0o111);
    }
    if !matches!(image, Image::Regular { .. } | Image::Link(_)) {
        return Err(invalid("Git did not produce a checkout leaf"));
    }
    let staged = work.path().join("prepared");
    write_private(&staged, &image)?;
    fs::rename(staged, pending.join(name))?;
    File::open(pending)?.sync_all()?;
    Ok(image)
}

fn index_lock(dir: &Path, pending: &Path) -> io::Result<()> {
    let prepared = pending.join("index.after");
    let lock = dir.join("index.lock");
    match fs::hard_link(&prepared, &lock) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fingerprints_match(&lock, &prepared)? {
                return Err(invalid("Git index is locked by another writer"));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn release_index(dir: &Path, pending: &Path) -> io::Result<()> {
    let path = dir.join("index.lock");
    match fingerprints_match(&path, &pending.join("index.after")) {
        Ok(true) => fs::remove_file(path),
        Ok(false) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn replace_index(staged: &Path, directory: &Path) -> io::Result<()> {
    let next = staged.with_extension("install");
    match fs::hard_link(staged, &next) {
        Ok(()) => (),
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && fingerprints_match(&next, staged)? => {}
        Err(error) => return Err(error),
    }
    fs::rename(next, directory.join("index"))
}

/// Git locks both the symbolic HEAD and its referent during a prepared
/// transaction. EOF releases both without updating them if sync stops.
struct RefTransaction {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
}

impl RefTransaction {
    fn prepare(
        git: &Git<'_>,
        head: &str,
        desired: &str,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Self> {
        let mut child = git
            .command(["update-ref", "-m", "dots sync", "--stdin"])?
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let input = child.stdin.take();
        let output = child.stdout.take().expect("requested Git transaction pipe");
        let mut transaction = Self {
            child,
            input,
            output,
        };
        transaction.request("start\n", b"start: ok\n", cancelled)?;
        let update = if head == desired {
            format!("verify HEAD {head}^{{commit}}\nprepare\n")
        } else {
            format!("update HEAD {desired}^{{commit}} {head}^{{commit}}\nprepare\n")
        };
        transaction.request(&update, b"prepare: ok\n", cancelled)?;
        if git.text(["symbolic-ref", "HEAD"])? != "refs/heads/dev" {
            return Err(invalid("the checked out branch changed"));
        }
        Ok(transaction)
    }

    fn request(
        &mut self,
        request: &str,
        response: &[u8],
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<()> {
        self.input
            .as_mut()
            .expect("Git transaction input remains open")
            .write_all(request.as_bytes())?;
        for expected in response {
            loop {
                if cancelled() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let mut poll = libc::pollfd {
                    fd: self.output.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll describes one live pipe for this call.
                match unsafe { libc::poll(&mut poll, 1, 100) } {
                    0 => continue,
                    -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {
                        continue;
                    }
                    -1 => return Err(io::Error::last_os_error()),
                    _ => break,
                }
            }
            let mut byte = [0];
            self.output.read_exact(&mut byte)?;
            if byte[0] != *expected {
                return Err(invalid("unexpected Git transaction response"));
            }
        }
        Ok(())
    }
}

impl Drop for RefTransaction {
    fn drop(&mut self) {
        drop(self.input.take());
        let wait = |child: &mut Child| {
            for _ in 0..20 {
                if !matches!(child.try_wait(), Ok(None)) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        if wait(&mut self.child) {
            return;
        }
        // Only this invocation's process group is targeted. Give Git time
        // to clean its own ref locks before bounding a stuck hook.
        let group = -(self.child.id() as libc::pid_t);
        // SAFETY: group is the private group created for the child.
        unsafe { libc::kill(group, libc::SIGTERM) };
        if !wait(&mut self.child) {
            // SAFETY: the child is still alive in the same private group.
            unsafe { libc::kill(group, libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

fn apply(
    git: &Git<'_>,
    directory: &Path,
    pending: &Path,
    on_path: &mut Option<PathBuf>,
    cancelled: impl Fn() -> bool,
    write_index: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<Installed> {
    let plan: Plan = serde_json::from_reader(File::open(pending.join("plan.json"))?)?;
    oid(plan.before.as_bytes())?;
    oid(plan.after.as_bytes())?;
    let shape = super::shape(git).map_err(io::Error::other)?;
    if super::shape_problem(&shape).is_some() {
        return Err(invalid("checkout shape changed"));
    }
    let head = git.id(["rev-parse", "--verify", "HEAD^{commit}"])?;
    if head != plan.before && head != plan.after {
        if !pending.join("started").exists() && !pending.join("finished").exists() {
            release_index(directory, pending)?;
            fs::remove_dir_all(pending)?;
            File::open(directory)?.sync_all()?;
        }
        return Err(invalid("branch changed while checkout was pending"));
    }

    let staged = pending.join("index.after");
    let installed = || {
        Ok(Installed {
            paths: changed(&git.tree(&plan.before)?, &git.tree(&plan.after)?),
            before: plan.before.clone(),
            after: plan.after.clone(),
            index_after: fs::read(&staged)?,
        })
    };
    if pending.join("finished").exists() {
        if head != plan.after {
            return Err(invalid("branch no longer matches the finished checkout"));
        }
        release_index(directory, pending)?;
        return installed();
    }

    index_lock(directory, pending)?;
    let index_contains_after = fingerprints_match(&directory.join("index"), &staged)?;
    if !index_contains_after
        && index_snapshot(directory)? != fs::read(pending.join("index.before"))?
    {
        release_index(directory, pending)?;
        if !pending.join("started").exists() {
            fs::remove_dir_all(pending)?;
        }
        return Err(invalid("Git index changed while sync was running"));
    }
    let mut reference = match RefTransaction::prepare(git, &head, &plan.after, &cancelled) {
        Ok(reference) => reference,
        Err(error) => {
            if !pending.join("started").exists() {
                release_index(directory, pending)?;
            }
            return Err(error);
        }
    };
    if !pending.join("started").exists() {
        write_synced(pending.join("started"), b"verified index\n")?;
        File::open(pending)?.sync_all()?;
    }
    let old = git.tree(&plan.before)?;
    let new = git.tree(&plan.after)?;
    let mut paths = Vec::new();
    if plan.checkout && !index_contains_after {
        let root = Dir::open(git.root())?;
        let stage = Dir::open(pending)?;
        for (position, item) in mutations(&old, &new).into_iter().enumerate() {
            let marker = pending.join(format!("done-{position}"));
            if marker.is_file() {
                paths.push(item);
                continue;
            }
            if cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            *on_path = Some(item.clone());
            let expected = if directory_in(&old, &item) {
                Image::Directory
            } else {
                stage.read_path(Path::new(&format!("expected-{position}")))?
            };
            let new_name = PathBuf::from(format!("new-{position}"));
            let mut replacement = stage.read_path(&new_name)?;
            if replacement == Image::Absent && new.contains_key(&item) {
                replacement = prepare_leaf(git, pending, &plan.after, &item, &expected, &new_name)?;
            }

            let old_name = format!("old-{position}");
            if expected == Image::Directory
                && replacement != Image::Absent
                && stage.read_path(Path::new(&old_name))? == Image::Absent
                && root.read_path(&item)? == Image::Directory
            {
                // A shared ancestor can become empty only after every
                // descendant is gone, regardless of lexical order.
                prune_empty_ancestors(&root, &old, &item)?;
            }
            replace_exact(
                &root,
                &item,
                &stage,
                &CStrName::new(&old_name)?,
                &CStrName::new(&format!("new-{position}"))?,
                &expected,
                &replacement,
            )?;
            write_synced(marker, b"applied\n")?;
            paths.push(item);
        }
    } else {
        paths = changed(&old, &new);
    }
    *on_path = None;

    // Keep Git excluded while installing the index and moving the ref.
    // The separate staged inode also identifies our lock on restart.
    if cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    if !index_contains_after {
        if let Err(error) = write_index(&staged, directory) {
            if !plan.checkout {
                release_index(directory, pending)?;
                fs::remove_dir_all(pending)?;
            }
            return Err(error);
        }
        File::open(directory)?.sync_all()?;
    }
    reference.request("commit\n", b"commit: ok\n", &cancelled)?;
    write_synced(pending.join("finished"), b"installed index and dev\n")?;
    File::open(pending)?.sync_all()?;
    release_index(directory, pending)?;
    File::open(directory)?.sync_all()?;
    Ok(Installed {
        before: plan.before,
        after: plan.after,
        paths,
        index_after: fs::read(pending.join("index.after"))?,
    })
}

fn prune_empty_ancestors(root: &Dir, old: &Tree, target: &Path) -> io::Result<()> {
    let unique: BTreeSet<_> = old
        .keys()
        .filter(|leaf| leaf.starts_with(target))
        .flat_map(|leaf| {
            leaf.ancestors()
                .skip(1)
                .take_while(|parent| *parent != target)
        })
        .collect();
    let mut parents: Vec<_> = unique.into_iter().collect();
    parents.sort_unstable_by_key(|path| Reverse(path.components().count()));
    for parent in parents {
        root.remove_dir_if_empty(parent)?;
    }
    Ok(())
}

fn backup_paths(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("old-"))
        .map(|entry| entry.path())
        .collect()
}

pub(super) fn finish(
    git: &Git<'_>,
    directory: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<Option<(Installed, Vec<PathBuf>)>, Interrupted> {
    let pending = directory.join(PENDING);
    match fs::symlink_metadata(&pending) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Ok(md) if md.is_dir() && md.permissions().mode() & 0o077 == 0 => (),
        _ => {
            return Err(Interrupted {
                path: None,
                backups: Vec::new(),
            });
        }
    };
    let mut at = None;

    let installed =
        apply(git, directory, &pending, &mut at, cancelled, replace_index).map_err(|_| {
            Interrupted {
                path: at,
                backups: backup_paths(&pending),
            }
        })?;
    let mut backups = backup_paths(&pending);
    if backups.is_empty() {
        fs::remove_dir_all(&pending).map_err(|_| Interrupted {
            path: None,
            backups: Vec::new(),
        })?;
    } else {
        let recovery = private_dir(directory, "dots-sync-recovery-").map_err(|_| Interrupted {
            path: None,
            backups: backups.clone(),
        })?;
        fs::rename(&pending, recovery.path()).map_err(|_| Interrupted {
            path: None,
            backups: backups.clone(),
        })?;
        let recovery = recovery.keep();
        backups = backup_paths(&recovery);
    }
    File::open(directory)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| Interrupted {
            path: None,
            backups: backups.clone(),
        })?;
    Ok(Some((installed, backups)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_head_transaction_excludes_competing_git_writers() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let before = fixture_commit(&git, "baseline");
        let after = fixture_commit(&git, "desired");
        let other = fixture_commit(&git, "competing commit");
        git.get(["reset", "--hard", &before]).unwrap();
        let mut transaction = RefTransaction::prepare(&git, &before, &after, &|| false).unwrap();
        assert!(
            git.get(["update-ref", "refs/heads/dev", &other, &before])
                .is_err()
        );
        assert!(
            git.get(["symbolic-ref", "HEAD", "refs/heads/elsewhere"])
                .is_err()
        );
        transaction
            .request("commit\n", b"commit: ok\n", &|| false)
            .unwrap();
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), after);
    }

    #[test]
    fn competing_ref_before_checkout_does_not_leave_a_partial_install() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let live = git.root().join("edit");
        fs::write(&live, b"baseline").unwrap();
        git.get(["add", "edit"]).unwrap();
        let before = fixture_commit(&git, "baseline");
        fs::write(&live, b"incoming edit").unwrap();
        git.get(["add", "edit"]).unwrap();
        let after = fixture_commit(&git, "desired");
        let other = fixture_commit(&git, "competing commit");
        git.get(["reset", "--hard", &before]).unwrap();
        let dir = git.root().join(".git");
        let index = index_snapshot(&dir).unwrap();
        begin(&git, &dir, &index, &before, &after, true).unwrap();
        git.get(["update-ref", "refs/heads/dev", &other, &before])
            .unwrap();
        assert!(finish(&git, &dir, || false).is_err());
        assert!(!has_pending(&dir));
        assert_eq!(fs::read(live).unwrap(), b"baseline");
        assert_eq!(index_snapshot(&dir).unwrap(), index);
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), other);
    }

    #[test]
    fn nested_directory_replacement_resumes_final_journal_cleanup() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let live = git.root().join("replace");
        fs::create_dir_all(live.join("middle/deep")).unwrap();
        fs::write(live.join("middle/deep/leaf"), b"original").unwrap();
        git.get(["add", "replace"]).unwrap();
        let before = fixture_commit(&git, "nested directory");
        fs::remove_dir_all(&live).unwrap();
        fs::write(&live, b"incoming file").unwrap();
        git.get(["add", "replace"]).unwrap();
        let after = fixture_commit(&git, "file in place of directory");
        git.get(["reset", "--hard", &before]).unwrap();
        let dir = git.root().join(".git");
        let index = index_snapshot(&dir).unwrap();
        begin(&git, &dir, &index, &before, &after, true).unwrap();
        assert!(
            apply(
                &git,
                &dir,
                &dir.join(PENDING),
                &mut None,
                || false,
                replace_index
            )
            .is_ok()
        );
        assert!(dir.join(PENDING).join("finished").is_file());
        let (_, backups) = finish(&git, &dir, || false).unwrap().unwrap();
        assert!(
            backups
                .iter()
                .any(|path| fs::read(path).is_ok_and(|bytes| bytes == b"original"))
        );
        assert!(
            backups
                .iter()
                .all(|path| path.exists() && !path.starts_with(dir.join(PENDING)))
        );
        assert_eq!(fs::read(live).unwrap(), b"incoming file");
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), after);
    }

    #[test]
    fn recognizes_only_its_own_index_lock_on_resume() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let pending = private_dir(path, "pending").unwrap();
        let lock = path.join("index.lock");
        fs::write(path.join("index"), b"earlier index").unwrap();
        fs::write(pending.path().join("index.after"), b"new index").unwrap();
        fs::write(&lock, b"another writer").unwrap();

        assert!(index_lock(path, pending.path()).is_err());
        assert_eq!(fs::read(&lock).unwrap(), b"another writer");
        fs::remove_file(&lock).unwrap();
        index_lock(path, pending.path()).unwrap();
        index_lock(path, pending.path()).unwrap();
        replace_index(&pending.path().join("index.after"), path).unwrap();
        release_index(path, pending.path()).unwrap();
        assert!(!lock.exists());
        assert_eq!(fs::read(path.join("index")).unwrap(), b"new index");
    }

    #[test]
    fn candidate_index_enospc_retains_the_index_ref_and_worktree() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let live = git.root().join("edit");
        fs::write(&live, b"baseline").unwrap();
        git.get(["add", "edit"]).unwrap();
        let before = fixture_commit(&git, "baseline");
        fs::write(&live, b"local edit").unwrap();
        git.get(["add", "edit"]).unwrap();
        let after = fixture_commit(&git, "candidate");
        git.get(["reset", "--mixed", &before]).unwrap();

        let dir = git.root().join(".git");
        let original = index_snapshot(&dir).unwrap();
        begin(&git, &dir, &original, &before, &after, false).unwrap();
        let result = apply(
            &git,
            &dir,
            &dir.join(PENDING),
            &mut None,
            || false,
            |prepared, dir| {
                assert!(fingerprints_match(prepared, &dir.join("index.lock")).unwrap());
                Err(io::Error::from_raw_os_error(nix::libc::ENOSPC))
            },
        );
        assert_eq!(
            result.err().unwrap().raw_os_error(),
            Some(nix::libc::ENOSPC)
        );
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), before);
        assert_eq!(index_snapshot(&dir).unwrap(), original);
        assert_eq!(fs::read(live).unwrap(), b"local edit");
        assert!(!dir.join("index.lock").exists());
        assert!(!dir.join(PENDING).exists());
    }

    #[test]
    fn linked_case_rename_resumes_after_index_install_failure() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (fixture, ctx) = git_fixture();
        let primary = Git::new(&ctx, ctx.root.path());
        fs::write(primary.root().join("Capital"), b"old spelling").unwrap();
        primary.get(["add", "Capital"]).unwrap();
        let before = fixture_commit(&primary, "old spelling");
        primary.get(["mv", "Capital", "transit"]).unwrap();
        primary.get(["mv", "transit", "capital"]).unwrap();
        fs::write(primary.root().join("capital"), b"new spelling").unwrap();
        primary.get(["add", "capital"]).unwrap();
        let after = fixture_commit(&primary, "new spelling");

        primary
            .get(["switch", "-qc", "fixture-primary", &before])
            .unwrap();
        let primary_index = index_snapshot(&primary.root().join(".git")).unwrap();
        let linked = fixture.path().canonicalize().unwrap().join("linked");
        primary
            .get([
                "worktree",
                "add",
                "--quiet",
                linked.to_str().unwrap(),
                "dev",
            ])
            .unwrap();
        let git = primary.at(&linked);
        git.get(["reset", "--hard", &before]).unwrap();
        let (dir, _) = super::super::git::repository_dirs(&git).unwrap();
        let original = index_snapshot(&dir).unwrap();
        assert!(
            preflight(
                &git,
                &before,
                &git.tree(&before).unwrap(),
                &git.tree(&after).unwrap()
            )
            .unwrap()
            .is_empty()
        );
        begin(&git, &dir, &original, &before, &after, true).unwrap();

        let result = apply(
            &git,
            &dir,
            &dir.join(PENDING),
            &mut None,
            || false,
            |_, _| Err(io::Error::from_raw_os_error(nix::libc::ENOSPC)),
        );
        assert_eq!(
            result.err().unwrap().raw_os_error(),
            Some(nix::libc::ENOSPC)
        );
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), before);
        assert_eq!(index_snapshot(&dir).unwrap(), original);
        assert_eq!(fs::read(linked.join("capital")).unwrap(), b"new spelling");
        assert!(
            fs::read_dir(&linked)
                .unwrap()
                .all(|entry| entry.unwrap().file_name() != "Capital")
        );

        let (installed, _) = finish(&git, &dir, || false).unwrap().unwrap();
        assert_eq!(installed.after, after);
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), after);
        assert_eq!(git.text(["status", "--porcelain"]).unwrap(), "");
        assert_eq!(
            index_snapshot(&primary.root().join(".git")).unwrap(),
            primary_index
        );
        assert!(!dir.join("index.lock").exists());
    }

    #[test]
    fn pending_checkout_recovers_a_displaced_config_before_loading_it() {
        use crate::sync::{
            Loaded, load,
            tests::{fixture_commit, git_fixture},
        };
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let config = git.root().join("config.toml");
        let before = fixture_commit(&git, "baseline");
        let updated = "[vars]\ndots_profiles = 'default'\n# next\n";
        fs::write(&config, updated).unwrap();
        git.get(["add", "config.toml"]).unwrap();
        let after = fixture_commit(&git, "candidate");
        git.get(["reset", "--hard", &before]).unwrap();
        let dir = git.root().join(".git");
        begin(
            &git,
            &dir,
            &index_snapshot(&dir).unwrap(),
            &before,
            &after,
            true,
        )
        .unwrap();
        fs::rename(&config, dir.join(PENDING).join("old-0")).unwrap();

        assert!(matches!(load(&ctx), Loaded::Ready(_)));
        let (installed, _) = finish(&git, &dir, || false).unwrap().unwrap();
        assert_eq!(installed.after, after);
        assert_eq!(fs::read_to_string(config).unwrap(), updated);
        assert!(matches!(load(&ctx), Loaded::LocalOnly));
    }
}
