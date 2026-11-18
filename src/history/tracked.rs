use std::collections::{BTreeSet, HashMap};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, Permissions};
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};

use nix::libc;
use serde::Deserialize;
use toml_edit::DocumentMut;

use crate::Fail;

use super::native::{Native, exists, expand};
use super::{blocked, invalid};

#[derive(Deserialize)]
struct Inventory {
    entries: Vec<Entry>,
    invalid: Vec<InvalidEntry>,
}

#[derive(Deserialize)]
struct InvalidEntry {
    path: PathBuf,
}

#[derive(Deserialize)]
struct Entry {
    mode: String,
    path: PathBuf,
    variant: Option<String>,
    declared_in: Option<PathBuf>,
}

fn inventory(native: &Native<'_, '_>, local: bool) -> Result<Inventory, Fail> {
    let mut args = vec!["bootstrap", "dotfiles"];
    if local {
        args.push("--local");
    }
    args.extend(["paths", "--json"]);
    let mut cmd = native.mise(&args)?;
    cmd.env_remove("__MISE_HISTORY_LOCAL")
        .env("MISE_HISTORY_SYNC", "manual")
        .env("GIT_ALLOW_PROTOCOL", "");
    let inventory: Inventory =
        serde_json::from_slice(&native.output(cmd, "could not read native tracked history paths")?)
            .map_err(|_| invalid("mise returned an invalid tracked history inventory"))?;
    let unproved = if local || inventory.invalid.is_empty() {
        inventory.invalid.first()
    } else {
        let inactive = inactive_invalid_paths(native, &inventory.invalid)?;
        inventory
            .invalid
            .iter()
            .find(|entry| !inactive.contains(&entry.path))
    };
    if let Some(entry) = unproved {
        return Err(invalid(format!(
            "mise reports an invalid tracked history entry: {}",
            entry.path.display()
        )));
    }
    Ok(inventory)
}

#[derive(Deserialize)]
struct Status {
    files: Vec<StatusEntry>,
}

#[derive(Deserialize)]
struct StatusEntry {
    target: PathBuf,
    mode: String,
    reason: Option<String>,
}

fn inactive_invalid_paths(
    native: &Native<'_, '_>,
    invalid_rows: &[InvalidEntry],
) -> Result<BTreeSet<PathBuf>, Fail> {
    // Native paths can report a fault before testing variants. Ask status
    // to select those targets without opening or initializing the store.
    let diagnostic = "could not verify variants of invalid tracked history entries";
    let mut cmd = native.mise(&["bootstrap", "dotfiles", "status", "--json", "--"])?;
    cmd.args(invalid_rows.iter().map(|entry| &entry.path))
        .env_remove("__MISE_HISTORY_LOCAL")
        .env("MISE_HISTORY_ENABLED", "false")
        .env("MISE_HISTORY_SYNC", "manual")
        .env("GIT_ALLOW_PROTOCOL", "");
    let output = native.probe(cmd, diagnostic)?;
    if !output.status.success() {
        // A malformed declaration can be absent from status completely.
        // Keep the paths command's configuration error in that case.
        return Ok(BTreeSet::new());
    }
    let status: Status = serde_json::from_slice(&output.stdout).map_err(|_| invalid(diagnostic))?;
    Ok(status
        .files
        .into_iter()
        .filter(|entry| {
            entry.mode == "track"
                && entry
                    .reason
                    .as_deref()
                    .and_then(|reason| reason.strip_prefix("no variant matches this machine: "))
                    .is_some_and(|variants| {
                        variants.split(", ").all(|variant| {
                            variant
                                .strip_prefix("host-")
                                .is_some_and(|id| super::host(id).is_ok())
                        })
                    })
        })
        .map(|entry| entry.target)
        .collect())
}

pub(super) fn capture_paths(
    native: &Native<'_, '_>,
    host: Option<&str>,
) -> Result<Vec<PathBuf>, Fail> {
    let paths = paths(native, host)?;
    if let Some(entry) = inventory(native, true)?.entries.first() {
        // mise 2026.11.9's unqualified save dispatches to both scopes.
        // Native track-local has no encrypted mode.
        return Err(invalid(format!(
            "cannot save encrypted settings while local-only history tracks {}",
            entry.path.display()
        )));
    }
    if !paths.is_empty() {
        validate_retained_policy(native, &paths)?;
    }
    Ok(paths)
}

