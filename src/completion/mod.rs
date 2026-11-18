use std::ffi::OsString;
use std::path::Path;

use crate::{Fail, config::Root};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Quote {
    Unquoted,
    Quoted,
    Escaped,
}

#[derive(Debug)]
pub struct Word {
    pub text: OsString,
    pub quote: Quote,
}

/// Offsets are zero-based. Words exclude the executable, and bytes index the decoded word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    pub word: usize,
    pub byte: usize,
}

pub struct Request<'a> {
    pub words: &'a [Word],
    pub cursor: Cursor,
    pub shell: Shell,
    pub cwd: &'a Path,
    pub root: &'a Root,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Candidate {
    pub value: String,
    pub description: String,
    pub append_space: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Response {
    Path,
    Dir,
    Values(Vec<Candidate>),
}

pub fn complete(_request: Request<'_>) -> Response {
    unreachable!("completion query is unported")
}

pub fn script(_shell: Shell) -> Result<&'static str, Fail> {
    Err(Fail::unported("R-01 completion", "dots completions"))
}

pub fn query(_args: &[OsString]) -> Result<u8, Fail> {
    Err(Fail::unported("R-01 completion", "shell completion"))
}
