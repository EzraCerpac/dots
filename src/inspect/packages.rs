use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::process::Command;

use serde::Deserialize;

use crate::Fail;
use crate::process::Mise;
use crate::run::Run;

use super::read_json;

const DEFAULT_PREFER: &[&str] = &[
    "apt", "dnf", "pacman", "zypper", "apk", "brew", "winget", "scoop", "choco",
];

#[derive(Debug, Deserialize)]
pub struct Manager {
    pub available: bool,
    pub reason: Option<String>,
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    pub package: String,
    pub requested_version: String,
    pub desired_state: String,
    pub state: String,
    pub installed_version: String,
    #[serde(default)]
    pub auto_updates: bool,
    pub reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct Settings {
    managers: Option<Vec<String>>,
    prefer: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
struct Effective {
    #[serde(default)]
    system_packages: Settings,
}

/// Restrict native probes and actions to managers selected by effective mise
/// settings and declarations. For bare names, mise still resolves the manager.
pub struct Scope {
    managers: BTreeSet<String>,
    declarations: BTreeMap<String, BTreeSet<String>>,
    has_bare: bool,
}

#[derive(Deserialize)]
struct Inventory {
    packages: Vec<Declared>,
}

#[derive(Deserialize)]
struct Declared {
    package: String,
}

impl Scope {
    pub fn read(run: &Run<'_>, mise: &Mise<'_>) -> Result<Self, Fail> {
        Self::read_with(run, |args| mise.command_active(args))
    }

    /// Use the caller's command preparation when it supervises mise setup
    /// probes such as locating the installed Ruby for Homebrew.
    pub fn read_with(
        run: &Run<'_>,
        mut prepare: impl FnMut(&[&str]) -> Result<Command, Fail>,
    ) -> Result<Self, Fail> {
        let mut command = prepare(&["bootstrap", "packages", "status", "--json"])?;
        command
            .env("MISE_SYSTEM_PACKAGES_MANAGERS", "")
            .env("MISE_QUIET", "1");
        let inventory: BTreeMap<String, Inventory> =
            read_json(run, command, "read declared packages")?;
        let settings: Effective = if inventory.is_empty() {
            Effective::default()
        } else {
            let command = prepare(&["settings", "ls", "--all", "--json", "system_packages"])?;
            read_json(run, command, "read package manager settings")?
        };
        let declarations = inventory
            .into_iter()
            .map(|(manager, items)| {
                (
                    manager,
                    items.packages.into_iter().map(|p| p.package).collect(),
                )
            })
            .collect();
        Ok(Self::from_inventory(declarations, settings.system_packages))
    }

    fn from_inventory(
        mut declarations: BTreeMap<String, BTreeSet<String>>,
        settings: Settings,
    ) -> Self {
        let has_bare = declarations
            .remove("unprefixed")
            .is_some_and(|items| !items.is_empty());
        let mut managers: BTreeSet<_> = declarations.keys().cloned().collect();
        if has_bare {
            managers.extend(settings.prefer.unwrap_or_else(|| {
                DEFAULT_PREFER
                    .iter()
                    .map(|manager| (*manager).into())
                    .collect()
            }));
        }
        if let Some(allowed) = settings.managers {
            managers.retain(|manager| allowed.contains(manager));
        }
        Self {
            managers,
            declarations,
            has_bare,
        }
    }

    pub fn managers(&self) -> impl Iterator<Item = &str> {
        self.managers.iter().map(String::as_str)
    }

    pub fn has_bare(&self) -> bool {
        self.has_bare
    }

    pub fn declares(&self, manager: &str, package: &str) -> bool {
        self.declarations
            .get(manager)
            .is_some_and(|names| names.contains(package))
    }

    pub(crate) fn declared_names(&self, manager: &str) -> impl Iterator<Item = &str> {
        self.declarations
            .get(manager)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    pub fn includes(&self, manager: &str) -> bool {
        self.managers.contains(manager)
    }

    pub fn status(
        &self,
        run: &Run<'_>,
        mise: &Mise<'_>,
        include_mas: bool,
    ) -> Result<BTreeMap<String, Manager>, Fail> {
        let command = self.command(mise, include_mas, ["status", "--json"])?;
        read_json(run, command, "inspect declared packages")
    }

    /// Probe a named manager independently. Use grouped status when assigning
    /// bare declarations to the first available manager in the original preferences.
    pub fn status_for(
        &self,
        run: &Run<'_>,
        mise: &Mise<'_>,
        manager: &str,
    ) -> Result<Option<Manager>, Fail> {
        if !self.includes(manager) {
            return Ok(None);
        }
        let command = mise.command_active(["bootstrap", "packages", "status", "--json"])?;
        let command = self.scoped_for(command, manager);
        let mut state: BTreeMap<String, Manager> =
            read_json(run, command, &format!("inspect {manager} packages"))?;
        Ok(state.remove(manager))
    }

    pub fn command<I, S>(
        &self,
        mise: &Mise<'_>,
        include_mas: bool,
        args: I,
    ) -> Result<Command, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let command = mise.command_active(
            ["bootstrap", "packages"]
                .map(std::ffi::OsString::from)
                .into_iter()
                .chain(args.into_iter().map(|arg| arg.as_ref().to_owned())),
        )?;
        Ok(self.scoped(command, include_mas))
    }

    pub fn scoped(&self, mut command: Command, include_mas: bool) -> Command {
        let allowlist = self
            .managers()
            .filter(|manager| include_mas || *manager != "mas")
            .collect::<Vec<_>>()
            .join(",");
        command.env("MISE_SYSTEM_PACKAGES_MANAGERS", allowlist);
        command
    }

    pub fn scoped_for(&self, mut command: Command, manager: &str) -> Command {
        command
            .env("MISE_SYSTEM_PACKAGES_MANAGERS", manager)
            .env("MISE_SYSTEM_PACKAGES_PREFER", manager);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_packages_add_only_allowed_preferences() {
        let inventory = [
            ("unprefixed".to_owned(), ["bare".to_owned()].into()),
            ("brew-cask".to_owned(), ["fixture".to_owned()].into()),
            ("mas".to_owned(), ["123456".to_owned()].into()),
        ]
        .into();
        let settings = Settings {
            managers: Some(vec!["apt".into(), "dnf".into(), "mas".into()]),
            prefer: Some(vec!["apt".into(), "brew".into()]),
        };
        let scope = Scope::from_inventory(inventory, settings);
        assert_eq!(scope.managers().collect::<Vec<_>>(), ["apt", "mas"]);
        assert!(scope.has_bare());
        assert!(scope.declares("brew-cask", "fixture"));
        assert!(scope.declares("mas", "123456"));
        assert!(!scope.declares("apt", "bare"));
        let named = scope.scoped_for(Command::new("mise"), "mas");
        for name in [
            "MISE_SYSTEM_PACKAGES_MANAGERS",
            "MISE_SYSTEM_PACKAGES_PREFER",
        ] {
            assert!(
                named
                    .get_envs()
                    .any(|(key, value)| key == name && value == Some(OsStr::new("mas")))
            );
        }
        let plan = scope.scoped(Command::new("mise"), false);
        assert!(plan.get_envs().any(|(key, value)| {
            key == "MISE_SYSTEM_PACKAGES_MANAGERS" && value == Some(OsStr::new("apt"))
        }));
    }
}
