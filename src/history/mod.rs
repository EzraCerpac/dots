mod inventory;

use crate::{Args, Ctx, Fail};

pub fn backup(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-06", "dots backup"))
}

pub fn restore_command(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-06", "dots restore"))
}

pub fn enroll_command(_ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    Err(Fail::unported("P-06", "dots enroll-history"))
}

use std::path::Path;

use crate::config::RepoSetting;
use crate::run::{Issue, Outcome, Run, StageId};

pub struct Restore<'a> {
    pub machine: &'a str,
    pub origin: &'a RepoSetting,
    pub identity: &'a Path,
    pub initialize: bool,
}

pub struct Enroll<'a> {
    pub profile: &'a str,
    pub machine: &'a str,
    pub origin: &'a RepoSetting,
    pub identity: &'a Path,
    pub recipients: &'a [String],
}

#[derive(Debug)]
pub enum Checkpoint {
    Saved,
    NothingTracked,
}

fn unported(operation: &str) -> Issue {
    Issue::new(
        Fail::unported("P-06", operation).diagnostic,
        "rerun with a build that includes P-06",
    )
}

pub fn restore(run: &Run<'_>, stage: StageId, _settings: Restore<'_>) -> Outcome {
    run.issue(stage, unported("restoring encrypted settings"));
    Outcome::Failed
}

pub fn enroll(run: &Run<'_>, stage: StageId, _settings: Enroll<'_>) -> Outcome {
    run.issue(stage, unported("enrolling encrypted settings history"));
    Outcome::Failed
}

pub fn checkpoint(run: &Run<'_>, stage: StageId, ctx: &Ctx) -> Result<Checkpoint, Issue> {
    if inventory::empty(run, stage, ctx) {
        Ok(Checkpoint::NothingTracked)
    } else {
        Err(unported("saving a settings checkpoint"))
    }
}
