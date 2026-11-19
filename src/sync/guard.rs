//! Compare, displace and create exclusively using anchored directory handles.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::libc;

use super::git::{Entry, Git, invalid};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Image {
    Absent,
    Regular { bytes: Vec<u8>, mode: u32 },
    Link(PathBuf),
    Directory,
}

impl Image {
    pub fn from_git(git: &Git<'_>, entry: Option<&Entry>) -> io::Result<Self> {
        let Some(entry) = entry else {
            return Ok(Self::Absent);
        };
        let bytes = git.get(["cat-file", "blob", &entry.oid])?;
        match entry.mode.as_str() {
            "100644" => Ok(Self::Regular { bytes, mode: 0o644 }),
            "100755" => Ok(Self::Regular { bytes, mode: 0o755 }),
            "120000" => Ok(Self::Link(PathBuf::from(OsString::from_vec(bytes)))),
            _ => Err(invalid("unsupported checkout object type")),
        }
    }

    pub fn checkout(
        git: &Git<'_>,
        revision: &str,
        path: &Path,
        entry: Option<&Entry>,
    ) -> io::Result<Self> {
        match entry {
            Some(entry) if matches!(entry.mode.as_str(), "100644" | "100755") => {
                let mut argument = OsString::from("--path=");
                argument.push(path.as_os_str());
                let attrs = format!("--attr-source={revision}");
                let bytes = git.get([
                    attrs.as_ref(),
                    OsStr::new("cat-file"),
                    "--filters".as_ref(),
                    &argument,
                    entry.oid.as_ref(),
                ])?;
                let mode = if entry.mode == "100755" { 0o755 } else { 0o644 };
                Ok(Self::Regular { bytes, mode })
            }
            _ => Self::from_git(git, entry),
        }
    }

    pub fn git_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Regular { bytes: a, mode: am }, Self::Regular { bytes: b, mode: bm }) => {
                a == b && (am & 0o100) == (bm & 0o100)
            }
            _ => self == other,
        }
    }
}

pub(super) struct Dir(File);

