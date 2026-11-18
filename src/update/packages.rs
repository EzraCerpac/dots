use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal};
use std::process::Command;

use serde::Deserialize;

use crate::Ctx;
use crate::config::Exceptions;
use crate::process::{Cwd, Mise};
use crate::run::{Issue, Outcome, Policy, Run, StageId};

use super::{Config, Step, Versions, child, json, mise_command, record_changes, step};

type Status = BTreeMap<String, Manager>;
type PackageNames = BTreeSet<String>;

#[derive(Deserialize)]
struct Manager {
    available: bool,
    #[serde(default)]
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    package: String,
    requested_version: String,
    desired_state: String,
    state: String,
    #[serde(default)]
    installed_version: String,
    #[serde(default)]
    auto_updates: bool,
}

#[derive(Deserialize)]
struct Settings {
    system_packages: Managers,
}

#[derive(Deserialize)]
struct Managers {
    managers: Option<Vec<String>>,
    prefer: Option<Vec<String>>,
}

// P-02 owns the effective package scope. This keeps the status and command
// adapter local until inspect::packages::Scope is published.
struct Scope {
    managers: BTreeSet<String>,
    inventory: Status,
}

impl Scope {
    fn read(run: &Run<'_>, mise: &Mise<'_>) -> Result<Self, Issue> {
        let inventory: Status = json(run, command(run, mise, "", ["status", "--json"]))?;
        if inventory.is_empty() {
            return Ok(Self {
                managers: BTreeSet::new(),
                inventory,
            });
        }
        let settings: Settings = json(
            run,
            mise_command(
                run,
                mise,
                ["settings", "ls", "--all", "--json", "system_packages"],
            ),
        )?;
        let mut names: BTreeSet<_> = inventory
            .keys()
            .filter(|name| *name != "unprefixed")
            .cloned()
            .collect();
        if inventory.contains_key("unprefixed") {
            names.extend(settings.system_packages.prefer.unwrap_or_else(|| {
                [
                    "apt", "dnf", "pacman", "zypper", "apk", "brew", "winget", "scoop", "choco",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            }));
        }
        if let Some(allowed) = settings.system_packages.managers {
            names.retain(|name| allowed.contains(name));
        }
        if !cfg!(target_os = "macos") {
            names.remove("mas");
        }
        Ok(Self {
            managers: names,
            inventory,
        })
    }

    fn includes(&self, manager: &str) -> bool {
        self.managers.contains(manager)
    }

    fn managers(&self) -> impl Iterator<Item = &str> {
        self.managers.iter().map(String::as_str)
    }

    fn has_bare(&self) -> bool {
        self.inventory.contains_key("unprefixed")
    }

    fn declares(&self, manager: &str, package: &str) -> bool {
        self.inventory
            .get(manager)
            .is_some_and(|entry| entry.packages.iter().any(|row| row.package == package))
    }

    fn status(&self, run: &Run<'_>, mise: &Mise<'_>, include_mas: bool) -> Result<Status, Issue> {
        json(
            run,
            self.command_in_run(run, mise, include_mas, ["status", "--json"]),
        )
    }

    fn status_for(
        &self,
        run: &Run<'_>,
        mise: &Mise<'_>,
        manager: &str,
    ) -> Result<Option<Manager>, Issue> {
        let cmd = command(run, mise, manager, ["status", "--json"]).map(|mut command| {
            command.env("MISE_SYSTEM_PACKAGES_PREFER", manager);
            command
        });
        let mut status: Status = json(run, cmd)?;
        Ok(status.remove(manager))
    }

    fn command_in_run<I, S>(
        &self,
        run: &Run<'_>,
        mise: &Mise<'_>,
        include_mas: bool,
        args: I,
    ) -> Result<Command, crate::Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let managers = self
            .managers()
            .filter(|name| include_mas || *name != "mas")
            .collect::<Vec<_>>()
            .join(",");
        command(run, mise, &managers, args)
    }
}

