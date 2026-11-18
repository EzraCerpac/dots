use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use nix::libc;
use serde::{Deserialize, Serialize};

use crate::Fail;

use super::checkpoint::{MARKER, STAGING, legacy_marker};
use crate::config::{RepoSetting, Vars};

use super::keys::{self, LocalSettings};
use super::native::{Native, exists, private_dir};
use super::{Restore, blocked, host, invalid, state_repo, tracked, validate_origin};

const PREVIOUS: &str = "repo.git.pre-restore.";
const TRACKING_REF: &str = "refs/remotes/origin/setup";

#[derive(Deserialize, Serialize)]
struct Journal {
    origin: String,
    branch: String,
    previous: Option<Previous>,
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
    let vars = Vars::read(&native.ctx.root)?;
    let expected = state_repo(&vars)?;
    validate_origin(native, &vars, origin)?;
    let (identity, recovered_keys) = keys::read_identity(native, identity)?;
    let local = LocalSettings::read(native)?;
    let mut recipients = local.recipients()?;
    recipients.extend(recovered_keys);
    keys::check_recipients(native, &recipients)?;
    let paths = tracked::paths(native, Some(&branch))?;
    let store = native.store()?;
    let raw = origin.transport_url();
    let _lock = super::native::lock(&store)?;

    let staging = parent(&store).join(STAGING);
    if exists(&staging)? {
        if !fs::symlink_metadata(&staging).is_ok_and(|meta| meta.is_dir()) {
            return Err(invalid(
                "pending history install location is not a real directory",
            ));
        }
        let staged = staging.join("repo.git");
        if exists(&staged)? {
            let record = read_journal(&staged)?;
            match_request(&record, &raw, &branch)?;
            native.bare(&staged)?;
            native.destinations(Some(&staged), &raw, expected, &vars)?;
            verify_manifest(native, &staged, "HEAD", &branch, &paths)?;
            install(&staging, &store, &record)?;
        } else if exists(&store.join(MARKER))? {
            fs::remove_dir(&staging)
                .map_err(|_| blocked("could not finish pending history installation"))?;
        } else {
            return Err(invalid(
                "incomplete restore staging needs inspection before retrying",
            ));
        }
    }

    let pending = if exists(&store.join(MARKER))? {
        let record = read_journal(&store)?;
        match_request(&record, &raw, &branch)?;
        native.bare(&store)?;
        true
    } else if exists(&legacy_marker(&store))? {
        native.bare(&store)?;
        let prior = read_marker(&legacy_marker(&store))?;
        if prior.trim_end() != raw {
            return Err(invalid(
                "pending first restore names a different history origin",
            ));
        }
        true
    } else {
        let old = existing_directory(&store)?;
        let connected = old.is_some()
            && native.bare(&store).is_ok()
            && native
                .local_origin(&store)?
                .is_some_and(|url| expected.matches(&url));
        if connected {
            false
        } else {
            if old.is_some() && !initialize {
                return Err(invalid(
                    "local history is connected elsewhere; inspect it before using dots restore --initialize-history",
                ));
            }
            prepare(native, &store, &raw, &branch, expected, &vars, &paths, old)?;
            true
        }
    };

    native.destinations(Some(&store), &raw, expected, &vars)?;
    verify_manifest(native, &store, "HEAD", &branch, &paths)?;

    let make_command = |args: &[&str]| {
        let mut cmd = native.mise(args)?;
        cmd.env("MISE_HISTORY_SYNC", "fetch-only")
            .env("MISE_AGE_IDENTITY_FILES", &identity);
        Ok::<_, Fail>(cmd)
    };
    let cmd = make_command(&[
        "--yes",
        "bootstrap",
        "dotfiles",
        "origin",
        "set",
        &raw,
        "--branch",
        &branch,
        "--sync",
        "fetch-only",
    ])?;
    apply(
        native,
        cmd,
        "mise could not connect and fetch encrypted history",
    )?;
    let fetched = revision(native, &store, TRACKING_REF)?;
    verify_manifest(native, &store, &fetched, &branch, &paths)?;
    let status = make_command(&["bootstrap", "dotfiles", "status"])?;
    let status = native.text(status, "could not inspect the fetched private settings")?;
    let cmd = make_command(&["--yes", "bootstrap", "dotfiles", "pull"])?;
    let diagnostic = if status.contains("both sides changed the same lines") {
        "mise could not restore private settings: both sides changed the same lines; resolve the native history conflict before retrying"
    } else {
        "mise could not apply incoming private settings"
    };
    apply(native, cmd, diagnostic)?;
    if pending {
        // Native rollback keeps its encrypted undo history. Dots never
        // copies plaintext targets or invokes save/sync during restore.
        let target = format!("commit:{fetched}");
        for mode in ["--dry-run", "--yes"] {
            let cmd = make_command(&[
                "bootstrap",
                "dotfiles",
                "rollback",
                "--to",
                &target,
                "--all",
                mode,
            ])?;
            apply(
                native,
                cmd,
                "mise could not apply the first private history checkpoint",
            )?;
        }
    }

