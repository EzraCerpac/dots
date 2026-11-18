use crate::{Args, Ctx, Fail};

pub fn add(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-01", "dots add"))
}

pub fn remove(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-01", "dots remove"))
}

pub fn install_release_app(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-01", "dots install-release-app"))
}