fn command<I, S>(
    run: &Run<'_>,
    mise: &Mise<'_>,
    managers: &str,
    args: I,
) -> Result<Command, crate::Fail>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let prefix = [
        std::ffi::OsStr::new("bootstrap"),
        std::ffi::OsStr::new("packages"),
    ];
    let args: Vec<_> = args.into_iter().collect();
    let mut cmd = mise_command(
        run,
        mise,
        prefix.into_iter().chain(args.iter().map(AsRef::as_ref)),
    )?;
    cmd.env("MISE_SYSTEM_PACKAGES_MANAGERS", managers);
    Ok(cmd)
}

pub(super) struct Update<'a, 'ctx> {
    ctx: &'ctx Ctx,
    run: &'a Run<'ctx>,
    mise: &'a Mise<'ctx>,
    scope: Scope,
    config: &'a Config,
    mas: bool,
}

pub(super) struct Store {
    specs: Specs,
    before: Versions,
    unremovable: Vec<String>,
    tracked: Option<PackageNames>,
}

impl<'a, 'ctx> Update<'a, 'ctx> {
    pub(super) fn inspect(
        ctx: &'ctx Ctx,
        run: &'a Run<'ctx>,
        mise: &'a Mise<'ctx>,
        config: &'a Config,
        mas: bool,
    ) -> Result<Self, Issue> {
        Ok(Self {
            ctx,
            run,
            mise,
            scope: Scope::read(run, mise)?,
            config,
            mas,
        })
    }

    pub(super) fn update_routine(&self) -> Option<Store> {
        if self.scope.managers().next().is_none() || self.run.cancelled() {
            return None;
        }
        let include_mas = self.authorize_store();
        // Only bare keys need a grouped probe to discover mise's assignment.
        // Native actions still use the full preference scope if that probe fails.
        let resolved = self
            .scope
            .has_bare()
            .then(|| self.scope.status(self.run, self.mise, include_mas).ok())
            .flatten();

        for name in self.scope.managers().filter(|name| *name != "mas") {
            if self.run.cancelled() {
                break;
            }
            match self.inspect_manager(name, resolved.as_ref()) {
                Ok(Some(manager)) if manager.available && !manager.packages.is_empty() => {
                    self.update_manager(name, &manager, include_mas, resolved.as_ref());
                }
                Err(error) => {
                    step(self.run, &format!("Inspect {name} packages"), |_| {
                        Err(error)
                    });
                }
                _ => (),
            }
        }
        if !include_mas || self.run.cancelled() {
            return None;
        }

        let mut store = None;
        step(self.run, "Prepare Mac App Store updates", |id| {
            let Some(manager) = self
                .inspect_manager("mas", resolved.as_ref())?
                .filter(|manager| !manager.packages.is_empty())
            else {
                return Ok(Outcome::Skipped);
            };
            if !manager.available {
                self.run.issue(
                    id,
                    Issue::new(
                        "Mac App Store updates need an available mas tool.",
                        "Install or repair mas, then rerun dots up --mas.",
                    ),
                );
                return Ok(Outcome::Deferred);
            }
            let packages: Vec<_> = manager.packages.iter().collect();
            let Some(specs) = self.selection("mas", &packages, false, resolved.is_some())? else {
                return Ok(Outcome::Skipped);
            };
            let tracked = self.tracked_names("mas", &manager, resolved.is_some());
            store = Some(Store {
                specs,
                before: versions("mas", &manager, tracked.as_ref()),
                tracked,
                unremovable: manager
                    .packages
                    .iter()
                    .filter(|pkg| {
                        pkg.desired_state == "absent" && pkg.state == "unexpectedly_installed"
                    })
                    .map(|pkg| format!("mas:{}", pkg.package))
                    .collect(),
            });
            Ok(Outcome::Completed)
        });
        store
    }

    fn inspect_manager(
        &self,
        name: &str,
        resolved: Option<&Status>,
    ) -> Result<Option<Manager>, Issue> {
        let mut status = self.scope.status_for(self.run, self.mise, name)?;
        if let Some(resolved) = resolved {
            let Some(known) = resolved.get(name) else {
                return Ok(None);
            };
            if let Some(status) = &mut status {
                status
                    .packages
                    .retain(|pkg| known.packages.iter().any(|row| row.package == pkg.package));
            }
        }
        Ok(status)
    }

