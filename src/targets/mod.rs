use std::path::{Path, PathBuf};

use crate::{Fail, config::Root};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    Active,
    Base,
    Profile(String),
    Path(PathBuf),
}

#[derive(Debug)]
pub struct Targets {
    _private: (),
}

#[derive(Debug)]
pub enum Resolution {
    One(Target),
    Ambiguous(Vec<Target>),
    None(Option<String>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub id: String,
    pub kind: Kind,
    pub aliases: Vec<String>,
    pub declarations: Vec<Declaration>,
    pub held: Option<String>,
    pub owned_apps: Vec<OwnedApp>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Tool,
    Package,
    ReleaseApp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Declaration {
    pub path: PathBuf,
    pub key: String,
    pub section: Section,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    Tools,
    Packages,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedApp {
    pub repo: String,
    pub path: PathBuf,
    pub bundle_id: String,
    pub marker: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Available {
    pub value: String,
    pub description: String,
}

impl Targets {
    /// Strict reads fail on untrustworthy configurations or holds. An explicit
    /// scope only reads the selected file; uninstall decisions also need Active.
    pub fn read(_root: &Root, _scope: Scope, _caller: &Path) -> Result<Self, Fail> {
        Err(Fail::unported("R-01 inventory", "reading removal targets"))
    }

    /// A passive query skips unreadable declarations and offers no targets when holds are unknown.
    pub fn read_for_completion(_root: &Root, _scope: Scope, _caller: &Path) -> Self {
        Self { _private: () }
    }

    pub fn resolve(&self, _request: &str, _keep_installed: bool) -> Resolution {
        unreachable!("Targets::read is unported")
    }

    pub fn available(
        &self,
        _prefix: &str,
        _used: &[String],
        _keep_installed: bool,
    ) -> Vec<Available> {
        // Passive completion cannot offer inventory before the inventory lane lands.
        Vec::new()
    }
}
