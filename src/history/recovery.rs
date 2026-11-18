use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use nix::libc;
use serde::{Deserialize, Serialize};

use crate::Fail;
use crate::config::{RepoSetting, Vars, selection};

use super::checkpoint::{MARKER, STAGING, legacy_marker};
use super::keys::{self, LocalSettings};
use super::native::{Native, exists, private_dir};
use super::{Restore, blocked, host, invalid, state_repo, tracked, validate_origin};

const PREVIOUS: &str = "repo.git.pre-restore.";
pub(super) const TRACKING_REF: &str = "refs/remotes/origin/setup";
pub(super) const MAIN_REF: &str = "refs/heads/main";

#[derive(Deserialize, Serialize)]
struct Journal {
    root: PathBuf,
    root_device: u64,
    root_inode: u64,
    home: PathBuf,
    origin: String,
    branch: String,
    device: u64,
    inode: u64,
    seed: String,
    progress: Progress,
    previous: Option<Previous>,
}

#[derive(Deserialize, Serialize)]
enum Progress {
    Prepared,
    Pulling(Attempt),
    RollingBack(Attempt),
    Applied,
}

#[derive(Deserialize, Serialize)]
struct Attempt {
    target: String,
    head: String,
    live: BTreeMap<PathBuf, Option<FileVersion>>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct FileVersion {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    mode: u32,
}

#[derive(Deserialize, Serialize)]
struct Previous {
    directory: String,
    device: u64,
    inode: u64,
    sync: bool,
}

pub(super) fn restore(native: &Native<'_, '_>, settings: Restore<'_>) -> Result<(), Fail> {
    let Restore {
        machine,
        origin,
        identity,
        initialize,
    } = settings;
    let branch = host(machine)?;
    let selected = selection::read(&native.ctx.root, &native.ctx.env)?;
    if selected.machine_id.as_deref() != Some(machine)
        || selected
            .active_environments()
            .split(',')
            .any(|name| host_stream(name) && name != branch)
    {
        return Err(invalid(
            "active mise selection must name only the restore host profile",
        ));
    }
    let vars = Vars::read(&native.ctx.root)?;
    let expected = state_repo(&vars)?;
    validate_origin(native, &vars, origin)?;
    let (identity, recovered_keys) = keys::read_identity(native, identity)?;
    let mut local = LocalSettings::read(native)?;
    let mut recipients = local.recipients()?;
    recipients.extend(recovered_keys);
    keys::check_recipients(native, &recipients)?;
    let paths = tracked::paths(native, Some(&branch))?;
    let store = native.store()?;
    let raw = local
        .origin()
        .filter(|url| url.contains(':'))
        .map_or_else(|| origin.transport_url().into_owned(), str::to_owned);
    let _lock = super::native::lock(&store)?;
    let staging = parent(&store).join(STAGING);
    if exists(&staging)? && !fs::symlink_metadata(&staging).is_ok_and(|meta| meta.is_dir()) {
        return Err(invalid(
            "pending history install location is not a real directory",
        ));
    }
    let staged = staging.join("repo.git");
    let pending = if exists(&staged)? {
        Some(staged.as_path())
    } else if exists(&store.join(MARKER))? {
        Some(store.as_path())
    } else {
        None
    };
    let journal = pending
        .map(|pending| {
            native.bare(pending)?;
            let record = read_journal(pending)?;
            match_request(native, pending, &record, &raw, &branch)?;
            Ok::<_, Fail>(record)
        })
        .transpose()?;
    let prior_origin = native.effective_origin().ok();
    if local.origin() != Some(&raw) || local.branch() != Some(&branch) {
        if exists(&store)? && journal.is_none() && !initialize {
            return Err(invalid(
                "local history is connected elsewhere; inspect it before using dots restore --initialize-history",
            ));
        }
        local.set_origin(&branch, &RepoSetting::parse(&raw)?)?;
        local.install(native)?;
    }
    // The explicit fetch below must use the same native config that apply sees.
    native.require_origin(&raw, &branch)?;

    if exists(&staging)? {
        if pending == Some(staged.as_path()) {
            let record = journal
                .as_ref()
                .ok_or_else(|| blocked("pending history journal vanished"))?;
            native.destinations(Some(&staged), &raw, expected, &vars)?;
            if revision(native, &staged, MAIN_REF)? != record.seed {
                return Err(blocked("the staged history seed checkpoint changed"));
            }
            verify_tree(native, &staged, &record.seed, &branch, &paths)?;
            install(&staging, &store, record)?;
        } else if journal.is_some() {
            fs::remove_dir(&staging)
                .map_err(|_| blocked("could not finish pending history installation"))?;
        } else {
            return Err(invalid(
                "incomplete restore staging needs inspection before retrying",
            ));
        }
    }

    let journal = if let Some(record) = journal {
        Some(record)
    } else if exists(&legacy_marker(&store))? {
        return Err(blocked(
            "an older pending restore cannot be safely resumed by this build; finish it with the dots version that started it",
        ));
    } else {
        let old = existing_directory(&store)?;
        let connected = old.is_some()
            && native.bare(&store).is_ok()
            && revision(native, &store, MAIN_REF).is_ok()
            && prior_origin.is_some_and(|(url, current_branch)| {
                current_branch == branch && expected.matches(&url)
            })
            && native
                .local_origin(&store)?
                .is_none_or(|url| expected.matches(&url))
            && read_sync(&store)?.is_none_or(|status| {
                status.origin_url.is_none_or(|url| expected.matches(&url))
                    && status.origin_branch.is_none_or(|current| current == branch)
            });
        if connected {
            None
        } else {
            if old.is_some() && !initialize {
                return Err(invalid(
                    "local history is connected elsewhere; inspect it before using dots restore --initialize-history",
                ));
            }
            Some(prepare(native, &store, &raw, &branch, &vars, &paths, old)?)
        }
    };

    native.destinations(Some(&store), &raw, expected, &vars)?;
    let recovery = Recovery {
        native,
        store: &store,
        origin: &raw,
        branch: &branch,
        identity: &identity,
        paths: &paths,
    };
    let pending = journal.is_some();
    if let Some(record) = journal {
        recovery.resume(record)?;
    } else {
        recovery.restore_established()?;
    }

    let mut local = LocalSettings::read(native)?;
    local.set_history(&branch, &RepoSetting::parse(&raw)?, &identity, &recipients)?;
    local.install(native)?;
    if pending {
        fs::remove_file(store.join(MARKER))
            .map_err(|_| blocked("could not finish the pending restore marker"))?;
        fsync_dir(&store)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct SyncState {
    origin_url: Option<String>,
    origin_branch: Option<String>,
    upstream_commit: Option<String>,
    #[serde(default)]
    conflicts: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    pending_directories: usize,
    #[serde(default)]
    pending_applications: Vec<PendingApplication>,
}

#[derive(Deserialize)]
struct PendingApplication {
    branch_path: String,
    configuration: bool,
    object: Option<(String, String)>,
}

fn read_sync(store: &Path) -> Result<Option<SyncState>, Fail> {
    let path = parent(store).join("sync.json");
    if !exists(&path)? {
        return Ok(None);
    }
    serde_json::from_reader(read_private(&path)?)
        .map(Some)
        .map_err(|_| invalid("could not verify native history synchronization state"))
}

struct Recovery<'a, 'run, 'ctx> {
    native: &'a Native<'run, 'ctx>,
    store: &'a Path,
    origin: &'a str,
    branch: &'a str,
    identity: &'a Path,
    paths: &'a [PathBuf],
}

impl Recovery<'_, '_, '_> {
    fn command(&self, args: &[&str], offline: bool) -> Result<Command, Fail> {
        let mut command = self.native.mise(args)?;
        command
            .env("MISE_HISTORY_SYNC", "fetch-only")
            .env("MISE_AGE_IDENTITY_FILES", self.identity)
            .env("GIT_TERMINAL_PROMPT", "0");
        if offline {
            command.env("GIT_ALLOW_PROTOCOL", "");
        }
        Ok(command)
    }

    fn fetch(&self) -> Result<String, Fail> {
        let head = revision(self.native, self.store, MAIN_REF)?;
        self.verify(&head)?;
        tracked::secure(self.native, self.paths)?;
        self.native.require_origin(self.origin, self.branch)?;
        let cmd = self.command(&["bootstrap", "dotfiles", "sync", "--fetch-only"], false)?;
        apply(self.native, cmd, "mise could not fetch encrypted history")?;
        revision(self.native, self.store, TRACKING_REF)
    }

    fn verify(&self, commit: &str) -> Result<BTreeSet<PathBuf>, Fail> {
        verify_tree(self.native, self.store, commit, self.branch, self.paths)
    }

    fn verify_plan(&self, target: &str) -> Result<(), Fail> {
        let head = revision(self.native, self.store, MAIN_REF)?;
        let mut allowed = self.verify(&head)?;
        allowed.extend(self.verify(target)?);
        if revision(self.native, self.store, TRACKING_REF)? != target {
            return Err(blocked(
                "fetched history changed while preparing restore; retry",
            ));
        }
        let sync = read_sync(self.store)?
            .ok_or_else(|| blocked("native fetch did not record a restore plan"))?;
        if sync.upstream_commit.as_deref() != Some(target) || sync.pending_directories != 0 {
            return Err(invalid(
                "native restore plan has an unchecked checkpoint or directory mode change",
            ));
        }
        if !sync.conflicts.is_empty() {
            return Err(history_conflict());
        }
        let stem = format!("home@{}/", self.branch);
        let home = physical_home(self.native)?;
        for application in sync.pending_applications {
            let path = application
                .branch_path
                .strip_prefix(&stem)
                .and_then(|relative| home_path(&home, relative));
            if application.configuration
                || path.is_none_or(|path| !allowed.contains(&path))
                || application
                    .object
                    .is_some_and(|(mode, oid)| mode != "100644" || !valid_oid(&oid))
            {
                return Err(unsafe_tree());
            }
        }
        if tracked::paths(self.native, Some(self.branch))? != self.paths {
            return Err(blocked(
                "active encrypted history paths changed while preparing restore",
            ));
        }
        Ok(())
    }

    fn restore_established(&self) -> Result<(), Fail> {
        let target = self.fetch()?;
        self.verify_plan(&target)?;
        self.pull()
    }

    fn pull(&self) -> Result<(), Fail> {
        let cmd = self.command(&["--yes", "bootstrap", "dotfiles", "pull"], true)?;
        apply(
            self.native,
            cmd,
            "mise could not apply incoming private settings",
        )
    }

    fn resume(&self, mut journal: Journal) -> Result<(), Fail> {
        loop {
            journal.progress = match &journal.progress {
                Progress::Prepared => {
                    let target = self.fetch()?;
                    self.verify_plan(&target)?;
                    Progress::Pulling(self.attempt(target, self.paths.iter())?)
                }
                Progress::Pulling(attempt) => {
                    let completed = self.completion(attempt, "apply")?;
                    if completed.is_none() {
                        require_unchanged(&attempt.live, &BTreeSet::new())?;
                        self.verify_plan(&attempt.target)?;
                        self.pull()?;
                    }
                    let written = self.completion(attempt, "apply")?.unwrap_or_default();
                    require_unchanged(&attempt.live, &written)?;
                    let allowed = self.verify(&attempt.target)?;
                    let remaining: BTreeMap<_, _> = attempt
                        .live
                        .iter()
                        .filter(|(path, _)| allowed.contains(*path) && !written.contains(*path))
                        .map(|(path, version)| (path.clone(), version.clone()))
                        .collect();
                    if remaining.is_empty() {
                        Progress::Applied
                    } else {
                        Progress::RollingBack(Attempt {
                            target: attempt.target.clone(),
                            head: revision(self.native, self.store, MAIN_REF)?,
                            live: remaining,
                        })
                    }
                }
                Progress::RollingBack(attempt) => {
                    if self.completion(attempt, "rollback")?.is_none() {
                        require_unchanged(&attempt.live, &BTreeSet::new())?;
                        self.verify_rollback(&attempt.target, attempt.live.keys())?;
                        // Native rollback records the encrypted undo of the live files.
                        let target = format!("commit:{}", attempt.target);
                        for mode in ["--dry-run", "--yes"] {
                            let mut command = self.command(
                                &[
                                    "bootstrap",
                                    "dotfiles",
                                    "rollback",
                                    "--to",
                                    &target,
                                    mode,
                                    "--",
                                ],
                                true,
                            )?;
                            command.args(attempt.live.keys());
                            apply(
                                self.native,
                                command,
                                "mise could not apply the first private history checkpoint",
                            )?;
                        }
                    }
                    Progress::Applied
                }
                Progress::Applied => return Ok(()),
            };
            write_journal(self.store, &journal)?;
        }
    }

    fn attempt<'p>(
        &self,
        target: String,
        paths: impl Iterator<Item = &'p PathBuf>,
    ) -> Result<Attempt, Fail> {
        let live = paths
            .map(|path| Ok((path.clone(), file_version(path)?)))
            .collect::<Result<_, Fail>>()?;
        let head = revision(self.native, self.store, MAIN_REF)?;
        Ok(Attempt { target, head, live })
    }

