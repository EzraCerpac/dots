use std::process::Command;

use serde::de::DeserializeOwned;

use crate::report::Command as Report;
use crate::run::{Issue, Outcome, Policy, Run, Sink, StageId, exit_code};
use crate::{Ctx, Fail};

pub mod packages;
mod retirement;
mod status;
mod test_command;

pub use retirement::retire_dotfiles;
pub use status::status;
pub use test_command::test;

fn with_report(ctx: &Ctx, command: Report, action: impl FnOnce(Run<'_>) -> u8) -> u8 {
    let run = Run::begin(ctx, Sink::Report(command));
    if let Err(error) = crate::cli::require_config(&ctx.root) {
        return startup_error(
            run,
            error,
            &format!(
                "Create a configuration with dots init or set DOTS_ROOT, then rerun your original dots {} command with the same arguments.",
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
