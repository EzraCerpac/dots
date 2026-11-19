pub mod bootstrap;
pub mod brew;
pub mod cli;
pub mod completion;
pub mod config;
pub mod config_commands;
pub mod exceptions;
pub mod history;
pub mod hooks;
pub mod init;
pub mod inspect;
pub mod packages;
pub mod process;
pub mod receipt;
pub mod report;
pub mod run;
pub mod skill;
pub mod supervisor;
pub mod sync;
pub mod targets;
pub mod ui;
pub mod update;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use config::Root;

pub type Args = clap::ArgMatches;

#[derive(Debug)]
pub struct Fail {
    pub status: u8,
    pub diagnostic: String,
    unported: bool,
}

impl Fail {
    pub fn new(status: u8, diagnostic: impl Into<String>) -> Self {
        Self {
            status,
            diagnostic: diagnostic.into(),
            unported: false,
        }
    }

    pub fn usage(diagnostic: impl Into<String>) -> Self {
        Self::new(2, diagnostic)
    }

    pub fn unported(task: &str, operation: &str) -> Self {
        Self {
            status: 1,
            diagnostic: format!("{operation} is not implemented in this build yet (task {task})"),
            unported: true,
        }
    }

    pub fn is_unported(&self) -> bool {
        self.unported
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.diagnostic.chars() {
            if c.is_control() {
                write!(f, "{}", c.escape_default())?;
            } else {
                write!(f, "{c}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for Fail {}

#[derive(Debug)]
pub struct Env {
    vars: std::collections::HashMap<OsString, OsString>,
    cwd: PathBuf,
}

impl Env {
    pub fn current() -> io::Result<Self> {
        Ok(Self {
            vars: std::env::vars_os().collect(),
            cwd: std::env::current_dir()?,
        })
    }

    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.vars.get(OsStr::new(name)).map(OsString::as_os_str)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn home(&self) -> Option<&Path> {
        self.get("HOME")
            .filter(|home| !home.is_empty())
            .map(Path::new)
    }

    /// The dots-owned directory beneath the configured state location.
    pub fn state_dir(&self) -> io::Result<PathBuf> {
        let base = self
            .get("XDG_STATE_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| self.home().map(|home| home.join(".local/state")))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "set HOME or XDG_STATE_HOME"))?;
        if !base.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state directory must be absolute",
            ));
        }
        Ok(base.join("dots"))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Flags {
    pub plain: bool,
    pub verbose: bool,
}

pub struct Ctx {
    pub env: Env,
    pub root: Root,
    pub flags: Flags,
}
