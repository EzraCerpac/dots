use crate::{Args, Ctx, Fail};

pub fn install(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-02", "dots exceptions"))
}

use crate::config::Exceptions;
use crate::run::Run;

pub enum Mode {
    Update,
    InstallOnly,
}

pub fn run(_run: &Run<'_>, _exceptions: &Exceptions, _mode: Mode) -> Result<(), Fail> {
    Err(Fail::unported("P-02", "running installer exceptions"))
}