impl Dir {
    pub fn open(path: &Path) -> io::Result<Self> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map(Self)
    }

    fn subdir(&self, name: &OsStr, create: bool) -> io::Result<Self> {
        let name = c_name(name)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let open = || {
            // SAFETY: name and the directory descriptor live through openat;
            // a successful result transfers one descriptor to File.
            let fd = unsafe { libc::openat(self.fd(), name.as_ptr(), flags) };
            owned(fd).map(Self)
        };
        match open() {
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                // SAFETY: name is a single component and dirfd is open.
                let made = unsafe { libc::mkdirat(self.fd(), name.as_ptr(), 0o755) };
                if made != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                    return Err(io::Error::last_os_error());
                }
                self.0.sync_all()?;
                open()
            }
            result => result,
        }
    }

    fn fd(&self) -> i32 {
        self.0.as_raw_fd()
    }

    pub fn parent(&self, path: &Path, create: bool) -> io::Result<(Self, CString)> {
        let mut dir = Self(self.0.try_clone()?);
        let parent = path.parent().ok_or_else(|| invalid("path has no parent"))?;
        for part in parent.components() {
            let std::path::Component::Normal(part) = part else {
                return Err(invalid("invalid relative path"));
            };
            dir = dir.subdir(part, create)?;
        }
        let name = path
            .file_name()
            .ok_or_else(|| invalid("path has no name"))?;
        Ok((dir, c_name(name)?))
    }

    pub fn read_path(&self, path: &Path) -> io::Result<Image> {
        match self.parent(path, false) {
            Ok((dir, name)) => dir.read(&name),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Image::Absent),
            Err(error) => Err(error),
        }
    }

    fn read(&self, name: &CString) -> io::Result<Image> {
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: the directory descriptor and name live through openat.
        let fd = unsafe { libc::openat(self.fd(), name.as_ptr(), flags) };
        match owned(fd) {
            Ok(mut file) => {
                let md = file.metadata()?;
                if md.is_dir() {
                    return Ok(Image::Directory);
                }
                if !md.is_file() {
                    return Err(invalid("unsupported filesystem entry"));
                }
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok(Image::Regular {
                    bytes,
                    mode: md.permissions().mode() & 0o7777,
                })
            }
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                let mut buffer = vec![0; 1024];
                loop {
                    // SAFETY: readlinkat writes at most buffer.len() bytes
                    // into this live writable buffer.
                    let size = unsafe {
                        libc::readlinkat(
                            self.fd(),
                            name.as_ptr(),
                            buffer.as_mut_ptr().cast(),
                            buffer.len(),
                        )
                    };
                    if size < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if (size as usize) < buffer.len() {
                        buffer.truncate(size as usize);
                        return Ok(Image::Link(PathBuf::from(OsString::from_vec(buffer))));
                    }
                    if buffer.len() >= 65_536 {
                        return Err(invalid("symlink destination is too long"));
                    }
                    buffer.resize(buffer.len() * 2, 0);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Image::Absent),
            Err(error) => Err(error),
        }
    }

    fn move_to(&self, name: &CString, saved: &Dir, slot: &CString) -> io::Result<()> {
        // SAFETY: both descriptors and names live through the exclusive
        // rename. An existing destination remains untouched.
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                self.fd(),
                name.as_ptr(),
                saved.fd(),
                slot.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                self.fd(),
                name.as_ptr(),
                saved.fd(),
                slot.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        saved.0.sync_all()?;
        self.0.sync_all()
    }

    fn link_to(&self, name: &CString, dir: &Dir, target: &CString) -> io::Result<()> {
        // SAFETY: both directory descriptors and names live through linkat.
        // A zero flag links the symlink itself, and linkat never overwrites.
        let result =
            unsafe { libc::linkat(self.fd(), name.as_ptr(), dir.fd(), target.as_ptr(), 0) };
        checked(result)?;
        dir.0.sync_all()
    }

    pub fn directory_identity(&self, path: &Path) -> io::Result<PathBuf> {
        let (parent, name) = self.parent(path, false)?;
        parent
            .subdir(OsStr::from_bytes(name.as_bytes()), false)?
            .identity()
    }

    fn identity(&self) -> io::Result<PathBuf> {
        let metadata = self.0.metadata()?;
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()).into())
    }

    fn is_empty(&self) -> io::Result<bool> {
        let fd = self.0.try_clone()?.into_raw_fd();
        // SAFETY: on success the stream owns this duplicate.
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: fdopendir did not consume the descriptor.
            drop(unsafe { File::from_raw_fd(fd) });
            return Err(error);
        }
        let result = loop {
            nix::errno::Errno::clear();
            // SAFETY: stream is open and each entry is used before readdir
            // is called again.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                let errno = nix::errno::Errno::last_raw();
                break if errno == 0 {
                    Ok(true)
                } else {
                    Err(io::Error::from_raw_os_error(errno))
                };
            }
            // SAFETY: readdir supplies a NUL-terminated d_name.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if !matches!(name.to_bytes(), b"." | b"..") {
                break Ok(false);
            }
        };
        // SAFETY: the stream is open and no entry is borrowed now.
        unsafe { libc::closedir(stream) };
        result
    }
}

fn owned(fd: i32) -> io::Result<File> {
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: successful openat returned a newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn checked(result: i32) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || matches!(bytes, b"." | b"..") || bytes.contains(&b'/') {
        return Err(invalid("unsafe file name"));
    }
    CString::new(bytes).map_err(io::Error::other)
}

pub(super) fn private_dir(parent: &Path, prefix: &str) -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
}

pub(super) fn write_private(path: &Path, image: &Image) -> io::Result<()> {
    match image {
        Image::Regular { bytes, mode } => {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(bytes)?;
            file.set_permissions(fs::Permissions::from_mode(*mode))?;
            file.sync_all()
        }
        Image::Link(destination) => std::os::unix::fs::symlink(destination, path),
        Image::Absent => Ok(()),
        Image::Directory => Err(invalid("cannot stage a directory as a leaf")),
    }
}

