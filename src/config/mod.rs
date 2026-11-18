use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

use crate::{Env, Fail};

#[derive(Debug)]
pub struct Root(PathBuf);

impl Root {
    /// The initial candidate path. The config lane adds physical resolution.
    pub fn resolve(env: &Env) -> Result<Self, Fail> {
        let path = env
            .get("DOTS_ROOT")
            .or_else(|| env.get("MISE_CONFIG_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env.get("XDG_CONFIG_HOME")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        // With no HOME, the default suffix still names an absolute path.
                        env.home().unwrap_or(Path::new("/")).join(".config")
                    })
                    .join("mise")
            });
        Ok(Self(env.cwd().join(path)))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug)]
pub struct Vars {
    pub profiles: Vec<String>,
    pub source_repo: Option<RepoSetting>,
    pub state_repo: Option<RepoSetting>,
    pub self_updating_casks: Vec<String>,
    pub publish_on_behalf_of: Option<String>,
}

impl Vars {
    pub fn read(_root: &Root) -> Result<Self, Fail> {
        Err(Fail::unported(
            "R-01 config",
            "reading configuration settings",
        ))
    }
}

#[derive(Debug)]
pub struct RepoSetting {
    _private: (),
}

impl RepoSetting {
    pub fn parse(_value: &str) -> Result<Self, Fail> {
        Err(Fail::unported("R-01 config", "parsing repository settings"))
    }

    pub fn matches(&self, _url: &str) -> bool {
        unreachable!("RepoSetting::parse is unported")
    }
}

pub mod selection {
    use super::{Env, Fail, Root};

    #[derive(Debug)]
    pub struct Selection {
        pub profile: Option<String>,
        pub machine_id: Option<String>,
        pub selectors: Vec<String>,
    }

    pub struct Persist<'a> {
        pub profile: &'a str,
        pub machine_id: &'a str,
        pub origin: Option<&'a str>,
    }

    pub fn read(_root: &Root, _env: &Env) -> Result<Selection, Fail> {
        Err(Fail::unported(
            "R-01 config",
            "reading the profile and machine ID",
        ))
    }

    pub fn persist(_root: &Root, _env: &Env, _selection: Persist<'_>) -> Result<(), Fail> {
        Err(Fail::unported(
            "R-01 config",
            "persisting the profile and machine ID",
        ))
    }
}

pub struct Doc {
    pub document: DocumentMut,
    _path: PathBuf,
}

impl Doc {
    pub fn open(_path: &Path) -> Result<Self, Fail> {
        Err(Fail::unported(
            "R-01 config",
            "opening a configuration document for editing",
        ))
    }

    pub fn install(self) -> Result<(), Fail> {
        Err(Fail::unported(
            "R-01 config",
            "installing a configuration document",
        ))
    }
}

pub mod passive {
    use super::{DocumentMut, Fail, Path, PathBuf, Root};

    pub fn toml(_path: &Path) -> Result<DocumentMut, Fail> {
        Err(Fail::unported(
            "R-01 config",
            "reading configuration TOML without evaluation",
        ))
    }

    pub fn profiles(_root: &Root) -> Result<Vec<String>, Fail> {
        Err(Fail::unported("R-01 config", "reading base profiles"))
    }

    /// Outer failures are failed probes. The inner results preserve per-file failures.
    pub fn active_files(_root: &Root) -> Result<Vec<Result<PathBuf, Fail>>, Fail> {
        Err(Fail::unported(
            "R-01 config",
            "listing active configuration files",
        ))
    }
}

#[derive(Debug)]
pub struct Exceptions {
    _private: (),
}

#[derive(Debug)]
pub struct Exception {
    pub name: String,
    pub path: PathBuf,
    pub install_only: bool,
}

impl Exceptions {
    pub fn read(_root: &Root) -> Result<Self, Fail> {
        Err(Fail::unported(
            "R-01 config",
            "reading installer exceptions",
        ))
    }

    /// Holds apply across every profile, independently of recipe selection.
    pub fn held_by(&self, _identity: &str) -> Option<&str> {
        unreachable!("Exceptions::read is unported")
    }

    pub fn selected(&self, _profile: &str) -> Result<Vec<&Exception>, Fail> {
        Err(Fail::unported(
            "R-01 config",
            "selecting installer exceptions",
        ))
    }
}
