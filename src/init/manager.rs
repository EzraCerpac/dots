use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde_json::Value;

use crate::config::{MANAGER_FALLBACK, manager_tool};
use crate::process::Mise;
use crate::{Ctx, Fail};

use super::git::{Git, trim_line};

const TOOL: &str = "github:EzraCerpac/dots";

pub(super) fn ensure(ctx: &Ctx, git: &Git<'_>) -> Result<(), Fail> {
    let mise = Mise::new(ctx)?;
    if declares_manager(&mise)?
        || mise
            .command_active(["which", "dots"])?
            .env("MISE_OFFLINE", "1")
            .output()
            .is_ok_and(|output| output.status.success())
    {
        return Ok(());
    }
    reject_tracked(git)?;
    let request = ctx.env.get("DOTS_VERSION").unwrap_or(OsStr::new("latest"));
    let request = request
        .to_str()
        .ok_or_else(|| Fail::usage("DOTS_VERSION must name a dots release"))?;
    let request = request.strip_prefix('v').unwrap_or(request);
    if request != "latest" && request.parse::<semver::Version>().is_err() {
        return Err(Fail::usage("DOTS_VERSION must name a dots release"));
    }

    let path = git.root.join(MANAGER_FALLBACK);
    let parent = path.parent().expect("manager fragment parent");
    ensure_directory(parent)?;
    ensure_ignored(git)?;
    let contents = format!("[tools]\n\"{TOOL}\" = \"{request}\"\n");
    let candidate = super::files::candidate(&path, contents.as_bytes(), 0o600)
        .map_err(|error| super::file_error(&path, error))?;
    candidate
        .persist_noclobber(&path)
        .map_err(|error| super::file_error(&path, error.error))?;
    if !declares_manager(&mise)? {
        return Err(Fail::new(
            1,
            format!(
                "mise did not load the machine-local configuration at {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn declares_manager(mise: &Mise<'_>) -> Result<bool, Fail> {
    let files = mise.json(["config", "ls", "--json"])?;
    let files = files
        .as_array()
        .ok_or_else(|| Fail::new(1, "mise returned an invalid configuration listing"))?;
    Ok(files
        .iter()
        .filter_map(|file| file["tools"].as_array())
        .flatten()
        .filter_map(Value::as_str)
        .any(manager_tool))
}

fn reject_tracked(git: &Git<'_>) -> Result<(), Fail> {
    let index = git.checked(
        &[
            "ls-files",
            "-z",
            "--",
            ":(icase,literal)conf.d/00-dots-manager.toml",
        ],
        "inspect the machine-local manager path",
    )?;
    let tracked = || {
        Fail::new(
            1,
            format!("Git tracks the machine-local path {MANAGER_FALLBACK}"),
        )
    };
    if !index.stdout.is_empty() {
        return Err(tracked());
    }
    let head = git.output(&["rev-parse", "--verify", "--quiet", "HEAD"])?;
    match head.status.code() {
        Some(0) => {
            let tree = git.checked(
                &["ls-tree", "-r", "--name-only", "-z", "HEAD", "--", "."],
                "inspect the committed machine-local manager path",
            )?;
            if tree.stdout.split(|&byte| byte == 0).any(|name| {
                name.get(..MANAGER_FALLBACK.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(MANAGER_FALLBACK.as_bytes()))
                    && matches!(name.get(MANAGER_FALLBACK.len()), None | Some(b'/'))
            }) {
                return Err(tracked());
            }
        }
        Some(1) => {}
        _ => return Err(Fail::new(1, "Git could not inspect the current commit")),
    }
    Ok(())
}

fn ensure_ignored(git: &Git<'_>) -> Result<(), Fail> {
    let check = || git.output(&["check-ignore", "-q", "--", MANAGER_FALLBACK]);
    match check()?.status.code() {
        Some(0) => return Ok(()),
        Some(1) => {}
        _ => {
            return Err(Fail::new(
                1,
                "Git could not check the machine-local exclusion",
            ));
        }
    }
    let output = git.checked(
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
        "locate the checkout-local Git exclusions",
    )?;
    let path = Path::new(OsStr::from_bytes(trim_line(&output.stdout)));
    let parent = path
        .parent()
        .filter(|_| path.is_absolute())
        .ok_or_else(|| Fail::new(1, "Git did not return an absolute exclusion path"))?;
    ensure_directory(parent)?;
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("expected a regular Git exclusion file"));
        }
        file.write_all(format!("\n/{MANAGER_FALLBACK}\n").as_bytes())
    })();
    result.map_err(|error| super::file_error(path, error))?;
    if !check()?.status.success() {
        return Err(Fail::new(
            1,
            format!("Git does not ignore {MANAGER_FALLBACK}"),
        ));
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), Fail> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) =>
        {
            Ok(())
        }
        Err(error) => Err(super::file_error(path, error)),
    }
}
