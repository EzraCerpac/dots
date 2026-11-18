use crate::{Args, Ctx, Fail};

pub fn last(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-09", "dots sync --last"))
}

pub struct Receipt {
    _private: (),
}

pub fn emit(_ctx: &Ctx, _receipt: &Receipt) -> Result<(), Fail> {
    Err(Fail::unported(
        "P-09",
        "recording and printing a sync receipt",
    ))
}