    fn tracked_names(
        &self,
        manager: &str,
        status: &Manager,
        resolved: bool,
    ) -> Option<PackageNames> {
        self.scope.has_bare().then(|| {
            status
                .packages
                .iter()
                .filter(|pkg| resolved || self.scope.declares(manager, &pkg.package))
                .map(|pkg| pkg.package.clone())
                .collect()
        })
    }

    fn selection(
        &self,
        manager: &str,
        packages: &[&Package],
        filtered: bool,
        resolved: bool,
    ) -> Result<Option<Specs>, Issue> {
        let specs = select_rows(
            packages,
            &self.config.exceptions,
            &self.config.profile,
            manager,
            filtered,
        )?;
        if self.scope.has_bare()
            && !resolved
            && specs.as_ref().is_some_and(|specs| !specs.0.is_empty())
            && packages
                .iter()
                .any(|pkg| !self.scope.declares(manager, &pkg.package))
        {
            return Err(Issue::new(
                format!(
                    "Cannot filter the {manager} pass because mise could not resolve unqualified package names."
                ),
                "Qualify the package declarations with manager names or fix the manager status error, then rerun dots up.",
            ));
        }
        Ok(specs)
    }

    fn authorize_store(&self) -> bool {
        if !cfg!(target_os = "macos") || !self.scope.includes("mas") || !self.mas {
            return false;
        }
        step(self.run, "Mac App Store authorization", |id| {
            if self.cached_authorization()
                || (io::stdin().is_terminal()
                    && io::stdout().is_terminal()
                    && child(self.run, id, self.sudo(&["-v"]), Policy::Interactive).is_ok())
            {
                return Ok(Outcome::Completed);
            }
            self.run.issue(
                id,
                Issue::new(
                    "Mac App Store updates need administrator authorization.",
                    "Run dots up --mas in an interactive terminal.",
                ),
            );
            Ok(Outcome::Deferred)
        }) == Some(Outcome::Completed)
    }

    fn sudo(&self, args: &[&str]) -> Result<Command, crate::Fail> {
        self.ctx.command("sudo", Cwd::Root).map(|mut cmd| {
            cmd.args(args);
            cmd
        })
    }

    fn cached_authorization(&self) -> bool {
        super::probe(self.run, self.sudo(&["-n", "-v"])).is_ok()
    }

    fn update_manager(
        &self,
        name: &str,
        manager: &Manager,
        include_mas: bool,
        resolved: Option<&Status>,
    ) {
        let packages: Vec<_> = manager.packages.iter().collect();
        let specs = match self.selection(name, &packages, false, resolved.is_some()) {
            Ok(Some(specs)) => specs,
            Ok(None) => return,
            Err(error) => {
                step(self.run, &format!("{name} packages"), |_| Err(error));
                return;
            }
        };
        let tracked = self.tracked_names(name, manager, resolved.is_some());
        for action in ["apply", "upgrade"] {
            let outcome = step(self.run, &format!("{name} packages: {action}"), |id| {
                let Some(manager) = self
                    .inspect_manager(name, resolved)?
                    .filter(|manager| manager.available)
                else {
                    return Ok(Outcome::Skipped);
                };
                let before = versions(name, &manager, tracked.as_ref());
                let casks;
                let specs = if action == "upgrade" && name == "brew-cask" {
                    let eligible = manager
                        .packages
                        .iter()
                        .filter(|pkg| {
                            pkg.state == "installed"
                                && !pkg.auto_updates
                                && !self.config.vars.self_updating_casks.contains(&pkg.package)
                        })
                        .collect::<Vec<_>>();
                    let Some(selected) = self.selection(
                        name,
                        &eligible,
                        eligible.len() != manager.packages.len(),
                        resolved.is_some(),
                    )?
                    else {
                        return Ok(Outcome::Skipped);
                    };
                    casks = selected;
                    &casks
                } else {
                    &specs
                };
                let policy = if name == "brew-cask" || !cfg!(target_os = "macos") {
                    Policy::Interactive
                } else {
                    Policy::Ordinary
                };
                let result = self.run_packages(id, name, action, specs, include_mas, policy);
                self.record(id, name, before, tracked.as_ref());
                result.map(|()| Outcome::Completed)
            });
            if outcome != Some(Outcome::Completed) {
                break;
            }
        }
    }

