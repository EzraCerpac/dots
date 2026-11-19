mod reports;

use std::io;

use crate::report::Command;
use crate::{Args, Ctx, Fail};

pub fn reports(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let command = args.get_one::<String>("COMMAND").map(|name| {
        let cli = crate::cli::tree();
        let name = cli
            .find_subcommand(name)
            .map_or(name.as_str(), |cmd| cmd.get_name());
        Command::ALL
            .into_iter()
            .find(|command| command.name() == name)
            .expect("report command accepted by the CLI")
    });
    reports::list(&ctx.env, command, &mut io::stdout().lock())
}

pub fn last(ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    use crate::report::{self, Command};
    use std::io;

    let read_error = |error| Fail::new(1, format!("could not read saved update report: {error}"));
    let entries = report::list(&ctx.env, Some(Command::Up)).map_err(read_error)?;
    for entry in entries {
        let report = match report::load(&ctx.env, &entry.id) {
            Ok(report) => report,
            // A completed report can be pruned between the two reads.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(read_error(error)),
        };
        transcript::write_report(&mut io::stdout().lock(), &report, true).map_err(|error| {
            Fail::new(1, format!("could not write saved update report: {error}"))
        })?;
        return Ok(report.status);
    }
    Err(Fail::new(1, "no completed update report is available yet"))
}

pub mod transcript;
