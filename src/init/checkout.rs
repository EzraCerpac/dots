use std::fs::{OpenOptions, TryLockError};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use crate::Fail;

use super::command_line;
use super::git::{Git, trim_line};

/// P-03 owns this read-only advice until the shared checkout-shape
/// probe covers all the conditions init has to explain.
pub(super) fn advise(git: &Git<'_>) -> Result<(), Fail> {
    let head = git.head()?;
    let has_commit = git.commit("HEAD")?.is_some();
    match head.as_deref() {
        Some("refs/heads/dev") => {}
        Some(branch) => println!(
            "HEAD is on {}. Move this configuration to dev before running dots sync.",
            branch.strip_prefix("refs/heads/").unwrap_or(branch)
        ),
        None => println!("HEAD is detached. Switch to dev before running dots sync."),
    }
    if !has_commit {
        println!("HEAD has no commit. Commit the configuration on dev before running dots sync.");
    }
    if git.root.join(".jj").is_dir() {
        println!("This is a JJ workspace. Use a plain Git checkout for dots sync.");
    }
    if git.git_path("dots-dev.json")?.exists() {
        println!(
            "This checkout has legacy sync state. Convert it to the 1.0 Git layout before running dots sync."
        );
    }
    for marker in [
        "MERGE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
    ] {
        if git.git_path(marker)?.exists() {
            println!(
                "Git operation in progress ({marker}). Finish or abort it before running dots sync."
            );
        }
    }
    match source_operation_in_progress(git)? {
        Ok(true) => println!(
            "Another sync or publish holds the repository lock. Wait for it to finish before running dots sync."
        ),
        Ok(false) => {}
        Err(err) => println!(
            "Could not inspect the source operation lock ({err}). Check its ownership and permissions before running dots sync."
        ),
    }
    let version = git.checked(&["--version"], "read the Git version")?;
    if git_too_old(trim_line(&version.stdout)) {
        println!("Update Git to 2.43 or newer before running dots sync.");
    }
    if head.as_deref() == Some("refs/heads/main")
        && has_commit
        && git.commit("refs/heads/dev")?.is_none()
        && git.commit("refs/remotes/origin/dev")?.is_none()
        && git
            .checked(
                &["--no-optional-locks", "status", "--porcelain", "-z"],
                "check for local edits",
            )?
            .stdout
            .is_empty()
    {
        println!();
        command_line("git -C ", git.root, " switch -c dev")?;
    }
    Ok(())
}

// P-03 read-only adapter until P-08 exposes its source-lock probe.
fn source_operation_in_progress(git: &Git<'_>) -> Result<io::Result<bool>, Fail> {
    let common = git.checked(&["rev-parse", "--git-common-dir"], "locate the source lock")?;
    let dir = git
        .root
        .join(std::ffi::OsStr::from_bytes(trim_line(&common.stdout)));
    Ok(probe_lock(&dir.join("dots-source-rust.lock")))
}

fn probe_lock(path: &std::path::Path) -> io::Result<bool> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "lock must be a private regular file owned by this user",
        ));
    }
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

fn git_too_old(version: &[u8]) -> bool {
    let version = String::from_utf8_lossy(version);
    let mut fields = version.trim_start_matches("git version ").split('.');
    let major = fields.next().and_then(|part| part.parse::<u32>().ok());
    let minor = fields.next().and_then(|part| part.parse::<u32>().ok());
    major.zip(minor).is_some_and(|version| version < (2, 43))
}