    fn checkpoint(&self, commit: &str) -> Result<NativeCheckpoint, Fail> {
        let reference = format!("commit:{commit}");
        let cmd = self.command(
            &[
                "bootstrap",
                "dotfiles",
                "history",
                "show",
                &reference,
                "--json",
            ],
            true,
        )?;
        let output = self
            .native
            .output(cmd, "could not inspect native checkpoint coverage")?;
        serde_json::from_slice(&output)
            .map_err(|_| invalid("could not read native checkpoint coverage"))
    }

    fn completion(&self, attempt: &Attempt, kind: &str) -> Result<Option<BTreeSet<PathBuf>>, Fail> {
        let head = revision(self.native, self.store, MAIN_REF)?;
        if head == attempt.head {
            return Ok(None);
        }
        if !ancestor(self.native, self.store, &attempt.head, &head)? {
            return Err(blocked("local history changed outside the pending restore"));
        }
        let native = self.checkpoint(&head)?;
        let Some(operation) = native.operation else {
            return Ok(None);
        };
        if operation.kind != kind
            || operation.status != "completed"
            || operation.to.or(operation.applied).as_deref() != Some(&attempt.target)
        {
            return Ok(None);
        }
        let home = physical_home(self.native)?;
        let written = operation
            .affected
            .iter()
            .map(|path| {
                covered_path(&home, path)
                    .filter(|path| attempt.live.contains_key(path))
                    .ok_or_else(unsafe_tree)
            })
            .collect::<Result<_, _>>()?;
        Ok(Some(written))
    }

