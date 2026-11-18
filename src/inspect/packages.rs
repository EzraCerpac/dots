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
}

impl Scope {
    pub fn read(run: &Run<'_>, mise: &Mise<'_>) -> Result<Self, Fail> {
        let mut command = mise.command(["bootstrap", "packages", "status", "--json"])?;
        command
            .env("MISE_SYSTEM_PACKAGES_MANAGERS", "")
            .env("MISE_QUIET", "1");
        let inventory: BTreeMap<String, serde::de::IgnoredAny> =
            read_json(run, command, "read declared packages")?;
        if inventory.is_empty() {
            return Ok(Self {
                managers: BTreeSet::new(),
            });
        }
        let command = mise.command(["settings", "ls", "--all", "--json", "system_packages"])?;
        let settings: Effective = read_json(run, command, "read package manager settings")?;
        Ok(Self::from_inventory(
            inventory.into_keys().collect(),
            settings.system_packages,
        ))
    }

    fn from_inventory(mut managers: BTreeSet<String>, settings: Settings) -> Self {
        if managers.remove("unprefixed") {
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
        Self { managers }
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
        let command = mise.command(
            ["bootstrap", "packages"]
                .map(std::ffi::OsString::from)
                .into_iter()
                .chain(args.into_iter().map(|arg| arg.as_ref().to_owned())),
        )?;
        Ok(self.scoped(command, include_mas))
    }

    pub(super) fn scoped(&self, mut command: Command, include_mas: bool) -> Command {
        let allowlist = self
            .managers
            .iter()
            .map(String::as_str)
            .filter(|manager| include_mas || *manager != "mas")
            .collect::<Vec<_>>()
            .join(",");
        command.env("MISE_SYSTEM_PACKAGES_MANAGERS", allowlist);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_packages_add_only_allowed_preferences() {
        let inventory = ["unprefixed", "brew-cask", "mas"].map(str::to_owned).into();
        let settings = Settings {
            managers: Some(vec!["apt".into(), "dnf".into(), "mas".into()]),
            prefer: Some(vec!["apt".into(), "brew".into()]),
        };
        let scope = Scope::from_inventory(inventory, settings);
        assert_eq!(scope.managers, ["apt".to_owned(), "mas".to_owned()].into());
        let plan = scope.scoped(Command::new("mise"), false);
        assert!(plan.get_envs().any(|(key, value)| {
            key == "MISE_SYSTEM_PACKAGES_MANAGERS" && value == Some(OsStr::new("apt"))
        }));
    }
}
