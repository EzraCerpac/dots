use crate::{Args, Ctx, Fail};

pub fn init(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-03", "dots init"))
}