    fn verify_rollback<'p>(
        &self,
        target: &str,
        paths: impl Iterator<Item = &'p PathBuf>,
    ) -> Result<(), Fail> {
        let allowed = self.verify(target)?;
        let checkpoint = self.checkpoint(target)?;
        let home = physical_home(self.native)?;
        let covered: BTreeSet<_> = checkpoint
            .tree
            .coverage
            .entries
            .into_iter()
            .filter(|entry| {
                entry.mode == "track"
                    && entry.encrypt
                    && entry.variant.as_deref() == Some(self.branch)
            })
            .filter_map(|entry| covered_path(&home, &entry.path))
            .collect();
        let unverified = checkpoint
            .tree
            .coverage
            .omitted
            .into_iter()
            .chain(checkpoint.tree.coverage.incomplete)
            .map(|entry| entry.path)
            .collect::<Vec<_>>();
        for path in paths {
            if !allowed.contains(path)
                || !covered.contains(path)
                || unverified.iter().any(|entry| {
                    covered_path(&home, entry).is_none_or(|entry| path.starts_with(entry))
                })
            {
                return Err(invalid(
                    "first restore includes a path without complete encrypted checkpoint coverage",
                ));
            }
        }
        if tracked::paths(self.native, Some(self.branch))? != self.paths {
            return Err(blocked(
                "active encrypted history paths changed while preparing restore",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct NativeCheckpoint {
    tree: NativeTree,
    operation: Option<NativeOperation>,
}

#[derive(Deserialize)]
struct NativeTree {
    coverage: NativeCoverage,
}

#[derive(Deserialize)]
struct NativeCoverage {
    entries: Vec<NativeEntry>,
    #[serde(default)]
    omitted: Vec<NativePath>,
    #[serde(default)]
    incomplete: Vec<NativePath>,
}

#[derive(Deserialize)]
struct NativeEntry {
    path: String,
    mode: String,
    encrypt: bool,
    variant: Option<String>,
}

#[derive(Deserialize)]
struct NativePath {
    path: String,
}

#[derive(Deserialize)]
struct NativeOperation {
    kind: String,
    status: String,
    to: Option<String>,
    applied: Option<String>,
    #[serde(default)]
    affected: Vec<String>,
}

fn covered_path(home: &Path, path: &str) -> Option<PathBuf> {
    home_path(home, path.strip_prefix("~/")?)
}

fn file_version(path: &Path) -> Result<Option<FileVersion>, Fail> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(FileVersion {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            mode: metadata.mode(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        _ => Err(unsafe_tree()),
    }
}

fn require_unchanged(
    live: &BTreeMap<PathBuf, Option<FileVersion>>,
    written: &BTreeSet<PathBuf>,
) -> Result<(), Fail> {
    for (path, previous) in live {
        if !written.contains(path) && file_version(path)? != *previous {
            return Err(blocked(
                "a tracked file changed after restore started; inspect the native restore journal before retrying",
            ));
        }
    }
    Ok(())
}

fn prepare(
    native: &Native<'_, '_>,
    store: &Path,
    raw: &str,
    branch: &str,
    vars: &Vars,
    paths: &[PathBuf],
    old: Option<fs::Metadata>,
) -> Result<Journal, Fail> {
    let base = parent(store);
    let temp = tempfile::Builder::new()
        .prefix(".dots-restore-clone-")
        .tempdir_in(base)
        .map_err(|_| blocked("could not stage a private history clone"))?;
    let staged = temp.path().join("repo.git");
    private_dir(&staged).map_err(|_| blocked("could not stage a private history repository"))?;
    let mut cmd = native.git(Some(&staged))?;
    cmd.args(["init", "--bare", "--initial-branch=main"]);
    native.output(cmd, "could not initialize the staged bare history clone")?;
    let mut cmd = native.git(Some(&staged))?;
    cmd.args(["config", "--local", "remote.origin.url", raw]);
    native.output(cmd, "could not configure the staged history origin")?;
    native.destinations(Some(&staged), raw, state_repo(vars)?, vars)?;
    let mut cmd = native.git(Some(&staged))?;
    let refspec = format!("refs/heads/{branch}:refs/heads/main");
    cmd.args([
        "fetch",
        "--no-tags",
        "--no-recurse-submodules",
        "--",
        raw,
        &refspec,
    ]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    native.output(
        cmd,
        "could not stage the selected remote history branch; the existing store was left unchanged",
    )?;
    native.bare(&staged)?;
    let seed = revision(native, &staged, MAIN_REF)?;
    verify_tree(native, &staged, &seed, branch, paths)?;
    fs::set_permissions(&staged, Permissions::from_mode(0o700))
        .map_err(|_| blocked("could not protect the staged history store"))?;

    let backup = old
        .as_ref()
        .map(|_| tempfile::Builder::new().prefix(PREVIOUS).tempdir_in(base))
        .transpose()
        .map_err(|_| blocked("could not reserve a private history backup directory"))?;
    if let Some(backup) = &backup {
        fs::set_permissions(backup.path(), Permissions::from_mode(0o700))
            .map_err(|_| blocked("could not protect the previous history store directory"))?;
    }
    let previous = match (old, backup.as_ref()) {
        (Some(old), Some(backup)) => Some(Previous {
            directory: backup
                .path()
                .file_name()
                .expect("backup basename")
                .to_string_lossy()
                .into_owned(),
            device: old.dev(),
            inode: old.ino(),
            sync: exists(&base.join("sync.json"))?,
        }),
        _ => None,
    };
    let staged_metadata =
        existing_directory(&staged)?.ok_or_else(|| blocked("staged history store vanished"))?;
    let root = native.ctx.root.path();
    let root_metadata =
        existing_directory(root)?.ok_or_else(|| blocked("configuration root vanished"))?;
    let record = Journal {
        root: native.ctx.root.path().to_owned(),
        root_device: root_metadata.dev(),
        root_inode: root_metadata.ino(),
        home: physical_home(native)?,
        origin: raw.to_owned(),
        branch: branch.to_owned(),
        device: staged_metadata.dev(),
        inode: staged_metadata.ino(),
        seed,
        progress: Progress::Prepared,
        previous,
    };
    write_journal(&staged, &record)?;
    let staging = base.join(STAGING);
    fs::rename(temp.path(), &staging)
        .map_err(|_| blocked("could not retain the staged history clone"))?;
    if let Some(backup) = backup {
        let _ = backup.keep();
    }
    fsync_dir(base)?;
    install(&staging, store, &record)?;
    Ok(record)
}

fn install(staging: &Path, store: &Path, record: &Journal) -> Result<(), Fail> {
    let base = parent(store);
    if let Some(previous) = &record.previous {
        let name = &previous.directory;
        if !name.starts_with(PREVIOUS) || name.contains('/') {
            return Err(invalid(
                "invalid previous-store path in pending history journal",
            ));
        }
        let dir = base.join(name);
        if !fs::symlink_metadata(&dir)
            .is_ok_and(|meta| meta.is_dir() && meta.mode() & 0o777 == 0o700)
        {
            return Err(invalid(
                "could not verify the private previous-store directory",
            ));
        }
        let backup = dir.join("repo.git");
        let matches =
            |meta: &fs::Metadata| meta.dev() == previous.device && meta.ino() == previous.inode;
        match (existing_directory(store)?, existing_directory(&backup)?) {
            (Some(old), None) if matches(&old) => {
                fs::rename(store, &backup)
                    .map_err(|_| blocked("could not preserve the previous history store"))?;
                fsync_dir(&dir)?;
                fsync_dir(base)?;
            }
            (None, Some(old)) if matches(&old) => {}
            _ => {
                return Err(invalid(
                    "the original history store changed; inspect the pending install before retrying",
                ));
            }
        }
        if previous.sync {
            let from = base.join("sync.json");
            let to = dir.join("sync.json");
            match (exists(&from)?, exists(&to)?) {
                (true, false) => {
                    fs::rename(from, to).map_err(|_| {
                        blocked("could not preserve previous history sync metadata")
                    })?;
                    fsync_dir(&dir)?;
                    fsync_dir(base)?;
                }
                (false, true) => {}
                _ => {
                    return Err(invalid(
                        "previous history sync metadata changed; inspect the pending install",
                    ));
                }
            }
        }
        println!(
            "Preserved the previous history store at {}",
            backup.display()
        );
    } else if exists(store)? {
        return Err(invalid(
            "the first history store appeared during staging; inspect it before retrying",
        ));
    }

    fs::rename(staging.join("repo.git"), store).map_err(|_| {
        blocked("could not install the staged history clone; the pending install can be retried")
    })?;
    fsync_dir(base)?;
    fs::remove_dir(staging)
        .map_err(|_| blocked("could not finish the history install staging directory"))
}

fn parent(store: &Path) -> &Path {
    store.parent().expect("history store has a parent")
}

fn existing_directory(path: &Path) -> Result<Option<fs::Metadata>, Fail> {
    if !exists(path)? {
        return Ok(None);
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|_| blocked("could not inspect the history repository"))?;
    if !meta.is_dir() {
        return Err(invalid("refusing a symlink or non-directory history store"));
    }
    Ok(Some(meta))
}

fn fsync_dir(path: &Path) -> Result<(), Fail> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| blocked("could not persist the history install state"))
}

fn write_journal(store: &Path, record: &Journal) -> Result<(), Fail> {
    let data = serde_json::to_vec(record)
        .map_err(|_| blocked("could not record pending history recovery"))?;
    let mut file = tempfile::NamedTempFile::new_in(store)
        .map_err(|_| blocked("could not record pending history recovery"))?;
    file.write_all(&data)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|_| blocked("could not persist the pending history recovery"))?;
    file.persist(store.join(MARKER))
        .map_err(|_| blocked("could not install the pending history recovery marker"))?;
    fsync_dir(store)
}

fn read_journal(store: &Path) -> Result<Journal, Fail> {
    serde_json::from_reader(read_private(&store.join(MARKER))?.take(4 * 1024 * 1024))
        .map_err(|_| invalid("could not validate the pending history recovery marker"))
}

fn read_private(path: &Path) -> Result<File, Fail> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .and_then(|file| {
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("not a regular marker"));
            }
            Ok(file)
        })
        .map_err(|_| invalid("could not read local history metadata"))
}

