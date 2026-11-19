use std::io;
use std::process::Output;

use super::git::Git;

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
