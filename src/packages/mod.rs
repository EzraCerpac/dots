mod add;
mod adopt;
mod edit;
pub mod github;
mod native;
mod remove;
#[cfg(test)]
mod tests;

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Vars, physical, selection};
use crate::report;
use crate::run::{Issue, Outcome, Policy, Run, Sink, StageId};
use crate::targets::Scope;
use crate::{Args, Ctx, Fail};

pub use github::install_release_app;

pub fn add(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    reported(ctx, report::Command::Add, |run| add::run(ctx, args, run))
}

pub fn remove(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    reported(ctx, report::Command::Remove, |run| {
        remove::run(ctx, args, run)
    })
}

fn reported(
    ctx: &Ctx,
    command: report::Command,
    operation: impl FnOnce(&Run<'_>) -> Result<(), Fail>,
) -> Result<u8, Fail> {
    let run = Run::begin(ctx, Sink::Report(command));
    let result = operation(&run);
    let status = run.end(result.as_ref().err().map_or(0, |error| error.status));
    match result {
        Err(error) if error.status == status => Err(error),
        _ => Ok(status),
    }
}

/// False stops the request list and leaves the exit status to Run::end.
fn stage(
    run: &Run<'_>,
    label: &str,
    next: &str,
    operation: impl FnOnce(StageId) -> Result<(), Fail>,
) -> Result<bool, Fail> {
    if run.cancelled() {
        return Ok(false);
    }
    #[cfg(test)]
    tests::before_stage_start();
    let id = run.start(label);
    // Admission can observe an interrupt after the preceding check.
    if run.cancelled() {
        return Ok(false);
    }
    let result = operation(id);
    if let Err(error) = &result
        && !run.cancelled()
    {
        run.issue(id, Issue::new(error.to_string(), next));
    }
    run.finish(
        id,
        if result.is_ok() {
            Outcome::Completed
        } else {
            Outcome::Failed
        },
    );
    if run.cancelled() {
        Ok(false)
    } else {
        result.map(|()| true)
    }
}

/// Native children own their output. A nonzero return needs a report issue, but
/// a duplicate terminal diagnostic would obscure the native message.
fn supervise(run: &Run<'_>, id: StageId, command: Command, action: &str) -> Result<(), Fail> {
    let program = command.get_program().to_string_lossy().into_owned();
    let status = run
        .run(id, command, Policy::Interactive)
        .map_err(|error| Fail::new(1, format!("{program} could not {action}: {error}")))?;
    if run.cancelled() || !status.success() {
        return Err(Fail::new(
            1,
            format!("{program} could not {action} ({status})"),
        ));
    }
    Ok(())
}

fn scope(args: &Args) -> Scope {
    if args.get_flag("base") {
        Scope::Base
    } else if let Some(profile) = args.get_one::<String>("profile") {
        Scope::Profile(profile.clone())
    } else if let Some(path) = args.get_one::<OsString>("path") {
        Scope::Path(PathBuf::from(path))
    } else {
        Scope::Active
    }
}

fn selected_path(ctx: &Ctx, scope: &Scope, default_profile: bool) -> Result<Option<PathBuf>, Fail> {
    let path = match scope {
        Scope::Active if !default_profile => return Ok(None),
        Scope::Active => {
            let profile = selection::read(&ctx.root, &ctx.env)?
                .profile
                .ok_or_else(|| {
                    Fail::usage("no profile selected; use --base, --profile, or --path")
                })?;
            ctx.root.path().join(format!("config.{profile}.toml"))
        }
        Scope::Base => ctx.root.path().join("config.toml"),
        Scope::Profile(profile) => {
            if !Vars::read(&ctx.root)?.profiles.contains(profile) {
                return Err(Fail::usage(format!("unknown dots profile {profile}")));
            }
            ctx.root.path().join(format!("config.{profile}.toml"))
        }
        Scope::Path(path) => absolute_request(ctx, path)?,
    };
    physical(&path).map(Some).map_err(|error| {
        Fail::usage(format!(
            "cannot resolve configuration path {}: {error}",
            path.display()
        ))
    })
}

fn absolute_request(ctx: &Ctx, path: &Path) -> Result<PathBuf, Fail> {
    if let Ok(relative) = path.strip_prefix("~") {
        let home = ctx
            .env
            .home()
            .ok_or_else(|| Fail::usage("set HOME to resolve ~/ paths"))?;
        Ok(home.join(relative))
    } else {
        Ok(ctx.env.cwd().join(path))
    }
}

fn file_request(request: &OsStr) -> bool {
    let bytes = request.as_encoded_bytes();
    [b"/".as_slice(), b"~/", b"./", b"../"]
        .iter()
        .any(|p| bytes.starts_with(p))
}

fn software_request(request: &OsStr) -> Result<&str, Fail> {
    request
        .to_str()
        .filter(|s| {
            !s.is_empty()
                && !s.starts_with('-')
                && !s.chars().any(char::is_control)
                && (!["https://", "http://", "ssh://"]
                    .iter()
                    .any(|prefix| s.starts_with(prefix))
                    || (github_url(s) && !s.contains(['?', '#', '@'])))
        })
        .ok_or_else(|| Fail::usage("invalid software name"))
}

fn github_url(request: &str) -> bool {
    request.starts_with("https://github.com/")
}

fn held(identity: &str, recipe: &str) -> Fail {
    Fail::usage(format!(
        "{identity} is held by installer exception {recipe}; change its recipe instead"
    ))
}

fn io_fail(action: &str, path: &Path, error: io::Error) -> Fail {
    Fail::new(1, format!("{action} {}: {error}", path.display()))
}

fn missing<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
