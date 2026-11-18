use crate::{Args, Ctx, Fail};

pub fn publish(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-10", "dots publish"))
}
