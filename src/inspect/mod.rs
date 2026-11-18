use crate::{Args, Ctx, Fail};

pub fn status(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-02", "dots status"))
}

pub fn plan(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-02", "dots plan"))
}

pub fn apply(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-02", "dots apply"))
}

pub fn retire_dotfiles(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-02", "dots retire-dotfiles"))
}

mod test_command;

pub use test_command::test;