pub(super) fn paths(native: &Native<'_, '_>, host: Option<&str>) -> Result<Vec<PathBuf>, Fail> {
    let inventory = inventory(native, false)?;
    let home = native.home()?;
    let physical_home = physical_home(home)?;
    let mut sources = HashMap::new();
    let mut paths = BTreeSet::new();

    for entry in inventory.entries {
        if entry.mode != "track" {
            return Err(invalid("mise returned an unknown tracked history mode"));
        }
        let target = target(home, &physical_home, &entry.path)?;
        let declared = entry.declared_in.ok_or_else(|| {
            invalid(format!(
                "history path {} has no active encrypted declaration",
                entry.path.display()
            ))
        })?;
        let declaring_file = expand(home, &declared)?;
        if !sources.contains_key(&declaring_file) {
            let doc = fs::read_to_string(&declaring_file)
                .ok()
                .and_then(|text| text.parse::<DocumentMut>().ok())
                .ok_or_else(|| {
                    invalid(format!(
                        "could not read dotfile declarations in {}",
                        declared.display()
                    ))
                })?;
            sources.insert(declaring_file.clone(), doc);
        }
        let dotfiles = sources[&declaring_file]
            .get("dotfiles")
            .and_then(|item| item.as_table_like())
            .ok_or_else(|| {
                invalid(format!(
                    "missing dotfile declarations in {}",
                    declared.display()
                ))
            })?;
        let mut found = false;
        for (spelling, declaration) in dotfiles.iter() {
            if lexical_target(home, &physical_home, Path::new(spelling))
                .as_ref()
                .ok()
                != Some(&target)
            {
                continue;
            }
            found = true;
            if !declaration
                .as_table_like()
                .and_then(|entry| entry.get("encrypt"))
                .and_then(|item| item.as_bool())
                .unwrap_or(false)
            {
                return Err(invalid(format!(
                    "history entry {} in {} must declare encrypt = true",
                    entry.path.display(),
                    declared.display()
                )));
            }
        }
        if !found {
            return Err(invalid(format!(
                "could not verify the encrypted history entry {} in {}",
                entry.path.display(),
                declared.display()
            )));
        }
        if let Some(host) = host
            && entry.variant.as_deref() != Some(host)
        {
            return Err(invalid(format!(
                "history entry {} in {} must be active through variant {host}",
                entry.path.display(),
                declared.display()
            )));
        }
        paths.insert(target);
    }
    Ok(paths.into_iter().collect())
}

#[derive(Deserialize)]
struct Manifest {
    enrollment: Vec<Enrollment>,
}

#[derive(Deserialize)]
struct Enrollment {
    path: PathBuf,
    encrypt: bool,
}

fn validate_retained_policy(native: &Native<'_, '_>, paths: &[PathBuf]) -> Result<(), Fail> {
    const REF: &str = "refs/heads/main";
    const FILE: &str = ".mise-history/manifest.json";

    let store = native.store()?;
    if !exists(&store)? {
        return Ok(());
    }
    native.bare(&store)?;
    let diagnostic = "could not verify native history's current enrollment";
    let git = || {
        let mut cmd = native.git(Some(&store))?;
        cmd.env("GIT_ALLOW_PROTOCOL", "");
        Ok::<_, Fail>(cmd)
    };
    let mut cmd = git()?;
    cmd.args(["rev-parse", "--verify", "--quiet", REF]);
    let output = native.probe(cmd, diagnostic)?;
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(());
    }
    let revision = std::str::from_utf8(&output.stdout)
        .ok()
        .map(str::trim_end)
        .filter(|oid| {
            output.status.success()
                && matches!(oid.len(), 40 | 64)
                && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .ok_or_else(|| invalid(diagnostic))?;
    let mut cmd = git()?;
    cmd.args(["ls-tree", "--name-only", revision, "--", FILE]);
    if native.output(cmd, diagnostic)?.is_empty() {
        return Ok(());
    }
    let mut cmd = git()?;
    cmd.args(["show", &format!("{revision}:{FILE}")]);
    let manifest: Manifest = serde_json::from_slice(&native.output(cmd, diagnostic)?)
        .map_err(|_| invalid(diagnostic))?;
    let home = physical_home(native.home()?)?;
    let config = native.ctx.root.path();
    // Native paths reports the selected stream but omits its effective
    // encryption policy. Mise can retain the current manifest's policy
    // even when the matching TOML declaration says encrypt = true.
    for entry in manifest.enrollment.iter().filter(|entry| !entry.encrypt) {
        let target = entry
            .path
            .strip_prefix("home")
            .map(|path| home.join(path))
            .or_else(|_| {
                entry
                    .path
                    .strip_prefix("config")
                    .map(|path| config.join(path))
            });
        if let Ok(target) = target
            && paths.contains(&target)
        {
            return Err(invalid(format!(
                "cannot verify encryption for {}: native history retains an unencrypted enrollment",
                target.display()
            )));
        }
    }
    Ok(())
}

fn lexical_target(home: &Path, physical_home: &Path, path: &Path) -> Result<PathBuf, Fail> {
    let absolute = expand(home, path)?;
    let relative = absolute
        .strip_prefix(home)
        .or_else(|_| absolute.strip_prefix(physical_home))
        .map_err(|_| invalid("a tracked history path lies outside HOME"))?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid(
            "a tracked history path has an invalid HOME-relative destination",
        ));
    }
    Ok(physical_home.join(relative))
}

