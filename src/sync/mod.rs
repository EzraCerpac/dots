pub mod lock;
mod source;

use crate::{Args, Ctx, Fail};

pub fn sync(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-08", "dots sync"))
}

use crate::receipt::Receipt;
use crate::run::{Issue, Run, StageId};

pub struct Shared {
    _private: (),
}

pub enum Loaded {
    LocalOnly,
    Problem(Issue),
    Ready(Shared),
}

pub enum Display {
    Standalone,
    Plain,
    Stage,
}

pub fn load(ctx: &Ctx) -> Loaded {
    if source::local_only(ctx).unwrap_or(false) {
        Loaded::LocalOnly
    } else {
        Loaded::Problem(Issue::new(
            Fail::unported("P-08", "loading the source configuration").diagnostic,
            "rerun with a build that includes P-08",
        ))
    }
}

pub fn run(_run: &Run<'_>, _stage: StageId, _loaded: Loaded, _display: Display) -> Receipt {
    unreachable!("sync runs are unported")
}
