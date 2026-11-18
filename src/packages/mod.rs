pub mod github;

pub use github::install_release_app;

use crate::{Args, Ctx, Fail};

pub fn add(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-01", "dots add"))
}

pub fn remove(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-01", "dots remove"))
}
