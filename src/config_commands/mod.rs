use std::ffi::OsString;

use crate::{Args, Ctx, Env, Fail, config::Root};

pub enum Invocation<'a> {
    Root,
    Profile(&'a Args),
    Mise(&'a [OsString]),
    External { name: &'a str, args: &'a [OsString] },
}

pub fn dispatch(_ctx: &Ctx, invocation: Invocation<'_>) -> Result<u8, Fail> {
    let operation = match invocation {
        Invocation::Root => "dots root",
        Invocation::Profile(_) => "dots profile",
        Invocation::Mise(_) => "dots mise",
        Invocation::External { name, .. } => {
            let mut bytes = name.bytes();
            let safe = bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
                && bytes.all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
            let diagnostic = if safe {
                format!("unknown command '{name}'; use dots help")
            } else {
                "invalid command name; use dots help".to_owned()
            };
            return Err(Fail::usage(diagnostic));
        }
    };
    Err(Fail::unported("R-01 config", operation))
}

pub fn list(_root: &Root) -> Vec<String> {
    // Help shows only configuration commands this build can dispatch.
    Vec::new()
}

pub fn version(_env: &Env) -> Result<String, Fail> {
    Err(Fail::unported("R-01 config", "dots --version"))
}