fn match_request(
    native: &Native<'_, '_>,
    store: &Path,
    record: &Journal,
    origin: &str,
    branch: &str,
) -> Result<(), Fail> {
    let root = native.ctx.root.path();
    let root_metadata =
        existing_directory(root)?.ok_or_else(|| blocked("configuration root vanished"))?;
    if record.root != root
        || record.home != physical_home(native)?
        || (record.root_device, record.root_inode) != (root_metadata.dev(), root_metadata.ino())
        || record.origin != origin
        || record.branch != branch
    {
        return Err(invalid(
            "pending history restore belongs to another configuration root, origin or branch",
        ));
    }
    let metadata =
        existing_directory(store)?.ok_or_else(|| blocked("pending history store vanished"))?;
    if (record.device, record.inode) != (metadata.dev(), metadata.ino()) {
        return Err(invalid(
            "pending history restore belongs to a different store",
        ));
    }
    let head = revision(native, store, MAIN_REF)?;
    if !ancestor(native, store, &record.seed, &head)? {
        return Err(invalid(
            "pending history store no longer contains its seed checkpoint",
        ));
    }
    Ok(())
}

pub(super) fn revision(
    native: &Native<'_, '_>,
    store: &Path,
    reference: &str,
) -> Result<String, Fail> {
    let mut cmd = local_git(native, store)?;
    let commit = format!("{reference}^{{commit}}");
    cmd.args(["rev-parse", "--verify", &commit]);
    let id = native.text(
        cmd,
        "could not resolve the fetched private history checkpoint",
    )?;
    let id = id.trim_end();
    if !valid_oid(id) {
        return Err(blocked("Git returned an invalid private history commit ID"));
    }
    Ok(id.to_owned())
}

