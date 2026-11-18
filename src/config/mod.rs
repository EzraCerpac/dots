mod doc;
mod exceptions;
mod path;
mod repo;
#[cfg(test)]
mod tests;
mod vars;

pub mod passive;
pub mod selection;

pub use doc::Doc;
pub use exceptions::{Exception, Exceptions};
pub use repo::{CheckoutShape, RepoSetting, checkout_shape};
pub use vars::Vars;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::{fmt, io};

use tempfile::TempDir;

use crate::{Env, Fail};
pub(crate) use path::physical;

#[derive(Debug)]
pub struct Root {
    path: PathBuf,
    pub(crate) child_bin: OnceLock<io::Result<TempDir>>,
}

impl Root {
    pub fn resolve(env: &Env) -> Result<Self, Fail> {
        let path = env
            .get("DOTS_ROOT")
            .or_else(|| env.get("MISE_CONFIG_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env.get("XDG_CONFIG_HOME")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| env.home().unwrap_or(Path::new("/")).join(".config"))
                    .join("mise")
            });
        Self::from_dir(env, &path)
    }

    /// Resolve a caller-supplied path, preserving any absent tail for init.
    pub(crate) fn from_dir(env: &Env, dir: &Path) -> Result<Self, Fail> {
        let absolute = env.cwd().join(dir);
        let path = physical(&absolute).map_err(|error| file_error(&absolute, error))?;
        Ok(Self {
            path,
            child_bin: OnceLock::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn file_error(path: &Path, problem: impl fmt::Display) -> Fail {
    Fail::usage(format!("{}: {problem}", path.display()))
}

pub(crate) fn valid_id(name: &str) -> bool {
    let alphanumeric = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    name.as_bytes().first().copied().is_some_and(alphanumeric)
        && name.bytes().all(|c| alphanumeric(c) || c == b'-')
}
