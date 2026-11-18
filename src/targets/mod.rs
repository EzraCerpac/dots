//! Read-only software inventory shared by removal and shell completion.

mod names;
mod ownership;

use crate::config::{Exceptions, Root, passive};
use crate::{Env, Fail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

pub(crate) use names::{github_repo, safe_text};
#[cfg(target_os = "macos")]
pub(crate) use ownership::{AppInstallation, marker_filename};
pub use ownership::{OwnedApp, OwnershipError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    Active,
    Base,
    Profile(String),
    Path(PathBuf),
}

impl Scope {
    fn files(self, root: &Root, caller: &Path) -> Result<Vec<Result<PathBuf, Fail>>, Fail> {
        let path = match self {
            Self::Active => return passive::active_files(root),
            Self::Base => root.path().join("config.toml"),
            Self::Profile(profile) => {
                if !passive::profiles(root)?.contains(&profile) {
                    return Err(Fail::usage(format!("Unknown profile {profile:?}")));
                }
                root.path().join(format!("config.{profile}.toml"))
            }
            Self::Path(path) => caller.join(path),
        };
        Ok(vec![fs::canonicalize(&path).map_err(|error| {
            Fail::new(1, format!("cannot read {path:?}: {error}"))
        })])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Tool,
    Package,
    ReleaseApp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    Tools,
    Packages,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Declaration {
    pub path: PathBuf,
    pub key: String,
    pub section: Section,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub id: String,
    pub kind: Kind,
    pub aliases: Vec<String>,
    pub declarations: Vec<Declaration>,
    pub held: Option<String>,
    pub owned_apps: Vec<OwnedApp>,
    /// A full removal must fail if ownership cannot be established.
    pub ownership_error: Option<OwnershipError>,
}

impl Target {
    fn new(id: &str, kind: Kind) -> Self {
        Self {
            id: id.to_owned(),
            kind,
            aliases: Vec::new(),
            declarations: Vec::new(),
            held: None,
            owned_apps: Vec::new(),
            ownership_error: None,
        }
    }

    pub fn qualified(&self) -> String {
        if self.id.contains(':') {
            self.id.clone()
        } else {
            format!("mise:{}", self.id)
        }
    }

    fn description(&self) -> String {
        match self.kind {
            Kind::Tool => "mise tool".into(),
            Kind::ReleaseApp => "GitHub app".into(),
            Kind::Package => match self.id.split_once(':').map(|(manager, _)| manager) {
                Some("brew") => "Homebrew formula".into(),
                Some("brew-cask") => "Homebrew cask".into(),
                Some("mas") => "Mac App Store app".into(),
                Some(manager) => format!("{manager} package"),
                None => unreachable!("package identity has a manager"),
            },
        }
    }

    fn offerable(&self, keep_installed: bool) -> bool {
        self.held.is_none()
            && if keep_installed {
                !self.declarations.is_empty()
            } else {
                self.ownership_error.is_none()
            }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Resolution {
    One(Target),
    Ambiguous(Vec<Target>),
    None(Option<String>),
}

#[derive(Debug, Eq, PartialEq)]
pub struct Available {
    pub value: String,
    pub description: String,
}

#[derive(Debug, Default)]
pub struct Targets {
    targets: BTreeMap<String, Target>,
    aliases: BTreeMap<String, BTreeSet<String>>,
    ownership_error: Option<OwnershipError>,
}

impl Targets {
    pub fn read(root: &Root, scope: Scope, caller: &Path) -> Result<Self, Fail> {
        Self::read_inventory(root, scope, caller, false)
    }

    pub fn read_for_completion(root: &Root, scope: Scope, caller: &Path) -> Self {
        Self::read_inventory(root, scope, caller, true).unwrap_or_default()
    }

    fn read_inventory(
        root: &Root,
        scope: Scope,
        caller: &Path,
        partial: bool,
    ) -> Result<Self, Fail> {
        let scoped = scope != Scope::Active;
        let holds = Exceptions::read(root)?;
        let mut all = Self::read_declarations(scope.files(root, caller), partial)?;
        if !scoped || all.targets.keys().any(|id| id.starts_with("github:")) {
            match Env::current().and_then(|env| env.state_dir()) {
                Ok(state) => all.add_ownership(
                    &state.join("github-apps"),
                    Path::new("/Applications"),
                    scoped,
                ),
                Err(error) => {
                    all.ownership_error = Some(OwnershipError::at(Path::new("github-apps"), error))
                }
            }
        }
        all.finish(|id| holds.held_by(id).map(str::to_owned));
        Ok(all)
    }

    fn read_declarations(
        files: Result<Vec<Result<PathBuf, Fail>>, Fail>,
        partial: bool,
    ) -> Result<Self, Fail> {
        let mut all = Self::default();
        let files = match files {
            Err(_) if partial => return Ok(all),
            result => result?,
        };
        for file in files {
            let loaded = file.and_then(|file| passive::toml(&file).map(|doc| (file, doc)));
            match loaded {
                Ok((file, doc)) => all.add_declarations(&file, &doc, partial)?,
                Err(_) if partial => {}
                Err(error) => return Err(error),
            }
        }
        Ok(all)
    }

    /// With an explicit scope, P-01 also reads Active before deciding
    /// whether the software may be uninstalled.
    pub fn get(&self, id: &str) -> Option<&Target> {
        self.targets.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Target> {
        self.targets.values()
    }

    /// Failure to read ownership records whose repository is unknown.
    pub fn ownership_error(&self) -> Option<&OwnershipError> {
        self.ownership_error.as_ref()
    }

    fn add_declarations(
        &mut self,
        path: &Path,
        doc: &DocumentMut,
        partial: bool,
    ) -> Result<(), Fail> {
        let packages = doc
            .get("bootstrap")
            .and_then(|item| item.as_table_like())
            .and_then(|table| table.get("packages"));
        for (section, entry, label) in [
            (Section::Tools, doc.get("tools"), "tools"),
            (Section::Packages, packages, "bootstrap.packages"),
        ] {
            let Some(entry) = entry else { continue };
            let Some(table) = entry.as_table_like() else {
                if partial {
                    continue;
                }
                return Err(Fail::usage(format!("{label} in {path:?} must be a table")));
            };
            for (key, _) in table.iter() {
                let kind = match section {
                    Section::Tools => Kind::Tool,
                    Section::Packages if names::package(key) => Kind::Package,
                    Section::Packages => continue,
                };
                let Some(id) = names::canonical(key) else {
                    continue;
                };
                let target = self
                    .targets
                    .entry(id.to_string())
                    .or_insert_with(|| Target::new(&id, kind));
                target.aliases.extend(names::aliases(key));
                target.declarations.push(Declaration {
                    path: path.to_owned(),
                    key: key.into(),
                    section,
                });
            }
        }
        Ok(())
    }

    fn add_ownership(&mut self, dir: &Path, applications: &Path, scoped: bool) {
        let entries = match ownership::read(dir, applications) {
            Ok(entries) => entries,
            Err(error) => {
                self.ownership_error = Some(error);
                return;
            }
        };
        for entry in entries {
            let record = match entry {
                Ok(record) => record,
                Err(error) => {
                    self.ownership_error.get_or_insert(error);
                    continue;
                }
            };
            if scoped && !self.targets.contains_key(&record.id) {
                continue;
            }
            let target = self
                .targets
                .entry(record.id.clone())
                .or_insert_with(|| Target::new(&record.id, Kind::ReleaseApp));
            let aliases = names::aliases(&record.id).chain(
                [record.alias]
                    .into_iter()
                    .filter(|alias| names::valid(alias)),
            );
            match record.app {
                Ok(app) => {
                    target.kind = Kind::ReleaseApp;
                    target.owned_apps.push(app);
                    target.aliases.extend(aliases);
                }
                Err(error) => {
                    target.ownership_error.get_or_insert(error);
                    for alias in aliases {
                        self.aliases
                            .entry(alias)
                            .or_default()
                            .insert(record.id.clone());
                    }
                }
            }
        }
    }

    fn finish(&mut self, held_by: impl Fn(&str) -> Option<String>) {
        for target in self.targets.values_mut() {
            target.held = held_by(&target.id);
            target.aliases.sort_unstable();
            target.aliases.dedup();
            if target.id.starts_with("github:") && target.ownership_error.is_none() {
                target.ownership_error.clone_from(&self.ownership_error);
            }
            self.aliases
                .entry(names::fold(&target.id))
                .or_default()
                .insert(target.id.clone());
            for alias in &target.aliases {
                self.aliases
                    .entry(alias.clone())
                    .or_default()
                    .insert(target.id.clone());
            }
        }
    }

    fn matching(&self, name: &str) -> Vec<&Target> {
        let Some(name) = names::canonical(name) else {
            return Vec::new();
        };
        if let Some((prefix, tool)) = name.split_once(':')
            && prefix.eq_ignore_ascii_case("mise")
            && !tool.contains(':')
        {
            if prefix == "mise" {
                return self.targets.get(tool).into_iter().collect();
            }
            return Vec::new();
        }
        if name.contains(':')
            && let Some(target) = self.targets.get(name.as_ref())
        {
            return vec![target];
        }
        self.aliases
            .get(&names::fold(&name))
            .into_iter()
            .flatten()
            .map(|id| &self.targets[id])
            .collect()
    }

    pub fn resolve(&self, request: &str, keep_installed: bool) -> Resolution {
        let mut matches = self.matching(request);
        match matches.len() {
            0 => Resolution::None(self.suggestion(request, keep_installed)),
            1 => Resolution::One(matches[0].clone()),
            _ => {
                matches.sort_by_key(|target| target.qualified());
                Resolution::Ambiguous(matches.into_iter().cloned().collect())
            }
        }
    }

    fn matches_only(&self, name: &str, id: &str) -> bool {
        matches!(self.matching(name).as_slice(), [target] if target.id == id)
    }

    fn suggestion(&self, request: &str, keep_installed: bool) -> Option<String> {
        let close = names::Close::new(&names::fold(&names::canonical(request)?));
        let mut suggestion: Option<&str> = None;
        for (alias, ids) in &self.aliases {
            if close.matches(alias) {
                for id in ids {
                    if suggestion.is_some_and(|other| other != id) {
                        return None;
                    }
                    suggestion = Some(id);
                }
            }
        }
        let target = &self.targets[suggestion?];
        if !target.offerable(keep_installed) {
            return None;
        }
        let name = if self.matches_only(&target.id, &target.id) {
            target.id.clone()
        } else {
            target.qualified()
        };
        self.matches_only(&name, &target.id).then_some(name)
    }

    pub fn available(&self, prefix: &str, used: &[String], keep_installed: bool) -> Vec<Available> {
        let prefix = names::fold(prefix);
        let used: BTreeSet<_> = used
            .iter()
            .filter_map(|value| {
                let matches = self.matching(value);
                match matches.as_slice() {
                    [target] => Some(&target.id),
                    _ => None,
                }
            })
            .collect();
        let mut values = BTreeMap::new();
        for target in self
            .iter()
            .filter(|target| target.offerable(keep_installed) && !used.contains(&target.id))
        {
            let mut matched = false;
            let mut ambiguous = false;
            for alias in target
                .aliases
                .iter()
                .filter(|name| name.starts_with(&prefix))
            {
                match self.matching(alias).as_slice() {
                    [found] if found.id == target.id => {
                        values.insert(alias.clone(), target.description());
                        matched = true;
                    }
                    [] => {}
                    _ => ambiguous = true,
                }
            }
            let qualified = target.qualified();
            if (ambiguous || (!matched && names::fold(&qualified).starts_with(&prefix)))
                && self.matches_only(&qualified, &target.id)
            {
                values.insert(qualified, target.description());
            }
        }
        values
            .into_iter()
            .map(|(value, description)| Available { value, description })
            .collect()
    }
}

#[cfg(test)]
mod tests;