    fn run_packages(
        &self,
        id: StageId,
        name: &str,
        action: &str,
        specs: &Specs,
        include_mas: bool,
        policy: Policy,
    ) -> Result<(), Issue> {
        let args = [action, "--manager", name, "--yes"]
            .into_iter()
            .chain(specs.0.iter().map(String::as_str));
        child(
            self.run,
            id,
            self.scope
                .command_in_run(self.run, self.mise, include_mas, args),
            policy,
        )
    }

    pub(super) fn update_store(&self, id: StageId, store: &Store) -> Step {
        for action in ["apply", "upgrade"] {
            if !self.cached_authorization() {
                if !self.run.cancelled() {
                    self.run.issue(id, Issue::new(
                        "Administrator authorization expired before the App Store update finished.",
                        "Run sudo -v, then rerun dots up --mas.",
                    ));
                    self.record(id, "mas", store.before.clone(), store.tracked.as_ref());
                }
                return Ok(Outcome::Deferred);
            }
            if let Err(error) =
                self.run_packages(id, "mas", action, &store.specs, true, Policy::Ordinary)
            {
                self.record(id, "mas", store.before.clone(), store.tracked.as_ref());
                if action == "apply" && !store.unremovable.is_empty() {
                    return Err(Issue::new(
                        format!(
                            "mise could not apply the App Store declarations. mas cannot remove {}.",
                            store.unremovable.join(", ")
                        ),
                        "Remove the apps by hand or delete their absent declarations, then rerun dots up --mas.",
                    ));
                }
                if !self.run.cancelled() && !self.cached_authorization() {
                    self.run.issue(
                        id,
                        Issue::new(
                            "Administrator authorization expired during the App Store update.",
                            "Run sudo -v, then rerun dots up --mas.",
                        ),
                    );
                    return Ok(Outcome::Deferred);
                }
                return Err(error);
            }
        }
        self.record(id, "mas", store.before.clone(), store.tracked.as_ref());
        Ok(Outcome::Completed)
    }

    fn record(&self, id: StageId, name: &str, before: Versions, tracked: Option<&PackageNames>) {
        if !self.run.cancelled()
            && let Ok(Some(manager)) = self.scope.status_for(self.run, self.mise, name)
            && manager.available
        {
            record_changes(self.run, id, before, versions(name, &manager, tracked));
        }
    }
}

#[derive(Debug)]
struct Specs(Vec<String>);

#[cfg(test)]
fn select(
    packages: &[Package],
    exceptions: &Exceptions,
    profile: &str,
    manager: &str,
    filtered: bool,
) -> Result<Option<Specs>, Issue> {
    select_rows(
        &packages.iter().collect::<Vec<_>>(),
        exceptions,
        profile,
        manager,
        filtered,
    )
}

fn select_rows(
    packages: &[&Package],
    exceptions: &Exceptions,
    profile: &str,
    manager: &str,
    filtered: bool,
) -> Result<Option<Specs>, Issue> {
    let mut excluded = filtered;
    let mut requested = Vec::new();
    for pkg in packages {
        let spec = format!("{manager}:{}", pkg.package);
        if exceptions.held_by_for(&spec, profile).is_some() {
            excluded = true;
        } else {
            requested.push((pkg, spec));
        }
    }
    if requested.is_empty() {
        return Ok(None);
    }
    if !excluded {
        return Ok(Some(Specs(Vec::new())));
    }
    // Positional specs are new, desired-present requests. Mise retains pinned
    // versions for macos-app only.
    if let Some((_, spec)) = requested.iter().find(|(pkg, _)| {
        pkg.desired_state != "present"
            || (manager != "macos-app" && pkg.requested_version != "latest")
    }) {
        return Err(Issue::new(
            format!(
                "Cannot update {manager}: mise cannot preserve configured versions and desired states for {spec} while excluding packages from this pass."
            ),
            "Review the manager's declarations and installer exception holds, then rerun dots up.",
        ));
    }
    Ok(Some(Specs(
        requested.into_iter().map(|(_, spec)| spec).collect(),
    )))
}

