mod reports;

use std::io;

use crate::report::Command;
use crate::{Args, Ctx, Fail};

pub fn reports(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let command = args.get_one::<String>("COMMAND").map(|name| {
        let cli = crate::cli::tree();
        let name = cli.find_subcommand(name).unwrap().get_name();
        Command::ALL
            .into_iter()
            .find(|command| command.name() == name)
            .expect("report command accepted by the CLI")
    });
    reports::list(&ctx.env, command, &mut io::stdout().lock())
}

pub fn last(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-12", "dots up --last"))
}