fn same_inode(first: &Dir, one: &CString, second: &Dir, two: &CString) -> io::Result<bool> {
    fn identity(dir: &Dir, name: &CString) -> io::Result<(libc::dev_t, libc::ino_t)> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the open directory and C string stay valid through fstatat.
        checked(unsafe {
            libc::fstatat(
                dir.fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        // SAFETY: a successful fstatat initialized the buffer.
        let stat = unsafe { stat.assume_init() };
        Ok((stat.st_dev, stat.st_ino))
    }
    Ok(identity(first, one)? == identity(second, two)?)
}

fn verify_parent(root: &Dir, path: &Path, held: &Dir) -> io::Result<()> {
    let (current, _) = root.parent(path, false)?;
    if current.identity()? == held.identity()? {
        Ok(())
    } else {
        Err(invalid("target's parent changed during replacement"))
    }
}

#[allow(clippy::too_many_arguments)]
fn displace_directory(
    root: &Dir,
    path: &Path,
    parent: &Dir,
    name: &CString,
    staging: &Dir,
    old_slot: &CStrName,
    current: &Image,
    replacement: &Image,
    before_move: impl Fn() -> io::Result<()>,
) -> io::Result<bool> {
    if staging.read(&old_slot.0)? == Image::Absent {
        if *current != Image::Directory {
            return Err(invalid("no displaced directory for this replacement"));
        }
        let child = parent.subdir(OsStr::from_bytes(name.as_bytes()), false)?;
        if !child.is_empty()? {
            return Err(invalid("target directory has local files"));
        }
        before_move()?;
        verify_parent(root, path, parent)?;
        parent.move_to(name, staging, &old_slot.0)?;
        let moved = staging.subdir(OsStr::from_bytes(old_slot.0.as_bytes()), false)?;
        if moved.identity()? != child.identity()? || !moved.is_empty()? {
            let _ = staging.move_to(&old_slot.0, parent, name);
            return Err(invalid("directory changed during displacement"));
        }
    }
    verify_parent(root, path, parent)?;
    let current = parent.read(name)?;
    if staging.read(&old_slot.0)? == Image::Directory {
        let moved = staging.subdir(OsStr::from_bytes(old_slot.0.as_bytes()), false)?;
        if &current == replacement && *replacement != Image::Absent {
            return Ok(true);
        }
        if current == Image::Absent && moved.is_empty()? {
            return Ok(false);
        }
    }
    Err(invalid("directory or target changed during replacement"))
}

/// Publish a staged image, remembering the displaced inode even on success.
/// Re-entering with the same recovery directory after an interruption is safe.
#[allow(clippy::too_many_arguments)]
fn replace_with(
    root: &Dir,
    path: &Path,
    staging: &Dir,
    old_slot: &CStrName,
    new_slot: &CStrName,
    expected: &Image,
    replacement: &Image,
    consume_replacement: bool,
    before_move: impl Fn() -> io::Result<()>,
    before_publish: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    let (parent, name) = root.parent(path, !matches!(replacement, Image::Absent))?;
    let current = parent.read(&name)?;
    let displaced = staging.read(&old_slot.0)?;
    if *expected == Image::Directory {
        if displace_directory(
            root,
            path,
            &parent,
            &name,
            staging,
            old_slot,
            &current,
            replacement,
            before_move,
        )? {
            return Ok(());
        }
    } else if !matches!(displaced, Image::Absent) {
        if !matches!(replacement, Image::Absent)
            && &current == replacement
            && same_inode(staging, &new_slot.0, &parent, &name)?
        {
            return Ok(());
        }
        if &displaced != expected {
            if current == Image::Absent {
                let _ = before_publish()
                    .and_then(|()| verify_parent(root, path, &parent))
                    .and_then(|()| staging.link_to(&old_slot.0, &parent, &name));
            }
            return Err(invalid("displaced file changed"));
        }
        if !matches!(current, Image::Absent) {
            return Err(invalid("another writer claimed the target"));
        }
    } else if &current == expected {
        match current {
            Image::Absent => (),
            _ => {
                before_move()?;
                verify_parent(root, path, &parent)?;
                parent.move_to(&name, staging, &old_slot.0)?;
                verify_parent(root, path, &parent)?;
                if &staging.read(&old_slot.0)? != expected {
                    // Restore by exclusive link only; the recovered inode
                    // stays private if a writer has already claimed the path.
                    let _ = before_publish()
                        .and_then(|()| verify_parent(root, path, &parent))
                        .and_then(|()| staging.link_to(&old_slot.0, &parent, &name));
                    return Err(invalid("target changed during replacement"));
                }
            }
        }
    } else if &current == replacement
        && *expected == Image::Absent
        && same_inode(staging, &new_slot.0, &parent, &name)?
    {
        // A completed exclusive install survives interruption before the
        // caller's journal advances.
        return Ok(());
    } else {
        return Err(invalid("target no longer matches the verified version"));
    }

    if !matches!(replacement, Image::Absent) {
        before_publish()?;
        verify_parent(root, path, &parent)?;
        if consume_replacement {
            staging.move_to(&new_slot.0, &parent, &name)?;
        } else {
            staging.link_to(&new_slot.0, &parent, &name)?;
        }
        verify_parent(root, path, &parent)?;
    }
    Ok(())
}

pub(super) struct CStrName(CString);

impl CStrName {
    pub fn new(value: &str) -> io::Result<Self> {
        c_name(value.as_ref()).map(Self)
    }
}

pub(super) fn replace_exact(
    root: &Dir,
    path: &Path,
    staging: &Dir,
    old: &CStrName,
    new: &CStrName,
    expected: &Image,
    replacement: &Image,
) -> io::Result<()> {
    replace_with(
        root,
        path,
        staging,
        old,
        new,
        expected,
        replacement,
        false,
        || Ok(()),
        || Ok(()),
    )
}

pub(super) fn replace_consuming(
    root: &Dir,
    path: &Path,
    staging: &Dir,
    old_slot: &CStrName,
    new_slot: &CStrName,
    expected: &Image,
    replacement: &Image,
) -> io::Result<()> {
    replace_with(
        root,
        path,
        staging,
        old_slot,
        new_slot,
        expected,
        replacement,
        true,
        || Ok(()),
        || Ok(()),
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn replace_at_boundary(
    root: &Dir,
    path: &Path,
    staging: &Dir,
    old: &CStrName,
    new: &CStrName,
    expected: &Image,
    replacement: &Image,
    before_move: impl Fn() -> io::Result<()>,
    before_publish: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    replace_with(
        root,
        path,
        staging,
        old,
        new,
        expected,
        replacement,
        false,
        before_move,
        before_publish,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(value: &[u8]) -> Image {
        Image::Regular {
            bytes: value.to_vec(),
            mode: 0o600,
        }
    }

    #[test]
    fn resumes_displacement_without_discarding_the_original_inode() {
        let world = tempfile::tempdir().unwrap();
        let root = private_dir(world.path(), "root").unwrap();
        let stage = private_dir(world.path(), "stage").unwrap();
        let original = bytes(b"before");
        let replacement = bytes(b"after");
        let live = root.path().join("target");
        write_private(&live, &original).unwrap();
        let mut editor = OpenOptions::new().append(true).open(&live).unwrap();
        fs::rename(&live, stage.path().join("previous")).unwrap();
        write_private(&stage.path().join("replacement"), &replacement).unwrap();

        replace_exact(
            &Dir::open(root.path()).unwrap(),
            Path::new("target"),
            &Dir::open(stage.path()).unwrap(),
            &CStrName::new("previous").unwrap(),
            &CStrName::new("replacement").unwrap(),
            &original,
            &replacement,
        )
        .unwrap();
        editor.write_all(b" edit").unwrap();
        assert_eq!(fs::read(live).unwrap(), b"after");
        assert_eq!(
            fs::read(stage.path().join("previous")).unwrap(),
            b"before edit"
        );
    }

    #[test]
    fn a_writer_claiming_the_target_keeps_both_versions() {
        let world = tempfile::tempdir().unwrap();
        let root = private_dir(world.path(), "root").unwrap();
        let stage = private_dir(world.path(), "stage").unwrap();
        let original = bytes(b"before");
        let replacement = bytes(b"after");
        let live = root.path().join("target");
        write_private(&stage.path().join("previous"), &original).unwrap();
        write_private(&stage.path().join("replacement"), &replacement).unwrap();
        write_private(&live, &bytes(b"competing file")).unwrap();

        let error = replace_exact(
            &Dir::open(root.path()).unwrap(),
            Path::new("target"),
            &Dir::open(stage.path()).unwrap(),
            &CStrName::new("previous").unwrap(),
            &CStrName::new("replacement").unwrap(),
            &original,
            &replacement,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(live).unwrap(), b"competing file");
        assert_eq!(fs::read(stage.path().join("previous")).unwrap(), b"before");
    }

    #[test]
    fn directory_replacement_resumes_after_displacement_and_keeps_a_claimant() {
        let world = tempfile::tempdir().unwrap();
        let root = private_dir(world.path(), "root").unwrap();
        let stage = private_dir(world.path(), "stage").unwrap();
        let live = root.path().join("target");
        fs::create_dir(&live).unwrap();
        let original = fs::metadata(&live).unwrap().ino();
        let replacement = bytes(b"new file");
        write_private(&stage.path().join("new"), &replacement).unwrap();
        let root_dir = Dir::open(root.path()).unwrap();
        let stage_dir = Dir::open(stage.path()).unwrap();
        let path = Path::new("target");
        let old = CStrName::new("old").unwrap();
        let new = CStrName::new("new").unwrap();

        let error = replace_at_boundary(
            &root_dir,
            path,
            &stage_dir,
            &old,
            &new,
            &Image::Directory,
            &replacement,
            || Ok(()),
            || Err(io::ErrorKind::Interrupted.into()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!live.exists());
        assert_eq!(
            fs::metadata(stage.path().join("old")).unwrap().ino(),
            original
        );

        fs::create_dir(&live).unwrap();
        fs::write(live.join("user-file"), b"keep this").unwrap();
        assert!(
            replace_exact(
                &root_dir,
                path,
                &stage_dir,
                &old,
                &new,
                &Image::Directory,
                &replacement
            )
            .is_err()
        );
        assert_eq!(fs::read(live.join("user-file")).unwrap(), b"keep this");

        fs::rename(&live, root.path().join("claimant")).unwrap();
        replace_exact(
            &root_dir,
            path,
            &stage_dir,
            &old,
            &new,
            &Image::Directory,
            &replacement,
        )
        .unwrap();
        assert_eq!(fs::read(live).unwrap(), b"new file");
        assert_eq!(
            fs::metadata(stage.path().join("old")).unwrap().ino(),
            original
        );
    }

    #[test]
    fn directory_swapped_during_displacement_is_restored_without_clobbering() {
        let world = tempfile::tempdir().unwrap();
        let root = private_dir(world.path(), "root").unwrap();
        let stage = private_dir(world.path(), "stage").unwrap();
        let live = root.path().join("target");
        fs::create_dir(&live).unwrap();
        let replacement = bytes(b"new file");
        write_private(&stage.path().join("new"), &replacement).unwrap();

        let error = replace_at_boundary(
            &Dir::open(root.path()).unwrap(),
            Path::new("target"),
            &Dir::open(stage.path()).unwrap(),
            &CStrName::new("old").unwrap(),
            &CStrName::new("new").unwrap(),
            &Image::Directory,
            &replacement,
            || {
                fs::rename(&live, root.path().join("saved-by-user"))?;
                fs::create_dir(&live)?;
                fs::write(live.join("user-file"), b"foreign directory")
            },
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read(live.join("user-file")).unwrap(),
            b"foreign directory"
        );
        assert!(root.path().join("saved-by-user").is_dir());
        assert!(!stage.path().join("old").exists());
    }

    #[test]
    fn changing_the_parent_during_displacement_keeps_both_directories() {
        let world = tempfile::tempdir().unwrap();
        let root = private_dir(world.path(), "root").unwrap();
        let stage = private_dir(world.path(), "stage").unwrap();
        let parent = root.path().join("sub");
        fs::create_dir_all(parent.join("target")).unwrap();
        let replacement = bytes(b"new file");
        write_private(&stage.path().join("new"), &replacement).unwrap();

        let error = replace_at_boundary(
            &Dir::open(root.path()).unwrap(),
            Path::new("sub/target"),
            &Dir::open(stage.path()).unwrap(),
            &CStrName::new("old").unwrap(),
            &CStrName::new("new").unwrap(),
            &Image::Directory,
            &replacement,
            || {
                fs::rename(&parent, root.path().join("saved-parent"))?;
                fs::create_dir_all(parent.join("target"))?;
                fs::write(parent.join("target/user-file"), b"foreign directory")
            },
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read(parent.join("target/user-file")).unwrap(),
            b"foreign directory"
        );
        assert!(root.path().join("saved-parent/target").is_dir());
    }
}
