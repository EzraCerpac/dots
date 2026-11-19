use std::ffi::OsString;

use crate::process::{Mise, is_executable};
use crate::report::Command;
use crate::run::{Issue, Outcome, Run, StageId};
use crate::{Ctx, Fail};

pub fn test(ctx: &Ctx, args: &[OsString]) -> Result<u8, Fail> {
    Ok(super::with_report(ctx, Command::Test, |run| {
        if run.cancelled() {
            return run.end(130);
        }
        let stage = run.start("Configuration tests");
        let (status, diagnostic) = match run_tests(ctx, &run, stage, args) {
            Ok(status) => (
                status,
                format!("Configuration tests failed (exit {status})"),
            ),
            Err(error) => {
                if !run.cancelled() {
                    eprintln!("dots: {error}");
                }
                (error.status, error.to_string())
            }
        };
        if status != 0 && !run.cancelled() {
            run.issue(
                stage,
                Issue::new(
                    diagnostic,
                    "Review the output and rerun dots test with the same arguments.",
                ),
            );
        }
        run.finish(
            stage,
            if run.cancelled() {
                Outcome::Cancelled
            } else if status == 0 {
                Outcome::Completed
            } else {
                Outcome::Failed
            },
        );
        run.end(status)
    }))
}

fn run_tests(ctx: &Ctx, run: &Run<'_>, stage: StageId, args: &[OsString]) -> Result<u8, Fail> {
    let mise = Mise::new(ctx)?;
    let script = ctx.root.path().join("tests/run");
    if !is_executable(&script) {
        return Err(Fail::new(
            1,
            format!("{} is missing or not executable", script.display()),
        ));
    }
    let command = mise.exec(&script, args)?;
    super::run_command(run, stage, command, &script.display().to_string())
}
