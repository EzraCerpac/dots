use std::ffi::OsString;
use std::process::Command;

use serde::de::DeserializeOwned;

use crate::process::Mise;
use crate::report::Command as Report;
use crate::run::{Issue, Outcome, Policy, Run, Sink, StageId, exit_code};
use crate::{Args, Ctx, Fail};

pub mod packages;
mod retirement;
mod status;
mod test_command;

pub use retirement::retire_dotfiles;
pub use status::status;
pub use test_command::test;

pub fn plan(ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    let run = Run::begin(ctx, Sink::Report(Report::Plan));
    let result = stage(&run, "Bootstrap plan", "dots plan", |stage| {
        let mise = Mise::new(ctx)?;
        let scope = packages::Scope::read(&run, &mise)?;
        let command = scope.scoped(mise.command(["bootstrap", "plan"])?, false);
        run_command(&run, stage, command, "mise bootstrap plan")
    });
    Ok(run.end(result))
}

pub fn apply(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let run = Run::begin(ctx, Sink::Report(Report::Apply));
    let result = stage(&run, "Apply dotfiles", "dots apply", |stage| {
        let mise = Mise::new(ctx)?;
        let mut native = ["bootstrap", "dotfiles", "apply"]
            .map(OsString::from)
            .to_vec();
        for flag in ["force", "dry-run", "yes", "prune", "prompt-secrets"] {
            if args.get_flag(flag) {
                native.push(format!("--{flag}").into());
            }
        }
        if let Some(targets) = args.get_many::<OsString>("TARGET") {
            native.push("--".into());
            native.extend(targets.cloned());
        }
        run_command(
            &run,
            stage,
            mise.command(native)?,
            "mise bootstrap dotfiles apply",
        )
    });
    Ok(run.end(result))
}

fn run_command(run: &Run<'_>, stage: StageId, cmd: Command, label: &str) -> Result<u8, Fail> {
    run.run(stage, cmd, Policy::Ordinary)
        .map(exit_code)
        .map_err(|error| Fail::new(1, format!("cannot run {label}: {error}")))
}

fn stage(
    run: &Run<'_>,
    label: &str,
    invocation: &str,
    action: impl FnOnce(StageId) -> Result<u8, Fail>,
) -> u8 {
    if run.cancelled() {
        return 130;
    }
    let id = run.start(label);
    let (status, diagnostic) = match action(id) {
        Ok(status) => (status, format!("{label} exited {status}")),
        Err(error) => {
            if !run.cancelled() {
                eprintln!("dots: {error}");
            }
            (error.status, error.to_string())
        }
    };
    if status != 0 && !run.cancelled() {
        run.issue(
            id,
            Issue::new(
                diagnostic,
                format!("Review the output and rerun {invocation} with the same arguments."),
            ),
        );
    }
    run.finish(
        id,
        if status == 0 {
            Outcome::Completed
        } else {
            Outcome::Failed
        },
    );
    status
}

fn read_json<T: DeserializeOwned>(
    run: &Run<'_>,
    command: Command,
    action: &str,
) -> Result<T, Fail> {
    let output = run
        .probe(command)
        .map_err(|error| Fail::new(1, format!("could not {action}: {error}")))?;
    if !output.status.success() {
        return Err(Fail::new(
            1,
            format!(
                "could not {action}: mise exited {}",
                exit_code(output.status)
            ),
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| {
        Fail::new(
            1,
            format!(
                "could not {action}: invalid mise response at line {}, column {}",
                error.line(),
                error.column(),
            ),
        )
    })
}
