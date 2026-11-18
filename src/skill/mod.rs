use crate::{Ctx, Fail};

pub fn refresh(_ctx: &Ctx) -> Result<(), Fail> {
    Err(Fail::unported(
        "R-01 config",
        "refreshing agent skill links",
    ))
}