    let current = tracked::paths(native, Some(&branch))?;
    tracked::secure(&current)?;
    let mut local = LocalSettings::read(native)?;
    local.set_history(&branch, origin, &identity, &recipients)?;
    local.install(native)?;
    if pending {
        for marker in [store.join(MARKER), legacy_marker(&store)] {
            if exists(&marker)? {
                fs::remove_file(&marker)
                    .map_err(|_| blocked("could not finish the pending restore marker"))?;
            }
        }
    }
    Ok(())
}

fn prepare(
    native: &Native<'_, '_>,
    store: &Path,
    raw: &str,
    branch: &str,
    expected: &RepoSetting,
    vars: &Vars,
    paths: &[PathBuf],
    old: Option<fs::Metadata>,
) -> Result<(), Fail> {
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
    native.destinations(Some(&staged), raw, expected, vars)?;
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
    native.output(
        cmd,
        "could not stage the selected remote history branch; the existing store was left unchanged",
    )?;
    native.bare(&staged)?;
    verify_manifest(native, &staged, "HEAD", branch, paths)?;
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
    let record = Journal {
        origin: raw.to_owned(),
        branch: branch.to_owned(),
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
    install(&staging, store, &record)
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
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(store.join(MARKER))
        .map_err(|_| blocked("could not record pending history recovery"))?;
    file.write_all(&data)
        .and_then(|()| file.sync_all())
        .map_err(|_| blocked("could not persist the pending history recovery"))?;
    fsync_dir(store)
}

fn read_journal(store: &Path) -> Result<Journal, Fail> {
    serde_json::from_str(&read_marker(&store.join(MARKER))?)
        .map_err(|_| invalid("could not validate the pending history recovery marker"))
}

fn read_marker(path: &Path) -> Result<String, Fail> {
    let mut value = String::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .and_then(|file| {
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("not a regular marker"));
            }
            file.take(8192).read_to_string(&mut value)
        })
        .map_err(|_| invalid("could not read the pending history marker"))?;
    Ok(value)
}

fn match_request(record: &Journal, origin: &str, branch: &str) -> Result<(), Fail> {
    if record.origin != origin || record.branch != branch {
        return Err(invalid(
            "pending history install names another origin or branch",
        ));
    }
    Ok(())
}

fn revision(native: &Native<'_, '_>, store: &Path, reference: &str) -> Result<String, Fail> {
    let mut cmd = native.git(Some(store))?;
    let commit = format!("{reference}^{{commit}}");
    cmd.args(["rev-parse", "--verify", &commit]);
    let id = native.text(
        cmd,
        "could not resolve the fetched private history checkpoint",
    )?;
    let id = id.trim_end();
    if !matches!(id.len(), 40 | 64) || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(blocked("Git returned an invalid private history commit ID"));
    }
    Ok(id.to_owned())
}

#[derive(Deserialize)]
struct Manifest {
    enrollment: Vec<Enrollment>,
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
}

fn verify_manifest(
    native: &Native<'_, '_>,
    store: &Path,
    reference: &str,
    branch: &str,
    paths: &[PathBuf],
) -> Result<(), Fail> {
    let commit = revision(native, store, reference)?;
    let mut cmd = native.git(Some(store))?;
    cmd.args(["show", &format!("{commit}:.mise-history/manifest.json")]);
    let data = native.output(cmd, "could not read the native history checkpoint manifest")?;
    let manifest: Manifest = serde_json::from_slice(&data)
        .map_err(|_| invalid("invalid encrypted history checkpoint manifest"))?;
    let home = native
        .home()?
        .canonicalize()
        .map_err(|_| invalid("could not resolve HOME"))?;
    for entry in manifest.enrollment {
        let ours = entry
            .variants
            .iter()
            .any(|variant| variant.profile.as_deref() == Some(branch));
        if !ours
            && !entry.variants.is_empty()
            && entry.variants.iter().all(|variant| {
                variant
                    .profile
                    .as_deref()
                    .is_some_and(|profile| profile.starts_with("host-"))
            })
        {
            continue;
        }
        let safe_entry = entry
            .path
            .strip_prefix("home")
            .ok()
            .is_some_and(|path| paths.contains(&home.join(path)));
        if !ours || !entry.encrypt || !safe_entry {
            return Err(invalid(
                "fetched history includes a path without an active encrypted host declaration",
            ));
        }
    }
    Ok(())
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
        Err(blocked(
            "mise could not restore private settings: both sides changed the same lines; resolve the native history conflict before retrying",
        ))
    } else {
        Err(blocked(message))
    }
}
