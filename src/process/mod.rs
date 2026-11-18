use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::{Ctx, Fail};

pub enum Cwd<'a> {
    Root,
    Caller,
    Dir(&'a Path),
}

impl Ctx {
    pub fn command(&self, _program: impl AsRef<OsStr>, _cwd: Cwd<'_>) -> Result<Command, Fail> {
        Err(Fail::unported("R-01 config", "preparing a child command"))
    }
}

pub struct Mise<'a> {
    _ctx: &'a Ctx,
}

impl<'a> Mise<'a> {
    pub fn new(_ctx: &'a Ctx) -> Result<Self, Fail> {
        Err(Fail::unported(
            "R-01 config",
            "selecting the mise executable",
        ))
    }

    pub fn command<I, S>(&self, _args: I) -> Result<Command, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Err(Fail::unported("R-01 config", "preparing a mise command"))
    }

    pub fn exec<I, S>(&self, _program: impl AsRef<OsStr>, _args: I) -> Result<Command, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Err(Fail::unported(
            "R-01 config",
            "preparing a pinned mise launch",
        ))
    }

    pub fn json<I, S>(&self, _args: I) -> Result<Value, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Err(Fail::unported(
            "R-01 config",
            "reading effective settings through mise",
        ))
    }
}

pub struct BundlePipe {
    _private: (),
}

impl BundlePipe {
    pub fn new(_json: &[u8]) -> Result<Self, Fail> {
        Err(Fail::unported(
            "P-05",
            "preparing the bootstrap hook's secret descriptor",
        ))
    }

    /// The pipe lives through spawn. Attachment must affect only this child.
    pub fn attach(&self, _command: &mut Command) -> Result<(), Fail> {
        Err(Fail::unported(
            "P-05",
            "attaching the bootstrap hook's secret descriptor",
        ))
    }
}

/// Private entry for dots-owned commands launched inside mise's environment.
pub fn exec_pinned(_args: &[OsString]) -> Result<u8, Fail> {
    Err(Fail::unported(
        "R-01 config",
        "launching the pinned executable",
    ))
}
