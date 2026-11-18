use crate::{Args, Ctx, Fail};

pub fn brew(_ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let operation = args.subcommand_name().map_or_else(
        || "dots brew".to_owned(),
        |name| format!("dots brew {name}"),
    );
    Err(Fail::unported("P-02", &operation))
}
