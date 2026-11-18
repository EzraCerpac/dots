use crate::{Args, Ctx, Fail};

pub fn reports(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-12", "dots reports"))
}

pub fn last(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-12", "dots up --last"))
}
