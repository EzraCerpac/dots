use std::io;
use std::process::Output;

use crate::run::Outcome;

use super::git::{Git, oid};

pub(crate) fn select_dev<G, E>(
    mut git: G,
    validate: impl FnOnce(&str, &str) -> Result<(), E>,
    failure: impl Fn(&'static str) -> E,
) -> Result<Outcome, E>
where
    G: FnMut(&[&str]) -> Result<Output, E>,
{
    let on_branch = |git: &mut G, expected: &[u8]| {
        let branch = git(&["symbolic-ref", "--quiet", "HEAD"])?;
        Ok(branch.status.success() && branch.stdout == expected)
    };
    if !on_branch(&mut git, b"refs/heads/main\n")? {
        return Ok(Outcome::Skipped);
    }
    let reference = |git: &mut G, name| {
        let output = git(&["rev-parse", "--verify", "--quiet", name])?;
        match output.status.code() {
            Some(0) => oid(output.stdout.trim_ascii())
                .map(Some)
                .map_err(|_| failure("could not verify a source revision")),
            Some(1) => Ok(None),
            _ => Err(failure("could not inspect source refs")),
        }
    };
    let Some(head) = reference(&mut git, "HEAD")? else {
        return Ok(Outcome::Skipped);
    };
    if reference(&mut git, "refs/remotes/origin/main")?.as_ref() != Some(&head) {
        return Ok(Outcome::Skipped);
    }
    let local = reference(&mut git, "refs/heads/dev")?;
    let remote = reference(&mut git, "refs/remotes/origin/dev")?;
    let candidate = local.as_deref().or(remote.as_deref()).unwrap_or(&head);
    let start = format!("{candidate}^{{commit}}");
    let dev = match (&local, &remote) {
        (Some(local), Some(remote)) if local == remote => &["dev"][..],
        (Some(local), None) if local == &head => &["dev"],
        (Some(_), _) => return Ok(Outcome::Skipped),
        (None, Some(_)) => &["--no-track", "-c", "dev", &start],
        (None, None) => &["-c", "dev", &start],
    };
    let clean = |git: &mut G| {
        let status = git(&["status", "--porcelain", "--untracked-files=all"])?;
        if !status.status.success() {
            return Err(failure(
                "could not check the source checkout for local edits",
            ));
        }
        Ok(status.stdout.is_empty())
    };
    if !clean(&mut git)? {
        return Ok(Outcome::Skipped);
    }
    validate(&head, candidate)?;
    let recheck = |git: &mut G| {
        if !clean(git)?
            || !on_branch(git, b"refs/heads/main\n")?
            || reference(git, "HEAD")?.as_ref() != Some(&head)
            || reference(git, "refs/remotes/origin/main")?.as_ref() != Some(&head)
            || reference(git, "refs/heads/dev")? != local
            || reference(git, "refs/remotes/origin/dev")? != remote
        {
            return Err(failure("Source checkout changed before switching to dev"));
        }
        Ok(())
    };
    recheck(&mut git)?;
    if local.is_none() && remote.is_some() {
        // Record the upstream before creating dev, without resolving the
        // cached ref. A failed config write leaves this checkout on main.
        for (key, value) in [
            ("branch.dev.remote", "origin"),
            ("branch.dev.merge", "refs/heads/dev"),
        ] {
            if !git(&["config", "--local", "--replace-all", key, value])?
                .status
                .success()
            {
                return Err(failure("could not set origin/dev as the upstream for dev"));
            }
        }
        recheck(&mut git)?;
    }
    let mut args = vec!["switch", "--quiet", "--no-guess", "--no-overwrite-ignore"];
    args.extend_from_slice(dev);
    if !git(&args)?.status.success() {
        return Err(failure("could not switch the clean source checkout to dev"));
    }
    if !on_branch(&mut git, b"refs/heads/dev\n")?
        || reference(&mut git, "HEAD")?.as_deref() != Some(candidate)
    {
        return Err(failure("dev does not match the verified source revision"));
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