fn versions(name: &str, manager: &Manager, tracked: Option<&PackageNames>) -> Versions {
    manager
        .packages
        .iter()
        .filter(|pkg| {
            !pkg.installed_version.is_empty()
                && tracked.is_none_or(|names| names.contains(&pkg.package))
        })
        .map(|pkg| {
            (
                format!("{name}:{}", pkg.package),
                pkg.installed_version.clone(),
            )
        })
        .collect()
}

pub(super) fn declared_dots_formula(run: &Run<'_>, mise: &Mise<'_>) -> Option<String> {
    let scope = Scope::read(run, mise).ok()?;
    if !scope.includes("brew") {
        return None;
    }
    let manager = scope
        .status_for(run, mise, "brew")
        .ok()
        .flatten()
        .filter(|manager| manager.available)?;
    let pkg = manager.packages.iter().find(|pkg| {
        pkg.package.rsplit('/').next() == Some("dots")
            && pkg.desired_state == "present"
            && scope.declares("brew", &pkg.package)
    })?;
    Some(format!("brew:{}", pkg.package))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Env, config::Root};
    use std::fs;

    fn exceptions(manifest: &str) -> Exceptions {
        let temp = tempfile::tempdir().unwrap();
        let env = Env {
            vars: [("DOTS_ROOT".into(), temp.path().as_os_str().into())].into(),
            cwd: temp.path().into(),
        };
        fs::write(
            temp.path().join("config.toml"),
            "[vars]\ndots_profiles = 'test other'\n",
        )
        .unwrap();
        let config = Root::resolve(&env).unwrap();
        fs::create_dir(temp.path().join("dots")).unwrap();
        fs::write(temp.path().join("dots/exceptions.tsv"), manifest).unwrap();
        Exceptions::read(&config).unwrap()
    }

    fn package(name: &str, version: &str, desired: &str) -> Package {
        Package {
            package: name.to_owned(),
            requested_version: version.to_owned(),
            desired_state: desired.to_owned(),
            state: "installed".to_owned(),
            installed_version: "1".to_owned(),
            auto_updates: false,
        }
    }

    #[test]
    fn native_only_selection_preserves_explicit_absence_and_pins() {
        let absent = package("retired", "latest", "absent");
        let pinned = package("pinned", "2", "present");
        let exceptions = exceptions("");
        for packages in [vec![absent], vec![pinned]] {
            assert!(
                select(&packages, &exceptions, "test", "pacman", false)
                    .unwrap()
                    .unwrap()
                    .0
                    .is_empty()
            );
        }
    }

    #[test]
    fn excluded_package_cannot_turn_a_pin_or_absence_into_latest() {
        let exceptions = exceptions("base\thold\thold=pacman:held\n");
        let held = package("held", "latest", "present");
        let ordinary = package("ordinary", "latest", "present");
        assert_eq!(
            select_rows(&[&held, &ordinary], &exceptions, "test", "pacman", false)
                .unwrap()
                .unwrap()
                .0,
            ["pacman:ordinary"]
        );
        for unsafe_explicit in [
            package("pinned", "2", "present"),
            package("absent", "latest", "absent"),
        ] {
            let rejected = select_rows(
                &[&held, &unsafe_explicit],
                &exceptions,
                "test",
                "pacman",
                false,
            )
            .unwrap_err();
            assert!(
                rejected
                    .diagnostic
                    .contains(&format!("pacman:{}", unsafe_explicit.package))
            );
        }
    }
    #[test]
    fn holds_from_another_profile_do_not_filter_the_manager() {
        let exceptions = exceptions("other\thold\thold=pacman:held\n");
        let held = package("held", "latest", "present");
        let pinned = package("pinned", "2", "present");
        let specs = select_rows(&[&held, &pinned], &exceptions, "test", "pacman", false)
            .unwrap()
            .unwrap();
        assert!(specs.0.is_empty());
        assert!(select_rows(&[&held, &pinned], &exceptions, "other", "pacman", false).is_err());
    }
}
