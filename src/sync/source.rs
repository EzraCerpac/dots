use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use crate::config::{Vars, checkout_shape, selection};
use crate::process::Cwd;
use crate::{Ctx, Fail};

pub(super) fn local_only(ctx: &Ctx) -> Result<bool, Fail> {
    let root = ctx.root.path();
    if Vars::read(&ctx.root)?.source_repo.is_some() {
        return Ok(false);
    }

    let selected = selection::read(&ctx.root, &ctx.env)?;
    let profile = selected.profile.as_deref().unwrap_or_default();
    let git = |args: &[&str]| {
        let mut command = ctx.command_with_profile("git", Cwd::Root, profile.as_ref())?;
        // Inspect this checkout and its actual configuration, even from a Git hook.
        for key in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            command.env_remove(key);
        }
        for key in ctx
            .env
            .vars
            .keys()
            .filter(|key| key.as_encoded_bytes().starts_with(b"GIT_TRACE"))
        {
            command.env_remove(key);
        }
        command
            .args(["--no-pager", "--no-optional-locks"])
            .args(args)
            .env("LC_ALL", "C")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .output()
            .map_err(|_| git_error())
    };
    let shape = checkout_shape(root, |args| {
        let output = git(args)?;
        let has_stdout = !output.stdout.is_empty();
        if !output.stderr.is_empty() || output.status.success() != has_stdout {
            return Err(git_error());
        }
        Ok(output)
    })?;
    if shape.git_too_old
        || shape.jj_workspace
        || shape.legacy_sync
        || shape.head.as_deref() != Some(OsStr::new("refs/heads/dev"))
        || !shape.has_commit
        || !shape.operations.is_empty()
    {
        return Ok(false);
    }

    let path = |arg| -> Result<PathBuf, Fail> {
        let output = git(&["rev-parse", "--path-format=absolute", arg])?;
        let bytes = output.stdout.strip_suffix(b"\n").ok_or_else(git_error)?;
        let path = PathBuf::from(OsString::from_vec(bytes.to_vec()));
        if !output.status.success() || !output.stderr.is_empty() || !path.is_absolute() {
            return Err(git_error());
        }
        path.canonicalize().map_err(|_| git_error())
    };
    if path("--show-toplevel")? != root || path("--git-common-dir")? != shape.common_dir {
        return Ok(false);
    }
    let directory = path("--absolute-git-dir")?;
    for marker in ["dots-sync-pending", "dots-sync-write.json"] {
        if !matches!(directory.join(marker).symlink_metadata(), Err(error) if error.kind() == io::ErrorKind::NotFound)
        {
            return Ok(false);
        }
    }

    // Exit 1 with no output means there is no origin config. Any origin
    // setting, including a partial remote or one in an include, blocks.
    let config = git(&["config", "--null", "--get-regexp", r"^remote\.origin\."])?;
    if config.status.code() != Some(1) || !config.stdout.is_empty() || !config.stderr.is_empty() {
        return Ok(false);
    }
    // Git also recognizes legacy remotes stored outside its configuration.
    let origin = git(&["remote", "get-url", "origin"])?;
    Ok(origin.status.code() == Some(2)
        && origin.stdout.is_empty()
        && origin.stderr == b"error: No such remote 'origin'\n")
}

fn git_error() -> Fail {
    Fail::new(1, "Git could not inspect the source checkout")
}
