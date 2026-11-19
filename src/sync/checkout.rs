use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirEntry, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::libc;
use serde::{Deserialize, Serialize};

use super::git::{Git, Tree, invalid, oid};
use super::guard::{CStrName, Dir, Image, private_dir, replace_exact, write_private};
use super::snapshot::{changed, overlaps};

const PENDING: &str = "dots-sync-pending";
const SNAPSHOT_LOCK: &str = "dots-sync-snapshot.lock";

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
    directories: BTreeMap<PathBuf, PathBuf>,
    modes: BTreeMap<PathBuf, u32>,
}

#[derive(Debug)]
pub(super) struct Blocked(pub PathBuf);

pub(super) struct Installed {
    pub before: String,
    pub after: String,
    pub paths: Vec<PathBuf>,
    pub index_after: Vec<u8>,
}

#[derive(Debug, Default)]
pub(super) struct Interrupted {
    pub path: Option<PathBuf>,
    pub backups: Vec<PathBuf>,
    pub moved_dev: Option<(String, String)>,
    pub recorded_revisions: Option<Box<(String, String)>>,
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

/// Hold the real worktree index still while a separate index captures edits.
/// The private inode also identifies our Git lock if the process dies.
pub(super) struct SnapshotIndex {
    owner: File,
    lock: PathBuf,
    bytes: Vec<u8>,
}

impl SnapshotIndex {
    pub(super) fn acquire(git: &Git<'_>, directory: &Path) -> io::Result<Self> {
        let anchor = directory.join(SNAPSHOT_LOCK);
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&anchor)?;
        let md = owner.metadata()?;
        if !md.is_file()
            || md.uid() != nix::unistd::geteuid().as_raw()
            || md.mode() & 0o077 != 0
            || md.len() != 0
            || md.nlink() > 2
        {
            return Err(invalid("invalid snapshot lock owner"));
        }
        loop {
            match owner.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    return Err(invalid("Git index is locked by another snapshot"));
                }
                Err(TryLockError::Error(error)) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        let mut guard = Self {
            owner,
            lock: directory.join("index.lock"),
            bytes: Vec::new(),
        };
        // Own cleanup before any fallible work. Another thread may have
        // forked with a duplicate of the locked file description.
        File::open(directory)?.sync_all()?;
        link_index_lock(&anchor, &guard.lock)?;
        git.ensure_dense_index()?;
        guard.bytes = index_snapshot(directory)?;
        Ok(guard)
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for SnapshotIndex {
    fn drop(&mut self) {
        if let (Ok(owner), Ok(lock)) = (self.owner.metadata(), fs::symlink_metadata(&self.lock))
            && lock.is_file()
            && (owner.dev(), owner.ino()) == (lock.dev(), lock.ino())
        {
            let _ = fs::remove_file(&self.lock);
        }
        // A child between fork and exec can briefly inherit the descriptor.
        let _ = super::lock::unlock(&self.owner);
    }
}

/// Identify a ref-only move without treating reflog messages as Git actions.
/// The caller checks the previous tree against the live paths before moving
/// any of them. When the index could contain real staged changes, stop.
pub(super) fn previous_index_base(
    git: &Git<'_>,
    index: &[u8],
    temporary: &Path,
    head: &str,
) -> io::Result<Option<String>> {
    fs::write(temporary, index)?;
    let staged = git.indexed(temporary).id(["write-tree"])?;
    let current = git.tree(head)?;
    let index = git.tree(&staged)?;
    let staged_paths = changed(&current, &index);
    if staged_paths.is_empty() {
        return Ok(None);
    }
    if !git.output(["reflog", "exists", "HEAD"])?.status.success() {
        return Err(invalid(
            "could not establish a worktree index base without a HEAD reflog",
        ));
    }
    // HEAD@{0} can resolve to the live ref even when the logged OID
    // differs. Use the actual worktree log and inspect older entries too:
    // several ref-only moves can have occurred since the index was written.
    let log = git.text(["reflog", "show", "--format=%H", "HEAD"])?;
    if log.is_empty() {
        return Err(invalid(
            "could not establish a worktree index base from an empty HEAD reflog",
        ));
    }
    let mut seen = BTreeSet::from([head.to_owned()]);
    for (position, record) in log.lines().enumerate() {
        let previous = oid(record.as_bytes())?;
        if !seen.insert(previous.clone()) {
            continue;
        }
        let old = git.tree(&previous)?;
        if position == 0 && index == old {
            return Ok(Some(previous));
        }
        if staged_paths
            .iter()
            .any(|path| index.get(path) == old.get(path))
        {
            // A staged revert or deletion can look exactly like a stale
            // entry, even when unrelated staged edits make the trees differ.
            return Err(invalid(
                "the dev move and staged edits cannot be distinguished",
            ));
        }
    }
    Ok(None)
}

/// A path being committed must still include its staged version. A rejected
/// path is not committed, so its index and worktree can remain distinct.
pub(super) fn preserves_staged(
    git: &Git<'_>,
    temporary: &Path,
    index: &[u8],
    base: &str,
    candidate: &str,
    rejected: &BTreeSet<PathBuf>,
) -> io::Result<bool> {
    fs::write(temporary, index)?;
    let staged = git.indexed(temporary).id(["write-tree"])?;
    let before = git.tree(base)?;
    let staged = git.tree(&staged)?;
    let candidate = git.tree(candidate)?;
    Ok(changed(&before, &staged).iter().all(|path| {
        staged.get(path) == candidate.get(path)
            || (overlaps(rejected, path) && before.get(path) == candidate.get(path))
    }))
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
    let mut plan = Plan {
        before: before.into(),
        after: after.into(),
        checkout,
        directories: BTreeMap::new(),
        modes: BTreeMap::new(),
    };
    write_synced(path.join("index.before"), original_index)?;
    write_synced(path.join("index.after"), original_index)?;
    // Git's two-tree merge retains independently staged entries in paths
    // untouched by this revision change and refuses an overlapping update.
    git.indexed(&path.join("index.after")).get([
        "-c",
        "core.splitIndex=false",
        "read-tree",
        "-i",
        "-m",
        before,
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
        for item in &mutations(&from, &to) {
            if from.contains_key(item) {
                let expected = leaf_before(git, before, &from, item)?;
                let observed = live.read_path(item)?;
                if !observed.git_eq(&expected) {
                    return Err(invalid(
                        "checkout path changed while preparing its replacement",
                    ));
                }
                if let Image::Regular { mode, .. } = observed {
                    plan.modes.insert(item.clone(), mode);
                }
            } else if directory_in(&from, item) {
                for directory in
                    std::iter::once(item.as_path()).chain(nested_ancestors(&from, item))
                {
                    plan.directories
                        .insert(directory.to_owned(), live.directory_identity(directory)?);
                }
            }
        }
    }
    write_synced(path.join("plan.json"), &serde_json::to_vec(&plan)?)?;
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
    link_index_lock(&prepared, &lock)
}

fn link_index_lock(prepared: &Path, lock: &Path) -> io::Result<()> {
    match fs::hard_link(prepared, lock) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fingerprints_match(lock, prepared)? {
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

fn apply(
    git: &Git<'_>,
    directory: &Path,
    pending: &Path,
    progress: &mut Interrupted,
    cancelled: impl Fn() -> bool,
    write_index: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<Installed> {
    let plan: Plan = serde_json::from_reader(File::open(pending.join("plan.json"))?)?;
    progress.recorded_revisions = Some(Box::new((
        oid(plan.before.as_bytes())?,
        oid(plan.after.as_bytes())?,
    )));
    let shape = super::shape(git).map_err(io::Error::other)?;
    if super::shape_problem(&shape).is_some() {
        return Err(invalid("checkout shape changed"));
    }
    let head = git.id(["rev-parse", "--verify", "HEAD^{commit}"])?;
    if head != plan.before && head != plan.after {
        progress.moved_dev = Some((plan.before.clone(), plan.after.clone()));
        if !pending.join("started").exists() && !pending.join("finished").exists() {
            release_index(directory, pending)?;
            retire_pending(directory, pending, progress)?;
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
            progress.moved_dev = Some((plan.before.clone(), plan.after.clone()));
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
            retire_pending(directory, pending, progress)?;
        }
        return Err(invalid("Git index changed while sync was running"));
    }
    super::verify_dev(git, &head)?;
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
            progress.path = Some(item.clone());
            let mut expected = leaf_before(git, &plan.before, &old, &item)?;
            if let (Image::Regular { mode, .. }, Some(observed)) =
                (&mut expected, plan.modes.get(&item))
            {
                *mode = *observed;
            }
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
                prune_empty_ancestors(&root, &stage, &plan, &old, &item, position)?;
            }
            if expected == Image::Directory {
                verify_directory_slot(&root, &stage, &item, &old_name, &plan)?;
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
    progress.path = None;

    // Keep Git excluded while installing the index and moving the ref.
    // The separate staged inode also identifies our lock on restart.
    if cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    if !index_contains_after {
        if let Err(error) = write_index(&staged, directory) {
            if !plan.checkout {
                release_index(directory, pending)?;
                retire_pending(directory, pending, progress)?;
            }
            return Err(error);
        }
        File::open(directory)?.sync_all()?;
    }
    let update = (|| {
        super::verify_dev(git, &head)?;
        if head != plan.after {
            git.get([
                "update-ref",
                "-m",
                "dots sync",
                "refs/heads/dev",
                &plan.after,
                &head,
            ])?;
        }
        Ok::<(), io::Error>(())
    })();
    if let Err(error) = update {
        if !cancelled()
            && git
                .id(["rev-parse", "--verify", "refs/heads/dev^{commit}"])
                .is_ok_and(|dev| dev != plan.before && dev != plan.after)
        {
            progress.moved_dev = Some((plan.before.clone(), plan.after.clone()));
        }
        if !plan.checkout {
            replace_index(&pending.join("index.before"), directory)?;
            File::open(directory)?.sync_all()?;
            release_index(directory, pending)?;
            retire_pending(directory, pending, progress)?;
        }
        return Err(error);
    }
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

fn nested_ancestors<'a>(old: &'a Tree, target: &Path) -> Vec<&'a Path> {
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
    parents.sort_unstable_by_key(|path| (Reverse(path.components().count()), *path));
    parents
}

fn prune_empty_ancestors(
    root: &Dir,
    stage: &Dir,
    plan: &Plan,
    old: &Tree,
    target: &Path,
    position: usize,
) -> io::Result<()> {
    let unused = CStrName::new("no-directory-replacement")?;
    for (number, directory) in nested_ancestors(old, target).into_iter().enumerate() {
        let slot = format!("old-dir-{position}-{number}");
        verify_directory_slot(root, stage, directory, &slot, plan)?;
        if stage.read_path(Path::new(&slot))? == Image::Directory
            && root.read_path(directory)? == Image::Absent
        {
            continue;
        }
        replace_exact(
            root,
            directory,
            stage,
            &CStrName::new(&slot)?,
            &unused,
            &Image::Directory,
            &Image::Absent,
        )?;
    }
    Ok(())
}

fn verify_directory_slot(
    root: &Dir,
    stage: &Dir,
    live: &Path,
    slot: &str,
    plan: &Plan,
) -> io::Result<()> {
    let expected = plan
        .directories
        .get(live)
        .ok_or_else(|| invalid("missing directory identity"))?;
    let slot = Path::new(slot);
    let observed = if stage.read_path(slot)? == Image::Absent {
        root.directory_identity(live)?
    } else {
        stage.directory_identity(slot)?
    };
    if &observed != expected {
        return Err(invalid("another directory claimed the target"));
    }
    Ok(())
}

fn disposable(entry: &DirEntry) -> io::Result<bool> {
    let kind = entry.file_type()?;
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return Ok(false);
    };
    let numbered = |prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|number| number.parse::<usize>().is_ok())
    };
    Ok((kind.is_file()
        && (matches!(
            name,
            "plan.json" | "index.before" | "index.after" | "index.install" | "started" | "finished"
        ) || numbered("done-")))
        || ((kind.is_file() || kind.is_symlink()) && numbered("new-")))
}

fn recovery_paths(journal: &Path) -> Vec<PathBuf> {
    let list = || -> io::Result<Vec<PathBuf>> {
        let mut saved = Vec::new();
        for entry in fs::read_dir(journal)? {
            let entry = entry?;
            if !disposable(&entry)? {
                saved.push(entry.path());
            }
        }
        Ok(saved)
    };
    list().unwrap_or_else(|_| vec![journal.to_owned()])
}

fn clean_retired(journal: &Path) -> io::Result<()> {
    for entry in fs::read_dir(journal)? {
        let entry = entry?;
        if disposable(&entry)? {
            fs::remove_file(entry.path())?;
        }
    }
    // Exclusive leaf cleanup and rmdir preserve any entries added while
    // cleanup runs. A later sync never interprets a retired journal as active.
    fs::remove_dir(journal)
}

fn park_pending(
    directory: &Path,
    pending: &Path,
    progress: &mut Interrupted,
) -> io::Result<PathBuf> {
    let parent = File::open(directory)?;
    let recovery = private_dir(directory, "dots-sync-recovery-")?;
    // Drop automatic recursive cleanup before renaming any live journal.
    let recovery = recovery.keep();
    if let Err(error) = fs::rename(pending, &recovery) {
        let _ = fs::remove_dir(&recovery);
        return Err(error);
    }
    // Completed empty directory transitions do not contain user bytes.
    // rmdir refuses any directory into which an editor has added a file.
    if recovery.join("finished").is_file() {
        for entry in fs::read_dir(&recovery)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry.file_name().as_encoded_bytes().starts_with(b"old-")
            {
                let _ = fs::remove_dir(entry.path());
            }
        }
    }
    progress.backups = recovery_paths(&recovery);
    parent.sync_all()?;
    Ok(recovery)
}

fn retire_pending(directory: &Path, pending: &Path, progress: &mut Interrupted) -> io::Result<()> {
    let recovery = park_pending(directory, pending, progress)?;
    if progress.backups.is_empty() && clean_retired(&recovery).is_err() {
        progress.backups = vec![recovery];
    }
    Ok(())
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
            return Err(Interrupted::default());
        }
    };
    let mut progress = Interrupted::default();
    let installed = apply(
        git,
        directory,
        &pending,
        &mut progress,
        cancelled,
        replace_index,
    )
    .and_then(|installed| {
        retire_pending(directory, &pending, &mut progress)?;
        Ok(installed)
    });
    match installed {
        Ok(installed) => Ok(Some((installed, progress.backups))),
        Err(_) => {
            if has_pending(directory) {
                progress.backups = recovery_paths(&pending);
            }
            // Omit revisions when the interrupted operation never started
            // or its journal has since been retired.
            if !fs::symlink_metadata(pending.join("started")).is_ok_and(|marker| marker.is_file()) {
                progress.recorded_revisions = None;
            }
            Err(progress)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_ref_move_from_another_worktree_reconciles_the_old_index() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let before = fixture_commit(&git, "baseline");
        let other = fixture.path().canonicalize().unwrap().join("other");
        git.get([
            "worktree",
            "add",
            "--quiet",
            "--detach",
            other.to_str().unwrap(),
        ])
        .unwrap();
        let detached = git.at(&other);
        fs::write(other.join("added"), b"from moved dev").unwrap();
        detached.get(["add", "added"]).unwrap();
        let after = fixture_commit(&detached, "add file");
        detached
            .get([
                "update-ref",
                "-m",
                "adopt dev",
                "refs/heads/dev",
                &after,
                &before,
            ])
            .unwrap();
        assert!(!git.root().join("added").exists());
        let live_head = git.id(["rev-parse", "--verify", "HEAD"]).unwrap();
        let logged_head = git
            .id(["reflog", "show", "-1", "--format=%H", "HEAD"])
            .unwrap();
        assert_eq!(live_head, after);
        assert_eq!(logged_head, before);

        let dir = git.root().join(".git");
        let index = index_snapshot(&dir).unwrap();
        let probe = fixture.path().join("probe.index");
        assert_eq!(
            previous_index_base(&git, &index, &probe, &after).unwrap(),
            Some(before.clone())
        );
        assert!(
            preflight(
                &git,
                &before,
                &git.tree(&before).unwrap(),
                &git.tree(&after).unwrap(),
            )
            .unwrap()
            .is_empty()
        );
        begin(&git, &dir, &index, &before, &after, true).unwrap();
        let (_, backups) = finish(&git, &dir, || false).unwrap().unwrap();
        assert!(backups.is_empty());
        assert_eq!(
            fs::read(git.root().join("added")).unwrap(),
            b"from moved dev"
        );
        assert_eq!(git.text(["status", "--porcelain"]).unwrap(), "");
    }

    #[test]
    fn named_ref_move_with_staged_work_declines_a_partly_stale_index() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let before = fixture_commit(&git, "baseline");
        fs::write(git.root().join("added"), b"from moved dev").unwrap();
        git.get(["add", "added"]).unwrap();
        let after = fixture_commit(&git, "add file");
        git.get(["reset", "--hard", &before]).unwrap();
        git.get([
            "update-ref",
            "-m",
            "adopt dev",
            "refs/heads/dev",
            &after,
            &before,
        ])
        .unwrap();
        fs::write(git.root().join("staged"), b"independent staged edit").unwrap();
        git.get(["add", "staged"]).unwrap();
        fs::write(git.root().join("staged"), b"independent unstaged edit").unwrap();

        let dir = git.root().join(".git");
        let index = index_snapshot(&dir).unwrap();
        let probe = fixture.path().join("probe.index");
        assert!(previous_index_base(&git, &index, &probe, &after).is_err());
        assert_eq!(index_snapshot(&dir).unwrap(), index);
        assert!(!git.root().join("added").exists());
        assert_eq!(
            git.get(["show", ":staged"]).unwrap(),
            b"independent staged edit"
        );
        assert_eq!(
            fs::read(git.root().join("staged")).unwrap(),
            b"independent unstaged edit"
        );
    }

    #[test]
    fn two_ref_only_moves_do_not_turn_an_unmaterialized_path_into_a_deletion() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let before = fixture_commit(&git, "baseline");
        let live = git.root().join("x");
        fs::write(&live, b"introduced").unwrap();
        git.get(["add", "x"]).unwrap();
        let added = fixture_commit(&git, "add x");
        fs::write(&live, b"latest dev bytes").unwrap();
        git.get(["add", "x"]).unwrap();
        let edited = fixture_commit(&git, "edit x");
        git.get(["reset", "--hard", &before]).unwrap();
        let dir = git.root().join(".git");
        let index = index_snapshot(&dir).unwrap();
        git.get([
            "update-ref",
            "-m",
            "dev adds x",
            "refs/heads/dev",
            &added,
            &before,
        ])
        .unwrap();
        git.get([
            "update-ref",
            "-m",
            "dev edits x",
            "refs/heads/dev",
            &edited,
            &added,
        ])
        .unwrap();
        assert_eq!(
            git.id(["reflog", "show", "-1", "--format=%H", "HEAD"])
                .unwrap(),
            edited
        );

        let probe = fixture.path().join("probe.index");
        assert!(previous_index_base(&git, &index, &probe, &edited).is_err());
        assert_eq!(git.id(["rev-parse", "refs/heads/dev"]).unwrap(), edited);
        assert_eq!(
            git.get(["show", "refs/heads/dev:x"]).unwrap(),
            b"latest dev bytes"
        );
        assert!(!live.exists());
        assert_eq!(index_snapshot(&dir).unwrap(), index);
    }

    #[test]
    fn intentional_staged_and_unstaged_deletions_remain_edits() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        for path in ["staged", "unstaged"] {
            fs::write(git.root().join(path), b"tracked").unwrap();
        }
        git.get(["add", "staged", "unstaged"]).unwrap();
        fixture_commit(&git, "baseline");
        fs::write(git.root().join("added"), b"from current dev").unwrap();
        git.get(["add", "added"]).unwrap();
        let head = fixture_commit(&git, "add file");
        git.get(["rm", "staged"]).unwrap();
        fs::remove_file(git.root().join("unstaged")).unwrap();
        let dir = git.root().join(".git");
        let index = index_snapshot(&dir).unwrap();
        let probe = fixture.path().join("probe.index");
        assert_eq!(
            previous_index_base(&git, &index, &probe, &head).unwrap(),
            None
        );
        assert!(
            git.text(["status", "--porcelain"])
                .unwrap()
                .contains("D  staged")
        );
        assert!(
            git.text(["status", "--porcelain"])
                .unwrap()
                .contains(" D unstaged")
        );

        // A staged deletion of a newly added file has the same index entry
        // as a ref-only update that added it. Leave its intent to Git.
        git.get(["rm", "added"]).unwrap();
        let index = index_snapshot(&dir).unwrap();
        assert!(previous_index_base(&git, &index, &probe, &head).is_err());
        assert!(!git.root().join("added").exists());
        assert_eq!(index_snapshot(&dir).unwrap(), index);
    }

    #[test]
    fn linked_snapshot_excludes_resets_and_sparse_index_changes() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (fixture, ctx) = git_fixture();
        let primary = Git::new(&ctx, ctx.root.path());
        let baseline = fixture_commit(&primary, "baseline");
        fs::write(primary.root().join("tracked"), b"live checkout").unwrap();
        primary.get(["add", "tracked"]).unwrap();
        let tip = fixture_commit(&primary, "add file");
        primary.get(["switch", "-qc", "fixture-primary"]).unwrap();
        let primary_dir = primary.root().join(".git");
        let primary_index = index_snapshot(&primary_dir).unwrap();
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
        let (dir, _) = super::super::git::repository_dirs(&git).unwrap();
        let expected = index_snapshot(&dir).unwrap();
        let snapshot = SnapshotIndex::acquire(&git, &dir).unwrap();
        assert!(SnapshotIndex::acquire(&git, &dir).is_err());
        assert!(git.get(["reset", "--hard", &baseline]).is_err());
        assert!(
            git.get(["update-index", "--skip-worktree", "tracked"])
                .is_err()
        );
        assert!(
            git.get(["sparse-checkout", "set", "--no-cone", "/config.toml"])
                .is_err()
        );
        assert_eq!(git.id(["rev-parse", "refs/heads/dev"]).unwrap(), tip);
        assert_eq!(fs::read(linked.join("tracked")).unwrap(), b"live checkout");
        assert_eq!(index_snapshot(&dir).unwrap(), snapshot.bytes());
        assert_eq!(snapshot.bytes(), expected);
        assert_eq!(index_snapshot(&primary_dir).unwrap(), primary_index);
        assert!(!primary_dir.join("index.lock").exists());
        drop(snapshot);
        assert!(!dir.join("index.lock").exists());

        git.get(["sparse-checkout", "disable"]).unwrap();
        git.get(["update-index", "--skip-worktree", "tracked"])
            .unwrap();
        fs::remove_file(linked.join("tracked")).unwrap();
        assert!(SnapshotIndex::acquire(&git, &dir).is_err());
        assert!(!dir.join("index.lock").exists());
    }

    #[test]
    fn snapshot_reclaims_only_its_own_stale_git_lock() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        fixture_commit(&git, "baseline");
        let dir = git.root().join(".git");
        let lock = dir.join("index.lock");
        fs::write(&lock, b"another Git writer").unwrap();
        assert!(SnapshotIndex::acquire(&git, &dir).is_err());
        assert_eq!(fs::read(&lock).unwrap(), b"another Git writer");
        fs::remove_file(&lock).unwrap();
        fs::hard_link(dir.join(SNAPSHOT_LOCK), &lock).unwrap();
        let snapshot = SnapshotIndex::acquire(&git, &dir).unwrap();
        assert_eq!(snapshot.bytes(), index_snapshot(&dir).unwrap());
        drop(snapshot);
        assert!(!lock.exists());
    }

    #[test]
    fn interrupted_cleanup_has_no_active_journal_and_allows_another_install() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let before = fixture_commit(&git, "baseline");
        let after = fixture_commit(&git, "candidate");
        git.get(["reset", "--hard", &before]).unwrap();
        let dir = git.root().join(".git");
        let index = index_snapshot(&dir).unwrap();
        begin(&git, &dir, &index, &before, &after, false).unwrap();
        let pending = dir.join(PENDING);
        let mut progress = Interrupted::default();
        let installed =
            apply(&git, &dir, &pending, &mut progress, || false, replace_index).unwrap();
        let retired = park_pending(&dir, &pending, &mut progress).unwrap();
        assert!(progress.backups.is_empty());
        fs::remove_file(retired.join("plan.json")).unwrap();
        assert!(finish(&git, &dir, || false).unwrap().is_none());
        assert!(!dir.join("index.lock").exists());
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), after);
        begin(&git, &dir, &installed.index_after, &after, &after, false).unwrap();
        assert!(finish(&git, &dir, || false).unwrap().is_some());
        // A different writer's entry created after the rename survives
        // cleanup and cannot obstruct the next transaction.
        let note = retired.join("other-writer");
        fs::write(&note, b"keep").unwrap();
        assert!(clean_retired(&retired).is_err());
        assert_eq!(fs::read(note).unwrap(), b"keep");
    }

    #[test]
    fn prepared_head_transaction_excludes_competing_git_writers() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let before = fixture_commit(&git, "baseline");
        let after = fixture_commit(&git, "desired");
        let other = fixture_commit(&git, "competing commit");
        git.get(["reset", "--hard", &before]).unwrap();
        let dir = git.root().join(".git");
        let index = index_snapshot(&dir).unwrap();
        begin(&git, &dir, &index, &before, &after, false).unwrap();
        let installed = apply(
            &git,
            &dir,
            &dir.join(PENDING),
            &mut Interrupted::default(),
            || false,
            |staged, path| {
                replace_index(staged, path)?;
                git.get(["update-ref", "refs/heads/dev", &other, &before])?;
                Ok(())
            },
        );
        assert!(installed.is_err());
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), other);
        assert_eq!(index_snapshot(&dir).unwrap(), index);
        assert!(!has_pending(&dir));
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
        let problem = finish(&git, &dir, || false).err().expect("dev moved");
        assert_eq!(problem.moved_dev, Some((before, after)));
        assert!(problem.backups.is_empty());
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
                &mut Interrupted::default(),
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
                .all(|path| path.exists() && !path.starts_with(dir.join(PENDING))),
            "invalid retired recovery paths: {:?}",
            backups
                .iter()
                .map(|path| (path, fs::symlink_metadata(path).map(|md| md.file_type())))
                .collect::<Vec<_>>()
        );
        assert_eq!(fs::read(live).unwrap(), b"incoming file");
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), after);
    }

    #[test]
    fn retired_journal_preserves_a_link_that_is_not_a_directory_identity() {
        let fixture = tempfile::tempdir().unwrap();
        let journal = fixture.path();
        let metadata = journal.join("done-2");
        let foreign = journal.join("unrelated-link");
        fs::write(&metadata, b"done").unwrap();
        std::os::unix::fs::symlink("unrelated-file", &foreign).unwrap();

        assert_eq!(
            recovery_paths(journal).as_slice(),
            std::slice::from_ref(&foreign)
        );
        assert!(clean_retired(journal).is_err());
        assert!(fs::symlink_metadata(metadata).is_err());
        assert_eq!(fs::read_link(foreign).unwrap(), Path::new("unrelated-file"));
    }

    #[test]
    fn a_competing_empty_nested_directory_survives_checkout() {
        use crate::sync::tests::{fixture_commit, git_fixture};
        use std::cell::Cell;

        let (fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let live = git.root().join("replace");
        let nested = live.join("middle/deep");
        let leaf = nested.join("leaf");
        fs::create_dir_all(&nested).unwrap();
        fs::write(&leaf, b"original").unwrap();
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

        let claimed = Cell::new(None);
        let saved = fixture.path().join("saved-empty-directory");
        let result = finish(&git, &dir, || {
            if !leaf.exists() && claimed.get().is_none() {
                fs::rename(&nested, &saved).unwrap();
                fs::create_dir(&nested).unwrap();
                claimed.set(Some(fs::metadata(&nested).unwrap().ino()));
            }
            false
        });
        assert!(result.is_err());
        assert_eq!(Some(fs::metadata(nested).unwrap().ino()), claimed.get());
        assert!(saved.is_dir());
        assert!(live.is_dir());
        assert!(has_pending(&dir));
        assert_eq!(index_snapshot(&dir).unwrap(), index);
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), before);
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
            &mut Interrupted::default(),
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
            &mut Interrupted::default(),
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
