use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, Permissions};
use std::io;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use nix::libc;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

use crate::Fail;

use super::native::{Native, exists, expand};
use super::probe_state::ProbeState;
use super::{blocked, invalid};

#[derive(Deserialize)]
struct Inventory {
    entries: Vec<Entry>,
    invalid: Vec<InvalidEntry>,
    // Flattening forces object input. A derived struct also accepts an array
    // of positional fields by default, including [[], []] here.
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct InvalidEntry {
    path: PathBuf,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct Entry {
    mode: String,
    path: PathBuf,
    variant: Option<String>,
    declared_in: Option<PathBuf>,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

fn inventory_command(
    native: &Native<'_, '_>,
    args: &[&str],
    state: Option<&ProbeState>,
) -> Result<Command, Fail> {
    let mut cmd = native.mise(args)?;
    if let Some(state) = state {
        // Even the native read commands drain notice queues on dispatch.
        cmd.env("MISE_STATE_DIR", state.path());
    }
    cmd.env_remove("__MISE_HISTORY_LOCAL")
        .env("MISE_HISTORY_SYNC", "manual")
        .env("GIT_ALLOW_PROTOCOL", "");
    if native.checkpoint {
        cmd.env("MISE_OFFLINE", "1")
            .env("MISE_NO_HOOKS", "1")
            .env("MISE_AUTO_UPDATE", "0")
            .env("GIT_NO_LAZY_FETCH", "1");
    }
    Ok(cmd)
}

fn inventory(
    native: &Native<'_, '_>,
    local: bool,
    state: Option<&ProbeState>,
) -> Result<Inventory, Fail> {
    let mut args = vec!["bootstrap", "dotfiles"];
    if local {
        args.push("--local");
    }
    args.extend(["paths", "--json"]);
    let cmd = inventory_command(native, &args, state)?;
    let diagnostic = "could not read native tracked history paths";
    let output = native.probe(cmd, diagnostic)?;
    if !output.status.success() {
        return Err(blocked(diagnostic));
    }
    let inventory: Inventory = serde_json::from_slice(&output.stdout)
        .map_err(|_| invalid("mise returned an invalid tracked history inventory"))?;
    // Only a fresh probe with isolated state can treat stderr as uncertainty.
    // Established stores may print a queued notice alongside valid inventory.
    if state.is_some() && inventory.entries.is_empty() && !output.stderr.is_empty() {
        return Err(blocked(diagnostic));
    }
    let unproved = if local || inventory.invalid.is_empty() {
        inventory.invalid.first()
    } else {
        let inactive = inactive_invalid_paths(native, &inventory.invalid, state)?;
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
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct StatusEntry {
    target: PathBuf,
    mode: String,
    state: String,
    reason: Option<String>,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

fn inactive_invalid_paths(
    native: &Native<'_, '_>,
    invalid_rows: &[InvalidEntry],
    state: Option<&ProbeState>,
) -> Result<BTreeSet<PathBuf>, Fail> {
    // Native paths can report a fault before testing variants. Ask status
    // to select those targets without opening or initializing the store.
    let diagnostic = "could not verify variants of invalid tracked history entries";
    let mut cmd = inventory_command(
        native,
        &["bootstrap", "dotfiles", "status", "--json", "--"],
        state,
    )?;
    cmd.args(invalid_rows.iter().map(|entry| &entry.path))
        .env("MISE_HISTORY_ENABLED", "false");
    let output = native.probe(cmd, diagnostic)?;
    if !output.status.success() || state.is_some() && !output.stderr.is_empty() {
        // A malformed declaration can be absent from status completely.
        // Keep the paths command's configuration error in that case.
        return Ok(BTreeSet::new());
    }
    let status: Status = serde_json::from_slice(&output.stdout).map_err(|_| invalid(diagnostic))?;
    Ok(invalid_rows
        .iter()
        .filter(|row| {
            if row.path.as_os_str().is_empty() {
                return false;
            }
            let mut matching = status.files.iter().filter(|entry| entry.target == row.path);
            matching.next().is_some_and(|entry| {
                entry.mode == "track"
                    && entry.state == "tracked"
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
            }) && matching.next().is_none()
        })
        .map(|row| row.path.clone())
        .collect())
}

pub(super) fn capture_paths(
    native: &Native<'_, '_>,
    host: Option<&str>,
) -> Result<TrackedPaths, Fail> {
    let state = ProbeState::for_capture(native)?;
    let paths = discover(native, host, None, state.as_ref())?;
    if let Some(entry) = inventory(native, true, state.as_ref())?.entries.first() {
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
    if let Some(state) = state {
        state.recheck()?;
    }
    Ok(paths)
}

pub(super) fn paths(native: &Native<'_, '_>, host: Option<&str>) -> Result<TrackedPaths, Fail> {
    discover(native, host, None, None)
}

pub(super) fn restore_paths(
    native: &Native<'_, '_>,
    host: Option<&str>,
) -> Result<TrackedPaths, Fail> {
    discover(native, host, Some(native.ctx.root.path()), None)
}

fn discover(
    native: &Native<'_, '_>,
    host: Option<&str>,
    source: Option<&Path>,
    state: Option<&ProbeState>,
) -> Result<TrackedPaths, Fail> {
    let inventory = inventory(native, false, state)?;
    let home = native.home()?;
    let physical_home = physical_home(home)?;
    let mut sources = HashMap::new();
    let mut paths = BTreeSet::new();

    for entry in inventory.entries {
        if entry.mode != "track" {
            return Err(invalid("mise returned an unknown tracked history mode"));
        }
        let target = lexical_target(home, &physical_home, &entry.path)?;
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
    let paths = paths.into_iter().collect::<Vec<_>>();
    let (binding, _) = observe(physical_home, source, &paths, false)?;
    Ok(TrackedPaths(binding))
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

fn physical_home(home: &Path) -> Result<PathBuf, Fail> {
    home.canonicalize()
        .map_err(|_| invalid("could not resolve HOME"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct PathBinding {
    home: PathBuf,
    source: Option<(PathBuf, DirectoryIdentity)>,
    targets: Vec<PathBuf>,
    directories: BTreeMap<PathBuf, DirectoryIdentity>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    fn read(directory: &File) -> io::Result<Self> {
        let metadata = directory.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl PathBinding {
    fn for_targets(&self, targets: &[PathBuf]) -> Self {
        let directories = self
            .home
            .ancestors()
            .chain(targets.iter().flat_map(|path| path.ancestors().skip(1)))
            .filter_map(|path| {
                self.directories
                    .get(path)
                    .map(|identity| (path.to_owned(), *identity))
            })
            .collect();
        Self {
            home: self.home.clone(),
            source: self.source.clone(),
            targets: targets.to_vec(),
            directories,
        }
    }

    fn check(&self, previous: &Self, allow_created: bool) -> Result<(), Fail> {
        let same = if allow_created {
            self.home == previous.home
                && self.source == previous.source
                && self.targets == previous.targets
                && previous
                    .directories
                    .iter()
                    .all(|(path, identity)| self.directories.get(path) == Some(identity))
        } else {
            self == previous
        };
        if !same {
            return Err(blocked(
                "HOME or a tracked history destination changed since validation",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct TrackedPaths(PathBinding);

impl Deref for TrackedPaths {
    type Target = [PathBuf];

    fn deref(&self) -> &Self::Target {
        &self.0.targets
    }
}

impl TrackedPaths {
    pub(super) fn binding(&self) -> &PathBinding {
        &self.0
    }

    pub(super) fn check_binding(
        &self,
        previous: &PathBinding,
        allow_created: bool,
    ) -> Result<(), Fail> {
        self.0.check(previous, allow_created)
    }

    pub(super) fn created_directories<'a>(
        &'a self,
        previous: &'a PathBinding,
    ) -> impl Iterator<Item = &'a Path> {
        self.0
            .directories
            .keys()
            .filter(|path| !previous.directories.contains_key(*path))
            .map(PathBuf::as_path)
    }

    pub(super) fn recheck(&self, native: &Native<'_, '_>) -> Result<(), Fail> {
        let home = physical_home(native.home()?)?;
        let source = self.0.source.as_ref().map(|_| native.ctx.root.path());
        reopen(home, source, self.binding(), false).map(drop)
    }

    pub(super) fn open_file(&self, path: &Path) -> Result<Option<File>, Fail> {
        if self
            .0
            .targets
            .binary_search_by(|target| target.as_path().cmp(path))
            .is_err()
        {
            return Err(invalid(
                "history observation lies outside the tracked inventory",
            ));
        }
        let binding = self.0.for_targets(&[path.to_owned()]);
        let source = binding.source.as_ref().map(|(root, _)| root.as_path());
        let mut files = reopen(binding.home.clone(), source, &binding, true)?;
        Ok(files.pop())
    }

    pub(super) fn secure(&self, native: &Native<'_, '_>) -> Result<(), Fail> {
        let home = physical_home(native.home()?)?;
        let source = self.0.source.as_ref().map(|_| native.ctx.root.path());
        // Validate the entire set before changing any mode, then hold only
        // one batch of open descriptors at a time.
        reopen(home.clone(), source, self.binding(), false)?;
        for targets in self.chunks(16) {
            let binding = self.0.for_targets(targets);
            let files = reopen(home.clone(), source, &binding, true)?;
            protect_modes(files, source.is_some())?;
        }
        Ok(())
    }
}

fn reopen(
    home: PathBuf,
    source: Option<&Path>,
    binding: &PathBinding,
    open_files: bool,
) -> Result<Vec<File>, Fail> {
    let (observed, files) = observe(home, source, &binding.targets, open_files)?;
    observed.check(binding, false)?;
    Ok(files)
}

fn protect_modes(files: Vec<File>, restoring: bool) -> Result<(), Fail> {
    for file in files {
        if restoring
            && file
                .metadata()
                .map_err(|_| blocked("could not verify a tracked history file before chmod"))?
                .nlink()
                != 1
        {
            return Err(linked_restore_target());
        }
        file.set_permissions(Permissions::from_mode(0o600))
            .map_err(|_| blocked("could not set tracked history file mode to 0600"))?;
    }
    Ok(())
}

const DIRECTORY_FLAGS: libc::c_int = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

fn observe(
    home: PathBuf,
    source: Option<&Path>,
    targets: &[PathBuf],
    open_files: bool,
) -> Result<(PathBinding, Vec<File>), Fail> {
    let root = Path::new("/");
    let root_fd = open_at(libc::AT_FDCWD, c"/", DIRECTORY_FLAGS)
        .map_err(|_| blocked("could not open filesystem root for history validation"))?;
    let source = source
        .map(|path| {
            let fd = walk_directories(&root_fd, root, path, &mut BTreeMap::new())?
                .ok_or_else(|| blocked("could not open source checkout for restore validation"))?;
            let identity = DirectoryIdentity::read(&fd)
                .map_err(|_| blocked("could not verify source checkout identity"))?;
            Ok::<_, Fail>((path.to_owned(), identity))
        })
        .transpose()?;

    let mut directories = BTreeMap::new();
    let home_fd = walk_directories(&root_fd, root, &home, &mut directories)?
        .ok_or_else(|| blocked("could not safely open HOME for history validation"))?;
    let mut files = Vec::new();
    for target in targets {
        if !target.starts_with(&home) {
            return Err(invalid("a tracked history path lies outside HOME"));
        }
        let parent = target
            .parent()
            .ok_or_else(|| invalid("invalid tracked history destination"))?;
        let Some(directory) = walk_directories(&home_fd, &home, parent, &mut directories)? else {
            continue;
        };
        let name = target
            .file_name()
            .and_then(|name| c_name(name).ok())
            .ok_or_else(|| invalid("invalid tracked history destination"))?;
        if !regular_target(&directory, &name, target, source.is_some())? || !open_files {
            continue;
        }
        let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        let file = open_at(directory.as_raw_fd(), &name, flags).map_err(|_| {
            blocked("could not safely open a tracked history file to protect its mode")
        })?;
        if !file.metadata().is_ok_and(|meta| meta.is_file()) {
            return Err(blocked(
                "tracked history file changed before protecting its mode",
            ));
        }
        files.push(file);
    }
    if let Some((root, identity)) = &source
        && !targets.is_empty()
        && (targets.iter().any(|path| path.starts_with(root))
            || directories.values().any(|directory| directory == identity))
    {
        return Err(invalid(
            "restore would write a tracked path inside the source checkout",
        ));
    }
    let binding = PathBinding {
        home,
        source,
        targets: targets.to_vec(),
        directories,
    };
    Ok((binding, files))
}

fn walk_directories(
    root: &File,
    base: &Path,
    path: &Path,
    directories: &mut BTreeMap<PathBuf, DirectoryIdentity>,
) -> Result<Option<File>, Fail> {
    let diagnostic = "could not verify a tracked history parent directory";
    let relative = path.strip_prefix(base).map_err(|_| invalid(diagnostic))?;
    let mut directory = root.try_clone().map_err(|_| blocked(diagnostic))?;
    let mut current = base.to_owned();
    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            let Component::Normal(name) = component else {
                return Err(invalid(diagnostic));
            };
            current.push(name);
            let name = c_name(name).map_err(|_| invalid(diagnostic))?;
            directory = match open_at(directory.as_raw_fd(), &name, DIRECTORY_FLAGS) {
                Ok(next) => next,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                    return Err(invalid(format!(
                        "refusing symlink or non-directory in tracked history path {}",
                        current.display()
                    )));
                }
                Err(_) => return Err(blocked(diagnostic)),
            };
        }
        let identity = DirectoryIdentity::read(&directory).map_err(|_| blocked(diagnostic))?;
        if directories
            .insert(current.clone(), identity)
            .is_some_and(|previous| previous != identity)
        {
            return Err(blocked(
                "a tracked history parent directory changed during validation",
            ));
        }
    }
    Ok(Some(directory))
}

fn regular_target(
    directory: &File,
    name: &CStr,
    path: &Path,
    restoring: bool,
) -> Result<bool, Fail> {
    let mut info = MaybeUninit::<libc::stat>::uninit();
    // fstatat borrows the live parent descriptor and writes only to info.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return match io::Error::last_os_error().kind() {
            io::ErrorKind::NotFound => Ok(false),
            _ => Err(blocked("could not inspect a tracked history destination")),
        };
    }
    // Successful fstatat initialized the result.
    let info = unsafe { info.assume_init() };
    match info.st_mode & libc::S_IFMT {
        libc::S_IFREG if restoring && info.st_nlink != 1 => Err(linked_restore_target()),
        libc::S_IFREG => Ok(true),
        libc::S_IFLNK => Err(invalid(format!(
            "refusing symlink in tracked history path {}",
            path.display()
        ))),
        _ => Err(invalid(format!(
            "tracked history path is not a regular file: {}",
            path.display()
        ))),
    }
}

fn linked_restore_target() -> Fail {
    invalid(
        "restore refuses a hard-linked tracked file. Replace the link with a private copy and retry",
    )
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

        let path = home.join("app/settings/data");
        let validated = observe(home.clone(), None, &[path], false).unwrap().0;
        fs::rename(home.join("app"), home.join("original")).unwrap();
        symlink(outside, home.join("app")).unwrap();

        assert!(reopen(home, None, &validated, true).is_err());
        assert_eq!(
            fs::metadata(other).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn securing_rejects_a_replacement_real_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let parent = home.join("app");
        let path = parent.join("data");
        fs::create_dir(&parent).unwrap();
        fs::write(&path, "original").unwrap();
        let validated = observe(home.clone(), None, std::slice::from_ref(&path), false)
            .unwrap()
            .0;
        fs::rename(&parent, home.join("original")).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(&path, "replacement").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        assert!(reopen(home, None, &validated, true).is_err());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn restore_refuses_a_home_target_inside_the_source_checkout() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let source = home.join("Source");
        fs::create_dir(&source).unwrap();
        let path = lexical_target(&home, &home, Path::new("~/Source/local/file")).unwrap();
        let paths = [path];
        let problem = observe(home.clone(), Some(&source), &paths, false).unwrap_err();
        assert!(problem.to_string().contains("inside the source checkout"));
        assert!(observe(home.clone(), None, &paths, false).is_ok());

        // On a case-insensitive filesystem the alternate spelling doesn't
        // start with the source path, but has the same directory identity.
        if home.join("source").is_dir() {
            let alias = home.join("source/file");
            assert!(!alias.starts_with(&source));
            assert!(observe(home, Some(&source), &[alias], false).is_err());
        }
    }

    #[test]
    fn journal_binding_identifies_created_parents_but_rejects_replaced_home() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().canonicalize().unwrap();
        let home = directory.join("home");
        let parent = home.join("new");
        let paths = [parent.join("data")];
        fs::create_dir(&home).unwrap();
        let validated = observe(home.clone(), None, &paths, false).unwrap().0;
        let saved = serde_json::to_vec(&validated).unwrap();
        let prior: PathBinding = serde_json::from_slice(&saved).unwrap();

        fs::create_dir(&parent).unwrap();
        let after = TrackedPaths(observe(home.clone(), None, &paths, false).unwrap().0);
        assert!(after.check_binding(&prior, false).is_err());
        after.check_binding(&prior, true).unwrap();
        assert_eq!(
            after.created_directories(&prior).collect::<Vec<_>>(),
            [parent.as_path()]
        );

        fs::rename(&home, directory.join("original")).unwrap();
        fs::create_dir_all(&parent).unwrap();
        let changed = TrackedPaths(observe(home, None, &paths, false).unwrap().0);
        assert!(changed.check_binding(&prior, true).is_err());
    }

    #[test]
    fn securing_changes_a_regular_file_and_skips_a_missing_path() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let path = home.join("app/data");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "tracked").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        let paths = [home.join("missing/data"), path.clone()];
        let validated = observe(home.clone(), None, &paths, false).unwrap().0;
        protect_modes(reopen(home, None, &validated, true).unwrap(), false).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn restore_rechecks_link_count_on_the_descriptor_before_chmod() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let source = home.join("Source");
        fs::create_dir(&source).unwrap();
        let target = home.join("tracked");
        let alias = source.join("script");
        fs::write(&target, "fixture").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o755)).unwrap();
        let paths = std::slice::from_ref(&target);
        let (binding, _) = observe(home.clone(), Some(&source), paths, false).unwrap();
        let files = reopen(home, Some(&source), &binding, true).unwrap();

        fs::hard_link(&target, &alias).unwrap();
        assert!(
            protect_modes(files, true)
                .unwrap_err()
                .to_string()
                .contains("hard-linked")
        );
        assert_eq!(fs::read(&alias).unwrap(), b"fixture");
        assert_eq!(
            fs::metadata(alias).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
