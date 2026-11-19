use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Permissions};
use std::io::{BufReader, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::Fail;
use crate::config::{RepoSetting, Vars, selection};

use super::checkpoint::{LEGACY_RESTORE, MARKER, STAGING, legacy_marker};
use super::keys::{self, LocalSettings};
use super::native::{Native, exists, private_dir, read_native_file};
use super::{Restore, blocked, host, invalid, state_repo, tracked, validate_origin};

const PREVIOUS: &str = "repo.git.pre-restore.";
const ATTEMPT: &str = ".dots-restore-attempt.";
const CLONE: &str = "clone-";
const RETIRING: &str = ".dots-restore-retiring";
pub(super) const TRACKING_REF: &str = "refs/remotes/origin/setup";
pub(super) const MAIN_REF: &str = "refs/heads/main";
pub(super) const INTERRUPTED_RESTORE: &str = "An earlier restore was interrupted and did not finish. Run dots mise dot history --pending to see a pending native operation, then dots mise dot recover UUID for that operation. Otherwise, review your tracked settings and retry with dots restore.";

#[derive(Deserialize, Serialize)]
struct Journal {
    root: PathBuf,
    home: PathBuf,
    paths: tracked::PathBinding,
    origin: String,
    branch: String,
    device: u64,
    inode: u64,
    seed: String,
    initial: bool,
    progress: Progress,
    obsolete_scope: Option<String>,
    previous: Option<StoreDirectory>,
}

#[derive(Deserialize, Serialize)]
enum Progress {
    Cloning(StoreDirectory),
    Prepared,
    Fetching(LiveVersions),
    Pulling(Attempt),
    RollingBack(Attempt),
    Applied,
}

#[derive(Deserialize, Serialize)]
struct Attempt {
    target: String,
    head: String,
    live: LiveVersions,
    created: BTreeSet<PathBuf>,
    scope: String,
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

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct ObservedFile {
    #[serde(flatten)]
    stamp: FileVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

type LiveVersions = BTreeMap<PathBuf, Option<ObservedFile>>;

#[derive(Clone, Deserialize, Serialize)]
struct StoreDirectory {
    directory: String,
    device: u64,
    inode: u64,
}

impl StoreDirectory {
    fn matches(&self, meta: &fs::Metadata) -> bool {
        (meta.dev(), meta.ino()) == (self.device, self.inode)
    }
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
    let configured = verify_capture_policy(native)?;
    let (identity, recovered_keys) = keys::read_identity(native, identity)?;
    let mut local = LocalSettings::read(native)?;
    let refresh_recipients = recovered_keys.iter().any(|key| !configured.contains(key));
    let mut recipients = local.recipients()?;
    recipients.extend(recovered_keys.iter().cloned());
    keys::check_recipients(native, &recipients)?;
    let paths = tracked::restore_paths(native, Some(&branch))?;
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
    resume_staging_retirement(native, &staging, &raw, &branch, &paths)?;
    let staged = staging.join("repo.git");
    let mut owned_stage = exists(&staging.join(MARKER))?;
    if owned_stage {
        let owner = read_journal(&staging)?;
        match_journal_owner(native, &staging, &owner, &raw, &branch)?;
        paths.check_binding(&owner.paths, false)?;
        let Progress::Cloning(clone) = &owner.progress else {
            return Err(invalid(
                "invalid ownership marker for an unfinished history clone",
            ));
        };
        if !exists(&staged.join(MARKER))? && !exists(&store.join(MARKER))? {
            discard_clone(&staging, clone)?;
            owned_stage = false;
        }
    }
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
    if exists(&staging)? {
        if pending == Some(staged.as_path()) {
            native.require_no_pending_operations()?;
            let record = journal
                .as_ref()
                .ok_or_else(|| blocked("pending history journal vanished"))?;
            paths.check_binding(&record.paths, false)?;
            native.destinations(Some(&staged), &raw, expected, &vars)?;
            if revision(native, &staged, MAIN_REF)? != record.seed {
                return Err(blocked("the staged history seed checkpoint changed"));
            }
            verify_tree(native, &staged, &record.seed, &branch, &paths)?;
            install(&staging, &store, record, owned_stage)?;
        } else if journal.is_some() {
            finish_staging(&staging, owned_stage)?;
        } else {
            return Err(invalid(
                "incomplete restore staging needs inspection before retrying",
            ));
        }
    }

    let journal = if let Some(record) = journal {
        Some(record)
    } else if exists(&legacy_marker(&store))? {
        return Err(blocked(LEGACY_RESTORE));
    } else {
        native.require_no_pending_operations()?;
        let prior_origin = native.effective_origin().ok();
        let old = existing_directory(&store)?;
        let connected = old.is_some()
            && native.bare(&store).is_ok()
            && revision(native, &store, MAIN_REF).is_ok()
            && prior_origin.is_some_and(|(url, current_branch)| {
                current_branch == branch && expected.matches(&url)
            })
            && match native.local_origin(&store)? {
                // Fetch-only can refresh the plan without replacing the
                // previous connection label in the native health record.
                Some(url) => expected.matches(&url),
                None => read_sync(&store)?.is_none_or(|status| {
                    status.origin_url.is_none_or(|url| expected.matches(&url))
                        && status.origin_branch.is_none_or(|current| current == branch)
                }),
            };
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
    // Classify and persist any store replacement before changing native origin
    // settings. Failed staging must leave the old connection authoritative.
    if refresh_recipients {
        // Native capture can use the current configuration for its undo.
        local.set_history(&branch, &RepoSetting::parse(&raw)?, &identity, &recipients)?;
        local.install(native)?;
    } else if local.origin() != Some(&raw) || local.branch() != Some(&branch) {
        local.set_origin(&branch, &RepoSetting::parse(&raw)?)?;
        local.install(native)?;
    }
    native.require_origin(&raw, &branch)?;
    let mut recovery = Recovery {
        native,
        store: &store,
        origin: &raw,
        branch: &branch,
        identity: &identity,
        paths,
    };
    if let Some(record) = journal {
        let established_retry = !record.initial && matches!(record.progress, Progress::Pulling(_));
        recovery.resume(record).map_err(|mut error| {
            if established_retry && error.diagnostic == changed_live_file().diagnostic {
                error.diagnostic = INTERRUPTED_RESTORE.into();
            }
            error
        })?;
    } else {
        recovery.restore_established()?;
    }

    let mut local = LocalSettings::read(native)?;
    let mut recipients = local.recipients()?;
    recipients.extend(recovered_keys);
    local.set_history(&branch, &RepoSetting::parse(&raw)?, &identity, &recipients)?;
    local.install(native)?;
    clear_journal(&store)
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

fn verify_capture_policy(native: &Native<'_, '_>) -> Result<BTreeSet<String>, Fail> {
    let mut cmd = native.mise(&["settings", "get", "history.allow_plaintext_history"])?;
    cmd.env("GIT_ALLOW_PROTOCOL", "");
    if native
        .text(cmd, "could not verify the plaintext history setting")?
        .trim()
        != "false"
    {
        return Err(invalid(
            "plaintext history must be disabled before restore; set history.allow_plaintext_history to false and retry dots restore",
        ));
    }
    super::recipient_config::read(native)
}

struct Recovery<'a, 'run, 'ctx> {
    native: &'a Native<'run, 'ctx>,
    store: &'a Path,
    origin: &'a str,
    branch: &'a str,
    identity: &'a Path,
    paths: tracked::TrackedPaths,
}

impl Recovery<'_, '_, '_> {
    fn command(&self, args: &[&str], offline: bool) -> Result<Command, Fail> {
        let mut command = self.native.mise(args)?;
        unmodified_git_graph(&mut command)
            .env_remove("__MISE_HISTORY_LOCAL")
            .env("MISE_HISTORY_SYNC", "fetch-only")
            .env("MISE_AGE_IDENTITY_FILES", self.identity)
            .env("GIT_TERMINAL_PROMPT", "0");
        if offline {
            command.env("GIT_ALLOW_PROTOCOL", "");
        }
        Ok(command)
    }

    fn fetch(&self, live: &LiveVersions) -> Result<String, Fail> {
        self.native.require_no_pending_operations()?;
        let head = revision(self.native, self.store, MAIN_REF)?;
        self.verify(&head)?;
        verify_capture_policy(self.native)?;
        self.native.require_origin(self.origin, self.branch)?;
        self.paths.recheck(self.native)?;
        self.native.require_no_pending_operations()?;
        let cmd = self.command(&["bootstrap", "dotfiles", "sync", "--fetch-only"], false)?;
        require_unchanged(live, &BTreeSet::new(), None)?;
        apply(self.native, cmd, "mise could not fetch encrypted history")?;
        require_unchanged(live, &BTreeSet::new(), None)?;
        revision(self.native, self.store, TRACKING_REF)
    }

    fn pin_fetch_paths(&self) -> Result<LiveVersions, Fail> {
        self.verify(&revision(self.native, self.store, MAIN_REF)?)?;
        verify_capture_policy(self.native)?;
        self.native.require_no_pending_operations()?;
        self.paths.secure(self.native)?;
        live_versions(&self.paths, Some(&self.paths))
    }

    fn verify(&self, commit: &str) -> Result<VerifiedTree, Fail> {
        verify_tree(self.native, self.store, commit, self.branch, &self.paths)
    }

    fn verify_plan(&self, target: &str) -> Result<Option<BTreeSet<PathBuf>>, Fail> {
        let head = revision(self.native, self.store, MAIN_REF)?;
        let local = self.verify(&head)?;
        let remote = self.verify(target)?;
        let allowed: BTreeSet<_> = local.paths.union(&remote.paths).collect();
        let directories: BTreeSet<_> = local.missing.union(&remote.missing).cloned().collect();
        if revision(self.native, self.store, TRACKING_REF)? != target {
            return Err(blocked(
                "fetched history changed while preparing restore; retry",
            ));
        }
        let sync = read_sync(self.store)?
            .ok_or_else(|| blocked("native fetch did not record a restore plan"))?;
        if sync.upstream_commit.as_deref() != Some(target) {
            return Err(invalid("native restore plan has an unchecked checkpoint"));
        }
        if !sync.conflicts.is_empty() {
            return Ok(None);
        }
        let stem = format!("home@{}/", self.branch);
        let home = physical_home(self.native)?;
        for application in sync.pending_applications {
            let path = application
                .branch_path
                .strip_prefix(&stem)
                .and_then(|relative| home_path(&home, relative));
            if application.configuration || path.is_none_or(|path| !allowed.contains(&path)) {
                return Err(unsafe_tree());
            }
            if let Some((mode, oid)) = application.object {
                // The native plan contains the transient decrypted object ID.
                // The persistent input envelopes were checked above.
                if mode != "100644" || !valid_oid(&oid) {
                    return Err(unsafe_tree());
                }
            }
        }
        self.paths.recheck(self.native)?;
        if sync.pending_directories == 0 {
            return Ok(Some(BTreeSet::new()));
        }
        self.verify_directories(&directories, sync.pending_directories)
            .map(Some)
    }

    fn verify_directories(
        &self,
        allowed: &BTreeSet<PathBuf>,
        expected_count: usize,
    ) -> Result<BTreeSet<PathBuf>, Fail> {
        self.before_apply()?;
        let mut command = self.command(&["bootstrap", "dotfiles", "pull", "--dry-run"], true)?;
        command.env("NO_COLOR", "1").env_remove("MISE_TERM_WIDTH");
        let plan = self.native.text(
            command,
            "could not verify native restore directory creation",
        )?;
        let home = physical_home(self.native)?;
        let mut directories = BTreeSet::new();
        for row in plan
            .lines()
            .filter_map(|line| line.trim_end().strip_suffix("setup"))
        {
            let Some(display) = row.trim_end().strip_suffix("permissions") else {
                continue;
            };
            let display = display.trim();
            let path = display
                .strip_prefix("~/")
                .and_then(|path| home_path(&home, path))
                .or_else(|| {
                    Path::new(display)
                        .is_absolute()
                        .then(|| PathBuf::from(display))
                })
                .filter(|path| allowed.contains(path))
                .ok_or_else(unsafe_tree)?;
            if !directories.insert(path) {
                return Err(unsafe_tree());
            }
        }
        if directories.len() != expected_count {
            return Err(unsafe_tree());
        }
        Ok(directories)
    }

    fn restore_established(&mut self) -> Result<(), Fail> {
        let record = new_journal(
            self.native,
            self.store,
            self.origin,
            self.branch,
            &self.paths,
            revision(self.native, self.store, MAIN_REF)?,
            false,
            Progress::Prepared,
            None,
        )?;
        self.resume(record)
    }

    fn launch_attempt(&self, attempt: &Attempt, rollback: bool) -> Result<(), Fail> {
        self.before_apply()?;
        let command = self.attempt_command(attempt, rollback)?;
        require_unchanged(&attempt.live, &BTreeSet::new(), Some(&self.paths))?;
        apply(
            self.native,
            command,
            "mise could not apply incoming private settings",
        )
    }

    fn before_apply(&self) -> Result<(), Fail> {
        let head = revision(self.native, self.store, MAIN_REF)?;
        self.verify(&head)?;
        verify_capture_policy(self.native)?;
        self.paths.recheck(self.native)?;
        self.native.require_no_pending_operations()
    }

    fn resume(&mut self, mut journal: Journal) -> Result<(), Fail> {
        if matches!(journal.progress, Progress::Prepared | Progress::Fetching(_)) {
            // Fetching has not applied incoming files, so a retry can bind
            // the current inventory. Prepared still validates the clone binding.
            if matches!(journal.progress, Progress::Fetching(_)) {
                journal.paths = self.paths.binding().clone();
            } else {
                self.paths.check_binding(&journal.paths, false)?;
            }
            journal.progress = Progress::Fetching(self.pin_fetch_paths()?);
            write_journal(self.store, &journal)?;
        }
        loop {
            if let Some(scope) = journal.obsolete_scope.take() {
                self.remove_scope(&scope)?;
                write_journal(self.store, &journal)?;
            }
            let resumed = match &mut journal.progress {
                Progress::Pulling(attempt) => {
                    self.recover_attempt(attempt, &journal.paths, false)?
                }
                Progress::RollingBack(attempt) => {
                    self.recover_attempt(attempt, &journal.paths, true)?
                }
                _ => false,
            };
            if resumed {
                self.paths = tracked::restore_paths(self.native, Some(self.branch))?;
                write_journal(self.store, &journal)?;
            }
            let obsolete_scope = match &journal.progress {
                Progress::Pulling(attempt) | Progress::RollingBack(attempt) => {
                    Some(attempt.scope.clone())
                }
                _ => None,
            };
            journal.progress = match &journal.progress {
                Progress::Cloning(_) | Progress::Prepared => {
                    return Err(invalid("refusing an unfinished history clone"));
                }
                Progress::Fetching(live) => {
                    self.paths.check_binding(&journal.paths, false)?;
                    let target = self.fetch(live)?;
                    let Some(created) = self.verify_plan(&target)? else {
                        // A first restore still needs its initial-apply marker.
                        if !journal.initial {
                            self.native.require_no_pending_operations()?;
                            clear_journal(self.store)?;
                        }
                        return Err(history_conflict());
                    };
                    Progress::Pulling(self.attempt(target, live.clone(), created)?)
                }
                Progress::Pulling(attempt) => {
                    let completed = self.completion(attempt, false)?;
                    if completed.is_none() {
                        self.paths.check_binding(&journal.paths, false)?;
                        require_unchanged(&attempt.live, &BTreeSet::new(), Some(&self.paths))?;
                        let created = self
                            .verify_plan(&attempt.target)?
                            .ok_or_else(history_conflict)?;
                        if created != attempt.created {
                            return Err(unsafe_tree());
                        }
                        self.launch_attempt(attempt, false)?;
                    }
                    let completed = self.completion(attempt, false)?;
                    if completed.is_some() {
                        self.refresh_paths(&mut journal.paths, &attempt.created)?;
                    } else {
                        self.paths.check_binding(&journal.paths, false)?;
                    }
                    let written = completed.unwrap_or_default();
                    require_unchanged(&attempt.live, &written, Some(&self.paths))?;
                    let allowed = self.verify(&attempt.target)?;
                    let remaining: LiveVersions = attempt
                        .live
                        .iter()
                        .filter(|(path, _)| {
                            allowed.paths.contains(*path) && !written.contains(*path)
                        })
                        .map(|(path, version)| (path.clone(), version.clone()))
                        .collect();
                    if remaining.is_empty() || !journal.initial {
                        Progress::Applied
                    } else {
                        let created = self.verify_rollback(&attempt.target, remaining.keys())?;
                        Progress::RollingBack(self.attempt(
                            attempt.target.clone(),
                            remaining,
                            created,
                        )?)
                    }
                }
                Progress::RollingBack(attempt) => {
                    if self.completion(attempt, true)?.is_none() {
                        self.paths.check_binding(&journal.paths, false)?;
                        require_unchanged(&attempt.live, &BTreeSet::new(), Some(&self.paths))?;
                        if self.verify_rollback(&attempt.target, attempt.live.keys())?
                            != attempt.created
                        {
                            return Err(unsafe_tree());
                        }
                        // Native rollback records the encrypted undo of the live files.
                        self.before_apply()?;
                        let target = format!("commit:{}", attempt.target);
                        let mut dry_run = self.command(
                            &[
                                "bootstrap",
                                "dotfiles",
                                "rollback",
                                "--to",
                                &target,
                                "--dry-run",
                                "--",
                            ],
                            true,
                        )?;
                        dry_run.args(attempt.live.keys());
                        apply(
                            self.native,
                            dry_run,
                            "mise could not verify the first private history checkpoint",
                        )?;
                        self.launch_attempt(attempt, true)?;
                        if self.completion(attempt, true)?.is_none() {
                            self.native.require_no_pending_operations()?;
                            require_unchanged(&attempt.live, &BTreeSet::new(), Some(&self.paths))?;
                            self.paths.check_binding(&journal.paths, false)?;
                        }
                    }
                    self.refresh_paths(&mut journal.paths, &attempt.created)?;
                    Progress::Applied
                }
                Progress::Applied => {
                    self.native.require_no_pending_operations()?;
                    return self.paths.check_binding(&journal.paths, false);
                }
            };
            journal.obsolete_scope = obsolete_scope;
            write_journal(self.store, &journal)?;
        }
    }

    fn attempt(
        &self,
        target: String,
        live: LiveVersions,
        created: BTreeSet<PathBuf>,
    ) -> Result<Attempt, Fail> {
        let head = revision(self.native, self.store, MAIN_REF)?;
        let directory = tempfile::Builder::new()
            .prefix(ATTEMPT)
            .tempdir_in(self.store)
            .map_err(|_| blocked("could not reserve a private native restore scope"))?;
        symlink(self.native.ctx.root.path(), directory.path().join("root"))
            .map_err(|_| blocked("could not bind the native restore to the configuration root"))?;
        let scope = directory
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(unsafe_tree)?
            .to_owned();
        fsync_dir(directory.path())?;
        let _retained = directory.keep();
        fsync_dir(self.store)?;
        Ok(Attempt {
            target,
            head,
            live,
            created,
            scope,
        })
    }

    fn scope_dir(&self, scope: &str) -> Result<PathBuf, Fail> {
        let valid = scope.strip_prefix(ATTEMPT).is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
        if !valid {
            return Err(unresolved_restore());
        }
        Ok(self.store.join(scope))
    }

    fn scope(&self, scope: &str) -> Result<PathBuf, Fail> {
        let directory = self.scope_dir(scope)?;
        let path = directory.join("root");
        if !fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir())
            || !fs::read_link(&path).is_ok_and(|target| target == self.native.ctx.root.path())
        {
            return Err(unresolved_restore());
        }
        Ok(path)
    }

    fn remove_scope(&self, scope: &str) -> Result<(), Fail> {
        let directory = self.scope_dir(scope)?;
        if existing_directory(&directory)?.is_none() {
            return Ok(());
        }
        if exists(&directory.join("root"))? {
            fs::remove_file(self.scope(scope)?)
                .map_err(|_| blocked("could not close the native restore scope"))?;
        }
        fs::remove_dir(directory).map_err(|_| blocked("could not close the native restore scope"))
    }

    fn attempt_command(&self, attempt: &Attempt, rollback: bool) -> Result<Command, Fail> {
        let scope = self.scope(&attempt.scope)?;
        let scope = scope.to_str().ok_or_else(unsafe_tree)?;
        if rollback {
            let target = format!("commit:{}", attempt.target);
            let mut command = self.command(
                &[
                    "--cd",
                    scope,
                    "bootstrap",
                    "dotfiles",
                    "rollback",
                    "--to",
                    &target,
                    "--yes",
                    "--",
                ],
                true,
            )?;
            command.args(attempt.live.keys());
            Ok(command)
        } else {
            self.command(
                &["--cd", scope, "--yes", "bootstrap", "dotfiles", "pull"],
                true,
            )
        }
    }

    fn refresh_paths(
        &mut self,
        binding: &mut tracked::PathBinding,
        admitted: &BTreeSet<PathBuf>,
    ) -> Result<(), Fail> {
        let current = tracked::restore_paths(self.native, Some(self.branch))?;
        current.check_binding(binding, true)?;
        for path in current.created_directories(binding) {
            if !admitted.contains(path)
                || !fs::symlink_metadata(path)
                    .is_ok_and(|meta| meta.is_dir() && meta.mode() & 0o7777 == 0o700)
            {
                return Err(unsafe_tree());
            }
        }
        *binding = current.binding().clone();
        self.paths = current;
        Ok(())
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

    fn completion(
        &self,
        attempt: &Attempt,
        rollback: bool,
    ) -> Result<Option<BTreeSet<PathBuf>>, Fail> {
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
        // A published native checkpoint deliberately omits argv and cwd.
        // Its target, kind, ancestry, and covered paths identify completion.
        if operation.kind != if rollback { "rollback" } else { "apply" }
            || operation.status != "completed"
            || operation.to.as_deref().or(operation.applied.as_deref()) != Some(&attempt.target)
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

    fn owns_operation(
        &self,
        attempt: &Attempt,
        rollback: bool,
        operation: &NativeOperation,
    ) -> Result<bool, Fail> {
        let command = self.attempt_command(attempt, rollback)?;
        Ok(
            operation.kind == if rollback { "rollback" } else { "apply" }
                && operation.cwd == self.native.ctx.root.path()
                && operation
                    .argv
                    .iter()
                    .map(String::as_str)
                    .eq(command.get_args()),
        )
    }

    fn recover_attempt(
        &self,
        attempt: &mut Attempt,
        binding: &tracked::PathBinding,
        rollback: bool,
    ) -> Result<bool, Fail> {
        let operations = self.native.pending_operations()?;
        let [operation] = operations.as_slice() else {
            return if operations.is_empty() {
                Ok(false)
            } else {
                Err(unresolved_restore())
            };
        };
        if operation.local {
            return Err(unresolved_restore());
        }
        let pending = parent(self.store)
            .join("index/pending")
            .join(format!("{}.json", operation.uuid));
        let record: NativePending =
            serde_json::from_reader(read_private(&pending)?.take(64 * 1024 * 1024))
                .map_err(|_| unresolved_restore())?;
        let native = record.checkpoint.operation.ok_or_else(unresolved_restore)?;
        if record.checkpoint.uuid != operation.uuid
            || native.id != operation.uuid
            || !self.owns_operation(attempt, rollback, &native)?
        {
            return Err(unresolved_restore());
        }
        let target = native.to.as_deref().or(native.applied.as_deref());
        if let Some(target) = target {
            if target != attempt.target {
                return Err(unresolved_restore());
            }
        } else {
            // Mise persists the invocation before recording its target.
            if record.recovery != NativeRecovery::Pending
                || native.status != "pending"
                || !native.journal.is_empty()
                || !native.affected.is_empty()
                || revision(self.native, self.store, MAIN_REF)? != attempt.head
                || revision(self.native, self.store, TRACKING_REF)? != attempt.target
                || require_unchanged(&attempt.live, &BTreeSet::new(), Some(&self.paths)).is_err()
            {
                return Err(unresolved_restore());
            }
            self.verify(&attempt.target)?;
        }
        self.paths.check_binding(binding, target.is_some())?;
        let affected = native.written_paths(attempt)?;
        for path in self.paths.created_directories(binding) {
            if !attempt.created.contains(path)
                || !affected.contains(path)
                || !fs::symlink_metadata(path)
                    .is_ok_and(|meta| meta.is_dir() && meta.mode() & 0o7777 == 0o700)
            {
                return Err(unresolved_restore());
            }
        }
        let completed = self.completion(attempt, rollback)?.is_some();
        if !completed {
            require_unchanged(&attempt.live, &affected, Some(&self.paths))?;
            if record.recovery != NativeRecovery::Pending {
                native.verify_completed_writes()?;
            }
        }
        let head = revision(self.native, self.store, MAIN_REF)?;
        self.verify(&head)?;
        verify_capture_policy(self.native)?;
        self.paths.recheck(self.native)?;
        if self.native.pending_operations()? != operations {
            return Err(unresolved_restore());
        }
        let command = self.command(&["bootstrap", "dotfiles", "recover", &operation.uuid], true)?;
        self.native.output(command, "native restore recovery needs attention; use dots mise dot history --pending to inspect the retained operation")?;
        self.native.require_no_pending_operations()?;
        if !completed && self.completion(attempt, rollback)?.is_none() {
            let current = tracked::restore_paths(self.native, Some(self.branch))?;
            current.check_binding(binding, false)?;
            require_unchanged(&attempt.live, &affected, Some(&current))?;
            for (path, previous) in &mut attempt.live {
                if affected.contains(path) {
                    *previous = observe_file(path, Some(&current))?;
                }
            }
            attempt.head = revision(self.native, self.store, MAIN_REF)?;
        }
        Ok(true)
    }

    fn verify_rollback<'p>(
        &self,
        target: &str,
        paths: impl Iterator<Item = &'p PathBuf>,
    ) -> Result<BTreeSet<PathBuf>, Fail> {
        let allowed = self.verify(target)?;
        let mut directories = BTreeSet::new();
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
            if !allowed.paths.contains(path)
                || !covered.contains(path)
                || unverified.iter().any(|entry| {
                    covered_path(&home, entry).is_none_or(|entry| path.starts_with(entry))
                })
            {
                return Err(invalid(
                    "first restore includes a path without complete encrypted checkpoint coverage",
                ));
            }
            directories.extend(
                allowed
                    .missing
                    .iter()
                    .filter(|parent| path.starts_with(parent))
                    .cloned(),
            );
        }
        self.paths.recheck(self.native)?;
        Ok(directories)
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
    id: String,
    kind: String,
    status: String,
    argv: Vec<String>,
    cwd: PathBuf,
    to: Option<String>,
    applied: Option<String>,
    #[serde(default)]
    affected: Vec<String>,
    #[serde(default)]
    journal: Vec<NativeWrite>,
}

#[derive(Deserialize)]
struct NativePending {
    checkpoint: PendingCheckpoint,
    recovery: NativeRecovery,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum NativeRecovery {
    Pending,
    UnfinishedWrites,
    Finished,
}

#[derive(Deserialize)]
struct PendingCheckpoint {
    uuid: String,
    operation: Option<NativeOperation>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum NativeWrite {
    Note,
    PathChanged {
        path: PathBuf,
        prior: NativePreimage,
    },
    Committed {
        seq: usize,
        after: NativeAfter,
    },
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum NativePreimage {
    Missing,
    File { mode: u32 },
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum NativeAfter {
    Missing,
    File { mode: u32, sha256: String },
    Dir { mode: u32, identity: (u64, u64) },
}

impl NativeOperation {
    fn written_paths(&self, attempt: &Attempt) -> Result<BTreeSet<PathBuf>, Fail> {
        let mut paths = BTreeSet::new();
        for write in &self.journal {
            match write {
                NativeWrite::Note => {}
                NativeWrite::PathChanged { path, prior } => {
                    let safe = match (attempt.live.get(path), prior) {
                        (Some(None), NativePreimage::Missing) => true,
                        (Some(Some(previous)), NativePreimage::File { mode }) => {
                            previous.stamp.mode & 0o7777 == 0o600 && *mode == 0o600
                        }
                        (None, NativePreimage::Missing) => attempt.created.contains(path),
                        _ => false,
                    };
                    if !safe || !paths.insert(path.clone()) {
                        return Err(unresolved_restore());
                    }
                }
                NativeWrite::Committed { seq, .. } => {
                    if !matches!(
                        self.journal.get(*seq),
                        Some(NativeWrite::PathChanged { .. })
                    ) {
                        return Err(unresolved_restore());
                    }
                }
            }
        }
        Ok(paths)
    }

    fn verify_completed_writes(&self) -> Result<(), Fail> {
        for (index, write) in self.journal.iter().enumerate() {
            let NativeWrite::PathChanged { path, .. } = write else {
                continue;
            };
            let after = self.journal.iter().rev().find_map(|entry| match entry {
                NativeWrite::Committed { seq, after } if *seq == index => Some(after),
                _ => None,
            });
            let Some(after) = after else {
                return Err(unresolved_restore());
            };
            if !after.still_matches(path)? {
                return Err(unresolved_restore());
            }
        }
        Ok(())
    }
}

impl NativeAfter {
    fn still_matches(&self, path: &Path) -> Result<bool, Fail> {
        match self {
            Self::Missing => Ok(!exists(path)?),
            Self::Dir { mode, identity } => Ok(fs::symlink_metadata(path).is_ok_and(|meta| {
                meta.is_dir()
                    && *mode == 0o700
                    && meta.mode() & 0o7777 == *mode
                    && (meta.dev(), meta.ino()) == *identity
            })),
            Self::File { mode, sha256 } => {
                let version = file_version(path)?.ok_or_else(unresolved_restore)?;
                if *mode != 0o600 || version.mode & 0o7777 != *mode {
                    return Ok(false);
                }
                let hash =
                    read_digest(&mut read_private(path)?).map_err(|_| unresolved_restore())?;
                let matching = sha256.len() == 2 * hash.len()
                    && hash.iter().enumerate().all(|(offset, byte)| {
                        sha256
                            .get(offset * 2..offset * 2 + 2)
                            .and_then(|digits| u8::from_str_radix(digits, 16).ok())
                            == Some(*byte)
                    });
                Ok(matching && file_version(path)? == Some(version))
            }
        }
    }
}

fn unresolved_restore() -> Fail {
    blocked(
        "the interrupted restore could not be verified; inspect the retained native operation with dots mise dot history --pending before retrying",
    )
}

fn covered_path(home: &Path, path: &str) -> Option<PathBuf> {
    home_path(home, path.strip_prefix("~/")?)
}

impl FileVersion {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            mode: metadata.mode(),
        }
    }

    fn digest(&self, path: &Path, paths: &tracked::TrackedPaths) -> Result<String, Fail> {
        let mut file = paths.open_file(path)?.ok_or_else(changed_live_file)?;
        let matches = |metadata: &fs::Metadata| {
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.mode() & 0o7777 == 0o600
                && Self::from_metadata(metadata) == *self
        };
        if !file.metadata().is_ok_and(|metadata| matches(&metadata)) {
            return Err(changed_live_file());
        }
        let hash = read_digest(&mut file).map_err(|_| changed_live_file())?;
        if !file.metadata().is_ok_and(|metadata| matches(&metadata))
            || file_version(path)?.as_ref() != Some(self)
        {
            return Err(changed_live_file());
        }
        Ok(hash.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

fn read_digest(file: &mut File) -> std::io::Result<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(digest.finalize().into());
        }
        digest.update(&buffer[..count]);
    }
}

fn file_version(path: &Path) -> Result<Option<FileVersion>, Fail> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(FileVersion::from_metadata(&metadata))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        _ => Err(unsafe_tree()),
    }
}

fn observe_file(
    path: &Path,
    digest_paths: Option<&tracked::TrackedPaths>,
) -> Result<Option<ObservedFile>, Fail> {
    let Some(stamp) = file_version(path)? else {
        return Ok(None);
    };
    let sha256 = digest_paths
        .map(|paths| stamp.digest(path, paths))
        .transpose()?;
    Ok(Some(ObservedFile { stamp, sha256 }))
}

fn live_versions(
    paths: &[PathBuf],
    digest_paths: Option<&tracked::TrackedPaths>,
) -> Result<LiveVersions, Fail> {
    paths
        .iter()
        .map(|path| Ok((path.clone(), observe_file(path, digest_paths)?)))
        .collect()
}

fn require_unchanged(
    live: &LiveVersions,
    written: &BTreeSet<PathBuf>,
    digest_paths: Option<&tracked::TrackedPaths>,
) -> Result<(), Fail> {
    for (path, previous) in live {
        if written.contains(path) {
            continue;
        }
        let current = file_version(path)?;
        if current.as_ref() == previous.as_ref().map(|file| &file.stamp) {
            continue;
        }
        // Stamp-only journals cannot establish that changed stamps still
        // describe the original bytes. Never retrofit a digest from live data.
        if let (Some(paths), Some(current), Some(previous)) = (digest_paths, current, previous)
            && current.mode == previous.stamp.mode
            && let Some(expected) = &previous.sha256
            && current.digest(path, paths)? == *expected
        {
            continue;
        }
        return Err(changed_live_file());
    }
    Ok(())
}

fn changed_live_file() -> Fail {
    blocked(
        "a tracked file changed after restore started; inspect the live file and any pending restore before retrying",
    )
}

fn prepare(
    native: &Native<'_, '_>,
    store: &Path,
    raw: &str,
    branch: &str,
    vars: &Vars,
    paths: &tracked::TrackedPaths,
    old: Option<fs::Metadata>,
) -> Result<Journal, Fail> {
    let base = parent(store);
    let temp = tempfile::Builder::new()
        .prefix(".dots-restore-reservation-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(base)
        .map_err(|_| blocked("could not stage a private history clone"))?;
    let clone = tempfile::Builder::new()
        .prefix(CLONE)
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(temp.path())
        .map_err(|_| blocked("could not stage a private history repository"))?;
    let clone_metadata = existing_directory(clone.path())?
        .ok_or_else(|| blocked("the staged history repository vanished"))?;
    let clone = StoreDirectory {
        directory: clone
            .keep()
            .file_name()
            .expect("clone basename")
            .to_string_lossy()
            .into_owned(),
        device: clone_metadata.dev(),
        inode: clone_metadata.ino(),
    };
    let clone_path = clone_path(&base.join(STAGING), &clone.directory)?;
    // Register the private slot before Git writes any clone data there.
    let owner = new_journal(
        native,
        temp.path(),
        raw,
        branch,
        paths,
        String::new(),
        true,
        Progress::Cloning(clone.clone()),
        None,
    )?;
    write_journal(temp.path(), &owner)?;
    let staging = base.join(STAGING);
    if exists(&staging)? {
        return Err(blocked(
            "the pending history install location already exists",
        ));
    }
    fs::rename(temp.path(), &staging)
        .map_err(|_| blocked("could not reserve the pending history install location"))?;
    fsync_dir(base)?;
    // A Git process left behind by a killed restore cannot reuse this name.
    let seed = match fetch_seed(native, &clone_path, raw, branch, vars, paths) {
        Ok(seed) => seed,
        Err(error) => {
            discard_clone(&staging, &clone)?;
            return Err(error);
        }
    };

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
        (Some(old), Some(backup)) => Some(StoreDirectory {
            directory: backup
                .path()
                .file_name()
                .expect("backup basename")
                .to_string_lossy()
                .into_owned(),
            device: old.dev(),
            inode: old.ino(),
        }),
        _ => None,
    };
    let record = new_journal(
        native,
        &clone_path,
        raw,
        branch,
        paths,
        seed,
        true,
        Progress::Prepared,
        previous,
    )?;
    if let Some(backup) = backup {
        let _ = backup.keep();
    }
    write_journal(&clone_path, &record)?;
    fs::rename(clone_path, staging.join("repo.git"))
        .map_err(|_| blocked("could not commit the staged history clone for install"))?;
    fsync_dir(&staging)?;
    native.require_no_pending_operations()?;
    paths.recheck(native)?;
    install(&staging, store, &record, true)?;
    Ok(record)
}

fn fetch_seed(
    native: &Native<'_, '_>,
    staged: &Path,
    raw: &str,
    branch: &str,
    vars: &Vars,
    paths: &tracked::TrackedPaths,
) -> Result<String, Fail> {
    private_dir(staged).map_err(|_| blocked("could not stage a private history repository"))?;
    let mut cmd = native.git(Some(staged))?;
    cmd.args(["init", "--bare", "--initial-branch=main"]);
    native.output(cmd, "could not initialize the staged bare history clone")?;
    let mut cmd = native.git(Some(staged))?;
    cmd.args(["config", "--local", "remote.origin.url", raw]);
    native.output(cmd, "could not configure the staged history origin")?;
    native.destinations(Some(staged), raw, state_repo(vars)?, vars)?;
    let mut cmd = native.git(Some(staged))?;
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
    let live = live_versions(paths, None)?;
    native.output(
        cmd,
        "could not stage the selected remote history branch; the existing store was left unchanged",
    )?;
    require_unchanged(&live, &BTreeSet::new(), None)?;
    native.bare(staged)?;
    let seed = revision(native, staged, MAIN_REF)?;
    verify_tree(native, staged, &seed, branch, paths)?;
    fs::set_permissions(staged, Permissions::from_mode(0o700))
        .map_err(|_| blocked("could not protect the staged history store"))?;
    Ok(seed)
}

#[allow(clippy::too_many_arguments)]
fn new_journal(
    native: &Native<'_, '_>,
    store: &Path,
    raw: &str,
    branch: &str,
    paths: &tracked::TrackedPaths,
    seed: String,
    initial: bool,
    progress: Progress,
    previous: Option<StoreDirectory>,
) -> Result<Journal, Fail> {
    let metadata = existing_directory(store)?.ok_or_else(|| blocked("history store vanished"))?;
    Ok(Journal {
        root: native.ctx.root.path().to_owned(),
        home: physical_home(native)?,
        paths: paths.binding().clone(),
        origin: raw.to_owned(),
        branch: branch.to_owned(),
        device: metadata.dev(),
        inode: metadata.ino(),
        seed,
        initial,
        progress,
        obsolete_scope: None,
        previous,
    })
}

fn install(staging: &Path, store: &Path, record: &Journal, owned_stage: bool) -> Result<(), Fail> {
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
        match (existing_directory(store)?, existing_directory(&backup)?) {
            (Some(old), None) if previous.matches(&old) => {
                fs::rename(store, &backup)
                    .map_err(|_| blocked("could not preserve the previous history store"))?;
                fsync_dir(&dir)?;
                fsync_dir(base)?;
            }
            (None, Some(old)) if previous.matches(&old) => {}
            _ => {
                return Err(invalid(
                    "the original history store changed; inspect the pending install before retrying",
                ));
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
    finish_staging(staging, owned_stage)
}

fn clone_path(staging: &Path, name: &str) -> Result<PathBuf, Fail> {
    if !name.strip_prefix(CLONE).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return Err(invalid("invalid history clone name in pending staging"));
    }
    Ok(staging.join(name))
}

fn discard_clone(staging: &Path, clone: &StoreDirectory) -> Result<(), Fail> {
    let repo = clone_path(staging, &clone.directory)?;
    if let Some(meta) = existing_directory(&repo)? {
        if !clone.matches(&meta) {
            return Err(invalid(
                "the unfinished history clone changed since it was staged",
            ));
        }
        fs::remove_dir_all(repo)
            .map_err(|_| blocked("could not discard the unfinished history clone"))?;
    }
    finish_staging(staging, true)
}

fn finish_staging(staging: &Path, owned_stage: bool) -> Result<(), Fail> {
    if !owned_stage {
        fs::remove_dir(staging)
            .map_err(|_| blocked("could not finish the history install staging directory"))?;
        return fsync_dir(parent(staging));
    }
    let retirement = parent(staging).join(RETIRING);
    // Keep the original name until the sibling link is durable.
    fs::hard_link(staging.join(MARKER), &retirement)
        .map_err(|_| blocked("could not create the history clone retirement marker"))?;
    finish_retirement(staging, &retirement, &read_private(&retirement)?)
}

fn resume_staging_retirement(
    native: &Native<'_, '_>,
    staging: &Path,
    origin: &str,
    branch: &str,
    paths: &tracked::TrackedPaths,
) -> Result<(), Fail> {
    let retirement = parent(staging).join(RETIRING);
    if !exists(&retirement)? {
        return Ok(());
    }
    let receipt = read_private(&retirement)?;
    let owner = parse_journal(&receipt)?;
    if !matches!(owner.progress, Progress::Cloning(_)) {
        return Err(invalid(
            "invalid ownership marker for a retired history clone",
        ));
    }
    match_journal_context(native, &owner, origin, branch)?;
    paths.check_binding(&owner.paths, false)?;
    if let Some(meta) = existing_directory(staging)? {
        match_journal_directory(&owner, &meta)?;
    }
    finish_retirement(staging, &retirement, &receipt)
}

fn finish_retirement(staging: &Path, retirement: &Path, receipt: &File) -> Result<(), Fail> {
    let base = parent(staging);
    if exists(staging)? {
        fsync_dir(base)?;
        let original = staging.join(MARKER);
        if exists(&original)? {
            let staged = fs::symlink_metadata(&original)
                .map_err(|_| blocked("could not inspect the history clone ownership marker"))?;
            let retired = receipt
                .metadata()
                .map_err(|_| blocked("could not inspect the history clone retirement marker"))?;
            if !staged.is_file() || (staged.dev(), staged.ino()) != (retired.dev(), retired.ino()) {
                return Err(invalid("the history clone ownership markers do not match"));
            }
            fs::remove_file(original)
                .map_err(|_| blocked("could not clear the history clone ownership marker"))?;
        }
        fsync_dir(staging)?;
        fs::remove_dir(staging)
            .map_err(|_| blocked("could not finish the history install staging directory"))?;
    }
    fsync_dir(base)?;
    fs::remove_file(retirement)
        .map_err(|_| blocked("could not clear the history clone retirement marker"))?;
    fsync_dir(base)
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
    parse_journal(&read_private(&store.join(MARKER))?)
}

fn clear_journal(store: &Path) -> Result<(), Fail> {
    fs::remove_file(store.join(MARKER))
        .map_err(|_| blocked("could not finish the pending restore marker"))?;
    fsync_dir(store)
}

#[cfg(test)]
#[test]
fn journal_round_trips_above_four_mib() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path();
    let home = PathBuf::from("/fixture/home");
    let metadata = fs::metadata(store).unwrap();
    let live: LiveVersions = (0..32_000)
        .map(|index| {
            let version = FileVersion {
                device: metadata.dev(),
                inode: index,
                size: 1024,
                modified: (1_720_000_000, 123_000_000),
                changed: (1_720_000_001, 456_000_000),
                mode: 0o100600,
            };
            (
                home.join(format!("settings/target-{index}.toml")),
                Some(ObservedFile {
                    stamp: version,
                    sha256: None,
                }),
            )
        })
        .collect();
    let paths = serde_json::from_value(serde_json::json!({
        "home": home,
        "source": null,
        "targets": live.keys().collect::<Vec<_>>(),
        "directories": {},
    }))
    .unwrap();
    let journal = Journal {
        root: home.join("config"),
        home,
        paths,
        origin: "file:///fixture/origin.git".into(),
        branch: "host-fixture".into(),
        device: metadata.dev(),
        inode: metadata.ino(),
        seed: "1".repeat(40),
        initial: false,
        progress: Progress::Fetching(live.clone()),
        obsolete_scope: None,
        previous: None,
    };
    write_journal(store, &journal).unwrap();
    assert!(fs::metadata(store.join(MARKER)).unwrap().len() > 4 * 1024 * 1024);
    let read = read_journal(store).unwrap();
    assert!(matches!(read.progress, Progress::Fetching(observed) if observed == live));
    assert!(is_established_restore(store, true));
}

pub(super) fn is_established_restore(store: &Path, fetching: bool) -> bool {
    let Ok(Some(metadata)) = existing_directory(store) else {
        return false;
    };
    read_journal(store).is_ok_and(|record| {
        !record.initial
            && matches!(record.progress, Progress::Fetching(_)) == fetching
            && (fetching || matches!(record.progress, Progress::Pulling(_)))
            && (fetching || matches!(exists(&parent(store).join(STAGING)), Ok(false)))
            && record.obsolete_scope.is_none()
            && match_journal_directory(&record, &metadata).is_ok()
    })
}

fn parse_journal(file: &File) -> Result<Journal, Fail> {
    serde_json::from_reader(BufReader::new(file))
        .map_err(|_| invalid("could not validate the pending history recovery marker"))
}

fn read_private(path: &Path) -> Result<File, Fail> {
    read_native_file(path).map_err(|_| invalid("could not read local history metadata"))
}

fn match_request(
    native: &Native<'_, '_>,
    store: &Path,
    record: &Journal,
    origin: &str,
    branch: &str,
) -> Result<(), Fail> {
    match_journal_owner(native, store, record, origin, branch)?;
    let head = revision(native, store, MAIN_REF)?;
    if !ancestor(native, store, &record.seed, &head)? {
        return Err(invalid(
            "pending history store no longer contains its seed checkpoint",
        ));
    }
    Ok(())
}

fn match_journal_owner(
    native: &Native<'_, '_>,
    store: &Path,
    record: &Journal,
    origin: &str,
    branch: &str,
) -> Result<(), Fail> {
    match_journal_context(native, record, origin, branch)?;
    let metadata =
        existing_directory(store)?.ok_or_else(|| blocked("pending history store vanished"))?;
    match_journal_directory(record, &metadata)
}

fn match_journal_context(
    native: &Native<'_, '_>,
    record: &Journal,
    origin: &str,
    branch: &str,
) -> Result<(), Fail> {
    let root = native.ctx.root.path();
    if record.root != root
        || record.home != physical_home(native)?
        || record.origin != origin
        || record.branch != branch
    {
        return Err(invalid(
            "pending history restore belongs to another configuration root, origin or branch",
        ));
    }
    Ok(())
}

fn match_journal_directory(record: &Journal, metadata: &fs::Metadata) -> Result<(), Fail> {
    if (record.device, record.inode) != (metadata.dev(), metadata.ino()) {
        return Err(invalid(
            "pending history restore belongs to a different store",
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
    unmodified_git_graph(&mut command)
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0");
    Ok(command)
}

pub(super) fn unmodified_git_graph(command: &mut Command) -> &mut Command {
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
}

#[derive(Deserialize)]
struct Manifest {
    enrollment: Vec<Enrollment>,
    permissions: BTreeMap<String, u32>,
}

struct VerifiedTree {
    paths: BTreeSet<PathBuf>,
    missing: BTreeSet<PathBuf>,
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
) -> Result<VerifiedTree, Fail> {
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
            && (!entry.encrypt
                || path.starts_with(native.ctx.root.path())
                || !paths.contains(&path)
                || !enrolled.insert(path))
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
        check_envelope(native, store, oid, path)?;
        for directory in live.ancestors().skip(1).take_while(|dir| *dir != home) {
            if !exists(directory)? {
                let relative = directory.strip_prefix(&home).map_err(|_| unsafe_tree())?;
                let name = format!("home/{}", relative.display());
                if manifest.permissions.get(&name) != Some(&0o700) {
                    return Err(unsafe_tree());
                }
            }
        }
    }
    let mut missing = BTreeSet::new();
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
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && *mode == 0o700 => {
                    missing.insert(live);
                }
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
    Ok(VerifiedTree {
        paths: enrolled,
        missing,
    })
}

fn check_envelope(
    native: &Native<'_, '_>,
    store: &Path,
    oid: &str,
    path: &str,
) -> Result<(), Fail> {
    let mut cmd = local_git(native, store)?;
    cmd.args(["cat-file", "blob", oid]);
    let blob = native.output(cmd, "could not inspect the encrypted checkpoint envelope")?;
    if !regular_envelope(&blob, path) {
        return Err(unsafe_tree());
    }
    Ok(())
}

fn regular_envelope(blob: &[u8], expected_path: &str) -> bool {
    // Mise's v1 named MessagePack envelope exposes the path and Git mode.
    // Native decrypt verifies these against the sealed fields before writing.
    let Some(mut body) = blob.strip_prefix(b"mise-encrypted-file-v1\n\x84") else {
        return false;
    };
    msgpack_string(&mut body) == Some("path")
        && msgpack_string(&mut body) == Some(expected_path)
        && msgpack_string(&mut body) == Some("mode")
        && msgpack_string(&mut body) == Some("100644")
        && msgpack_string(&mut body) == Some("scheme")
        && msgpack_string(&mut body).is_some_and(|scheme| !scheme.is_empty())
        && msgpack_string(&mut body) == Some("ciphertext")
        && msgpack_bytes(&mut body, false).is_some_and(|bytes| !bytes.is_empty())
        && body.is_empty()
}

fn msgpack_string<'a>(input: &mut &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(msgpack_bytes(input, true)?).ok()
}

fn msgpack_bytes<'a>(input: &mut &'a [u8], string: bool) -> Option<&'a [u8]> {
    let (marker, rest) = input.split_first()?;
    let (length, remaining) = match (string, marker) {
        (true, 0xa0..=0xbf) => (u32::from(marker & 0x1f), rest),
        (true, 0xd9) | (false, 0xc4) => (u32::from(*rest.first()?), rest.get(1..)?),
        (true, 0xda) | (false, 0xc5) => (
            u32::from(u16::from_be_bytes(rest.get(..2)?.try_into().ok()?)),
            rest.get(2..)?,
        ),
        (true, 0xdb) | (false, 0xc6) => (
            u32::from_be_bytes(rest.get(..4)?.try_into().ok()?),
            rest.get(4..)?,
        ),
        _ => return None,
    };
    let (value, rest) = remaining.split_at_checked(length as usize)?;
    *input = rest;
    Some(value)
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
    relative
        .split('/')
        .all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.eq_ignore_ascii_case(".git")
                && !part.contains(['\\', ':'])
                && !part.chars().any(char::is_control)
        })
        .then(|| home.join(relative))
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