pub(super) fn valid_oid(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|c| c.is_ascii_hexdigit())
}

pub(super) fn ancestor(
    native: &Native<'_, '_>,
    store: &Path,
    from: &str,
    to: &str,
) -> Result<bool, Fail> {
    if !valid_oid(from) || !valid_oid(to) {
        return Err(blocked("invalid history checkpoint ID"));
    }
    let mut cmd = local_git(native, store)?;
    cmd.args(["merge-base", "--is-ancestor", from, to]);
    match native
        .probe(cmd, "could not check history checkpoint ancestry")?
        .status
        .code()
    {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(blocked("could not check history checkpoint ancestry")),
    }
}

fn local_git(native: &Native<'_, '_>, store: &Path) -> Result<Command, Fail> {
    let mut command = native.git(Some(store))?;
    // Even a nominally local Git read can fetch missing partial-clone objects.
    command
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0");
    Ok(command)
}

#[derive(Deserialize)]
struct Manifest {
    enrollment: Vec<Enrollment>,
    permissions: BTreeMap<String, u32>,
}

#[derive(Deserialize)]
struct Enrollment {
    path: PathBuf,
    encrypt: bool,
    variants: Vec<Variant>,
}

#[derive(Deserialize)]
struct Variant {
    profile: Option<String>,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    machine: bool,
}

