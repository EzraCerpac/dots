use crate::{Args, Ctx, Fail};

pub fn bootstrap(_ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let operation = args.subcommand_name().map_or_else(
        || "dots bootstrap".to_owned(),
        |name| format!("dots bootstrap {name}"),
    );
    Err(Fail::unported("P-05", &operation))
}

pub fn bundle(_ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let operation = args.subcommand_name().map_or_else(
        || "dots bundle".to_owned(),
        |name| format!("dots bundle {name}"),
    );
    Err(Fail::unported("P-05", &operation))
}

pub fn remote(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-05", "dots remote"))
}
