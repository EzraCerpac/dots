use std::ffi::OsStr;

use crate::Fail;
use crate::config::checkout_shape;
use crate::sync::lock::RepoLock;

use super::command_line;
use super::git::Git;

pub(super) fn advise(git: &Git<'_>) -> Result<(), Fail> {
    let shape = checkout_shape(git.root, |args| git.output(args))?;
    let head = shape.head.as_deref();
    match head {
        Some(branch) if branch == "refs/heads/dev" => {}
        Some(branch) => {
            let branch = branch.to_string_lossy();
            println!(
                "HEAD is on {}. Move this configuration to dev before running dots sync.",
                branch.strip_prefix("refs/heads/").unwrap_or(&branch)
            );
        }
        None => println!("HEAD is detached. Switch to dev before running dots sync."),
    }
    if !shape.has_commit {
        println!("HEAD has no commit. Commit the configuration on dev before running dots sync.");
    }
    if shape.jj_workspace {
        println!("This is a JJ workspace. Use a plain Git checkout for dots sync.");
    }
    if shape.legacy_sync {
        println!(
            "This checkout has legacy sync state in .git/dots-dev.json. See the Checkout shape section of the sync guide before running dots sync."
        );
    }
    for marker in shape.operations {
        println!(
            "Git operation in progress ({marker}). Finish or abort it before running dots sync."
        );
    }
    match RepoLock::is_held(&shape.common_dir) {
        Ok(true) => println!(
            "Another sync or publish holds the repository lock. Wait for it to finish before running dots sync."
        ),
        Ok(false) => {}
        Err(err) => println!(
            "Could not inspect the source operation lock ({err}). Check its ownership and permissions before running dots sync."
        ),
    }
    if shape.git_too_old {
        println!("Update Git to 2.43 or newer before running dots sync.");
    }
    if head == Some(OsStr::new("refs/heads/main"))
        && shape.has_commit
        && git.commit("refs/heads/dev")?.is_none()
        && git.commit("refs/remotes/origin/dev")?.is_none()
        && git
            .checked(
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain",
                    "--ignore-submodules=none",
                    "--untracked-files=normal",
                    "-z",
                ],
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