fn verify_tree(
    native: &Native<'_, '_>,
    store: &Path,
    commit: &str,
    branch: &str,
    paths: &[PathBuf],
) -> Result<BTreeSet<PathBuf>, Fail> {
    if !valid_oid(commit) {
        return Err(invalid("invalid private history checkpoint ID"));
    }
    let mut cmd = local_git(native, store)?;
    cmd.args(["show", &format!("{commit}:.mise-history/manifest.json")]);
    let data = native.output(cmd, "could not read the native history checkpoint manifest")?;
    let manifest: Manifest = serde_json::from_slice(&data)
        .map_err(|_| invalid("invalid encrypted history checkpoint manifest"))?;
    let home = physical_home(native)?;
    let mut enrolled = BTreeSet::new();
    let mut streams = BTreeSet::new();
    for entry in manifest.enrollment {
        let path = entry
            .path
            .to_str()
            .and_then(|path| path.strip_prefix("home/"))
            .and_then(|path| home_path(&home, path))
            .ok_or_else(unsafe_tree)?;
        if !streams.insert(path.clone()) || entry.variants.is_empty() {
            return Err(unsafe_tree());
        }
        let mut names = BTreeSet::new();
        for variant in entry.variants {
            let profile = variant
                .profile
                .filter(|name| host_stream(name))
                .ok_or_else(unsafe_tree)?;
            if variant.default
                || variant.machine
                || !variant.os.is_empty()
                || !names.insert(profile)
            {
                return Err(unsafe_tree());
            }
        }
        if names.contains(branch)
            && (!entry.encrypt || !paths.contains(&path) || !enrolled.insert(path))
        {
            return Err(unsafe_tree());
        }
    }

    let mut cmd = local_git(native, store)?;
    cmd.args(["ls-tree", "-r", "-t", "-z", commit]);
    let tree = native.output(cmd, "could not inspect private checkpoint objects")?;
    for row in tree.split(|&byte| byte == 0).filter(|row| !row.is_empty()) {
        let line = std::str::from_utf8(row).map_err(|_| unsafe_tree())?;
        let (object, path) = line.split_once('\t').ok_or_else(unsafe_tree)?;
        let mut object = object.split(' ');
        let (Some(mode), Some(kind), Some(oid), None) =
            (object.next(), object.next(), object.next(), object.next())
        else {
            return Err(unsafe_tree());
        };
        if path == ".mise-history" && kind == "tree"
            || matches!(
                path,
                ".mise-history/manifest.json" | ".mise-history/format.toml"
            ) && kind == "blob"
                && mode == "100644"
        {
            continue;
        }
        let (stem, suffix) = path.split_once('/').unwrap_or((path, ""));
        let Some(profile) = stem.strip_prefix("home@").filter(|name| host_stream(name)) else {
            return Err(unsafe_tree());
        };
        if profile != branch || suffix.is_empty() && kind == "tree" {
            continue;
        }
        let live = home_path(&home, suffix).ok_or_else(unsafe_tree)?;
        if kind == "tree"
            && enrolled
                .iter()
                .any(|entry| entry.starts_with(&live) && *entry != live)
        {
            continue;
        }
        if !enrolled.contains(&live)
            || kind != "blob"
            || mode != "100644"
            || !valid_oid(oid)
            || manifest.permissions.get(path) != Some(&0o600)
        {
            return Err(unsafe_tree());
        }
        let mut cmd = local_git(native, store)?;
        cmd.args(["cat-file", "blob", oid]);
        let blob = native.output(cmd, "could not inspect the encrypted checkpoint envelope")?;
        if !blob.starts_with(b"mise-encrypted-file-v1\n") {
            return Err(unsafe_tree());
        }
    }
    for (path, mode) in &manifest.permissions {
        let (stem, suffix) = path.split_once('/').ok_or_else(unsafe_tree)?;
        let live = home_path(&home, suffix).ok_or_else(unsafe_tree)?;
        if stem == "home" {
            if !enrolled
                .iter()
                .any(|entry| entry.starts_with(&live) && *entry != live)
            {
                if streams
                    .iter()
                    .any(|entry| entry.starts_with(&live) && *entry != live)
                {
                    continue;
                }
                return Err(unsafe_tree());
            }
            match fs::symlink_metadata(&live) {
                Ok(metadata) if metadata.is_dir() && metadata.mode() & 0o777 == *mode => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && *mode == 0o700 => {}
                _ => {
                    return Err(invalid(
                        "history would change permissions of a containing directory",
                    ));
                }
            }
        } else {
            let profile = stem
                .strip_prefix("home@")
                .filter(|name| host_stream(name))
                .ok_or_else(unsafe_tree)?;
            if profile == branch && (!enrolled.contains(&live) || *mode != 0o600) {
                return Err(unsafe_tree());
            }
        }
    }
    Ok(enrolled)
}

