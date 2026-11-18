use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use nix::libc;

/// The source-operation lock shared by sync and publish.
///
/// Keep the inode in place: removing it would let a second process lock a new
/// inode while another participant still holds the original one.
#[derive(Debug)]
pub struct RepoLock {
    _file: File,
}

impl RepoLock {
    /// `common_git_dir` is the canonical, physical Git common directory.
    pub fn try_acquire(common_git_dir: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(common_git_dir.join("dots-source-rust.lock"))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "source operation lock must be a private regular file owned by this user",
            ));
        }

        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(Self { _file: file })),
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn excludes_another_participant_and_reuses_the_inode() {
        let dir = tempfile::tempdir().unwrap();
        let guard = RepoLock::try_acquire(dir.path()).unwrap().unwrap();
        let path = dir.path().join("dots-source-rust.lock");
        let original = fs::metadata(&path).unwrap();
        assert_eq!(original.permissions().mode() & 0o077, 0);
        assert!(RepoLock::try_acquire(dir.path()).unwrap().is_none());

        drop(guard);
        let _guard = RepoLock::try_acquire(dir.path()).unwrap().unwrap();
        assert_eq!(fs::metadata(path).unwrap().ino(), original.ino());
    }

    #[test]
    fn refuses_symlink_and_shared_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dots-source-rust.lock");
        let target = dir.path().join("unrelated");
        fs::write(&target, b"keep").unwrap();
        symlink(&target, &path).unwrap();
        assert!(RepoLock::try_acquire(dir.path()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"keep");

        fs::remove_file(&path).unwrap();
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            RepoLock::try_acquire(dir.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
