use std::ffi::{OsStr, OsString};
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
    Ok(with_report(ctx, Report::Plan, |run| {
        let result = stage(&run, "Bootstrap plan", "dots plan", |stage| {
            let mise = Mise::new(ctx)?;
            let scope = packages::Scope::read(&run, &mise)?;
            let command = scope.scoped(mise.command_active(["bootstrap", "plan"])?, false);
            run_command(&run, stage, command, "mise bootstrap plan")
        });
        run.end(result)
    }))
}

pub fn apply(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let mut arguments = Vec::new();
    for flag in ["force", "dry-run", "yes", "prune", "prompt-secrets"] {
        if args.get_flag(flag) {
            arguments.push(format!("--{flag}").into());
        }
    }
    let target_offset = arguments.len();
    if let Some(targets) = args.get_many::<OsString>("TARGET") {
        arguments.push("--".into());
        arguments.extend(targets.cloned());
    }
    let rerun = apply_rerun(&arguments, target_offset);
    Ok(with_report(ctx, Report::Apply, |run| {
        let run = run.with_rerun(if rerun.starts_with("dots apply") {
            rerun.clone()
        } else {
            "Rerun your original dots apply command with the same arguments.".into()
        });
        let result = stage(&run, "Apply dotfiles", &rerun, |stage| {
            let mise = Mise::new(ctx)?;
            let native = ["bootstrap", "dotfiles", "apply"]
                .map(OsString::from)
                .into_iter()
                .chain(arguments);
            run_command(
                &run,
                stage,
                mise.command_active(native)?,
                "mise bootstrap dotfiles apply",
            )
        });
        run.end(result)
    }))
}

fn apply_rerun(arguments: &[OsString], target_offset: usize) -> String {
    let targets = arguments.get(target_offset + 1..).unwrap_or_default();
    let marker_needed = targets
        .iter()
        .any(|arg| arg.as_encoded_bytes().starts_with(b"-"));
    let operands = arguments
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != target_offset || marker_needed)
        .map(|(_, arg)| shell_operand(arg))
        .collect::<Option<Vec<_>>>();
    match operands.as_deref() {
        Some([]) => "dots apply".into(),
        Some(operands) => format!("dots apply {}", operands.join(" ")),
        None => "your original dots apply command".into(),
    }
}

fn shell_operand(arg: &OsStr) -> Option<String> {
    let arg = arg
        .to_str()
        .filter(|text| !text.chars().any(char::is_control))?;
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_./:@%+=,-".contains(&byte))
    {
        Some(arg.to_owned())
    } else {
        Some(format!("'{}'", arg.replace('\'', "'\\''")))
    }
}

fn with_report(ctx: &Ctx, command: Report, action: impl FnOnce(Run<'_>) -> u8) -> u8 {
    let run = Run::begin(ctx, Sink::Report(command));
    if let Err(error) = crate::cli::require_config(&ctx.root) {
        return startup_error(
            run,
            error,
            &format!(
                "Create a configuration with dots init or set DOTS_ROOT, then rerun dots {}.",
                command.name()
            ),
        );
    }
    action(run)
}

fn startup_error(run: Run<'_>, error: Fail, next: &str) -> u8 {
    if run.cancelled() {
        return run.end(130);
    }
    let diagnostic = format!("dots: {error}");
    eprintln!("{diagnostic}");
    let startup = run.start("Startup");
    run.issue(startup, Issue::new(diagnostic, next));
    run.finish(startup, Outcome::Failed);
    run.end(error.status)
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
                format!("Review the output and rerun {invocation}."),
            ),
        );
    }
    run.finish(
        id,
        if run.cancelled() {
            Outcome::Cancelled
        } else if status == 0 {
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