fn host_stream(profile: &str) -> bool {
    profile
        .strip_prefix("host-")
        .is_some_and(|machine| host(machine).is_ok())
}

fn unsafe_tree() -> Fail {
    invalid("private history includes an unsafe path, stream, object or recorded mode")
}

fn physical_home(native: &Native<'_, '_>) -> Result<PathBuf, Fail> {
    native
        .home()?
        .canonicalize()
        .map_err(|_| invalid("could not resolve HOME"))
}

fn home_path(home: &Path, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    (!relative.is_empty()
        && relative.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.eq_ignore_ascii_case(".git")
                && !part.contains(['\\', ':'])
                && !part.chars().any(char::is_control)
        })
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_))))
    .then(|| home.join(path))
}

fn apply(
    native: &Native<'_, '_>,
    command: std::process::Command,
    message: &str,
) -> Result<(), Fail> {
    let output = native.probe(command, message)?;
    if output.status.success() {
        return Ok(());
    }
    let conflict = [output.stdout, output.stderr]
        .iter()
        .any(|bytes| String::from_utf8_lossy(bytes).contains("both sides changed the same lines"));
    if conflict {
        Err(history_conflict())
    } else {
        Err(blocked(message))
    }
}

fn history_conflict() -> Fail {
    blocked(
        "mise could not restore private settings: both sides changed the same lines; resolve the native history conflict before retrying",
    )
}
