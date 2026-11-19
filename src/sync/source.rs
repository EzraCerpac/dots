use std::io;
use std::process::Output;

use crate::{Fail, run::Outcome};

use super::git::Git;

pub(crate) fn select_dev(
    mut git: impl FnMut(&[&str]) -> Result<Output, Fail>,
    validate_origin: impl FnOnce() -> Result<(), Fail>,
) -> Result<Outcome, Fail> {
    let branch = git(&["symbolic-ref", "--quiet", "HEAD"])?;
    if !branch.status.success() || branch.stdout != b"refs/heads/main\n" {
        return Ok(Outcome::Skipped);
    }
    let mut reference = |name| {
        let output = git(&["rev-parse", "--verify", "--quiet", name])?;
        match output.status.code() {
            Some(0) => Ok(Some(output.stdout)),
            Some(1) => Ok(None),
            _ => Err(Fail::new(1, "could not inspect source refs")),
        }
    };
    let Some(head) = reference("HEAD")? else {
        return Ok(Outcome::Skipped);
    };
    if reference("refs/remotes/origin/main")?.as_ref() != Some(&head) {
        return Ok(Outcome::Skipped);
    }
    let dev = match (
        reference("refs/heads/dev")?,
        reference("refs/remotes/origin/dev")?,
    ) {
        (Some(local), Some(remote)) if local == remote => &["dev"][..],
        (Some(local), None) if local == head => &["dev"],
        (Some(_), _) => return Ok(Outcome::Skipped),
        (None, Some(_)) => &["--track", "-c", "dev", "refs/remotes/origin/dev"],
        (None, None) => &["-c", "dev"],
    };
    let status = git(&["status", "--porcelain", "--untracked-files=all"])?;
    if !status.status.success() {
        return Err(Fail::new(
            1,
            "could not check the source checkout for local edits",
        ));
    }
    if !status.stdout.is_empty() {
        return Ok(Outcome::Skipped);
    }
    // A normal switch may run a checkout hook. Verify origin before invoking it.
    validate_origin()?;
    let mut args = vec!["switch", "--quiet", "--no-guess", "--no-overwrite-ignore"];
    args.extend_from_slice(dev);
    if !git(&args)?.status.success() {
        return Err(Fail::new(
            1,
            "could not switch the clean source checkout to dev",
        ));
    }
    Ok(Outcome::Completed)
}

/// Prove the checkout has no origin, including a partial Git configuration or
/// a legacy remote stored outside the Git config file.
pub(super) fn origin_absent(git: &Git<'_>) -> io::Result<bool> {
    let inspect = |args: &[&str]| -> io::Result<Output> {
        let mut command = git.command(args)?;
        command.env("LC_ALL", "C").env("GIT_ALLOW_PROTOCOL", "");
        git.output_command(command)
    };
    let config = inspect(&["config", "--null", "--get-regexp", r"^remote\.origin\."])?;
    if config.status.code() != Some(1) || !config.stdout.is_empty() || !config.stderr.is_empty() {
        return Ok(false);
    }
    let origin = inspect(&["remote", "get-url", "origin"])?;
    Ok(origin.status.code() == Some(2)
        && origin.stdout.is_empty()
        && origin.stderr == b"error: No such remote 'origin'\n")
}
