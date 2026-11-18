use crate::{Args, Ctx, Fail};

pub fn up(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-04", "dots up"))
}