fn target(home: &Path, physical_home: &Path, path: &Path) -> Result<PathBuf, Fail> {
    let target = lexical_target(home, physical_home, path)?;
    let mut cursor = target.as_path();
    while cursor != physical_home {
        match fs::symlink_metadata(cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "refusing symlink in tracked history path {}",
                    cursor.display()
                )));
            }
            Ok(meta) if cursor == target && !meta.is_file() => {
                return Err(invalid(format!(
                    "tracked history path is not a regular file: {}",
                    cursor.display()
                )));
            }
            Ok(meta) if cursor != target && !meta.is_dir() => {
                return Err(invalid(format!(
                    "tracked history path has a non-directory parent: {}",
                    cursor.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(blocked("could not inspect a tracked history destination")),
        }
        cursor = cursor.parent().expect("validated HOME ancestry");
    }
    Ok(target)
}

fn physical_home(home: &Path) -> Result<PathBuf, Fail> {
    home.canonicalize()
        .map_err(|_| invalid("could not resolve HOME"))
}

pub(super) fn secure(native: &Native<'_, '_>, paths: &[PathBuf]) -> Result<(), Fail> {
    if paths.is_empty() {
        return Ok(());
    }
    secure_paths(&physical_home(native.home()?)?, paths)
}

fn secure_paths(home: &Path, paths: &[PathBuf]) -> Result<(), Fail> {
    let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let relative_home = home
        .strip_prefix("/")
        .map_err(|_| invalid("HOME must name an absolute directory"))?;
    let root = open_at(libc::AT_FDCWD, c"/", flags)
        .and_then(|root| open_directories(&root, relative_home))
        .map_err(|_| blocked("could not safely open HOME to protect tracked history file modes"))?;
    let opened = paths
        .iter()
        .filter_map(|path| {
            let Ok(relative) = path.strip_prefix(home) else {
                return Some(Err(invalid("a tracked history path lies outside HOME")));
            };
            match open_target(&root, relative) {
                Ok(file) if file.metadata().is_ok_and(|meta| meta.is_file()) => Some(Ok(file)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                _ => Some(Err(blocked(
                    "could not safely open a tracked history file to protect its mode",
                ))),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    for file in opened {
        file.set_permissions(Permissions::from_mode(0o600))
            .map_err(|_| blocked("could not set tracked history file mode to 0600"))?;
    }
    Ok(())
}

fn open_target(root: &File, path: &Path) -> io::Result<File> {
    let (parent, name) = path
        .parent()
        .zip(path.file_name())
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let directory = open_directories(root, parent)?;
    let name = c_name(name)?;
    let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    open_at(directory.as_raw_fd(), &name, flags)
}

fn open_directories(root: &File, path: &Path) -> io::Result<File> {
    let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut directory = root.try_clone()?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        };
        directory = open_at(directory.as_raw_fd(), &c_name(name)?, flags)?;
    }
    Ok(directory)
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn open_at(parent: libc::c_int, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // openat only borrows the name and directory descriptor for this call.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), libc::O_RDONLY | flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // On success openat returns a new, owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn securing_rejects_an_ancestor_replaced_after_validation() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap().join("home");
        let parent = home.join("app/settings");
        fs::create_dir_all(&parent).unwrap();
        fs::write(parent.join("data"), "inside").unwrap();
        let outside = temporary.path().join("outside");
        fs::create_dir_all(outside.join("settings")).unwrap();
        let other = outside.join("settings/data");
        fs::write(&other, "outside").unwrap();
        fs::set_permissions(&other, Permissions::from_mode(0o644)).unwrap();

        let validated = target(&home, &home, Path::new("~/app/settings/data")).unwrap();
        fs::rename(home.join("app"), home.join("original")).unwrap();
        symlink(outside, home.join("app")).unwrap();

        assert!(secure_paths(&home, &[validated]).is_err());
        assert_eq!(
            fs::metadata(other).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn securing_changes_a_regular_file_and_skips_a_missing_path() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let path = home.join("app/data");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "tracked").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        secure_paths(&home, &[home.join("missing/data"), path.clone()]).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
