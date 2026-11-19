use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal};
use std::process::Command;

use crate::Ctx;
use crate::config::Exceptions;
use crate::inspect::packages::{Manager, Package, Scope};
use crate::process::{Cwd, Mise};
use crate::run::{Issue, Outcome, Policy, Run, StageId};

use super::app_store::{self, Tool};
use super::{Config, Step, Versions, child, issue, json, mise_command, record_changes, step};

type Status = BTreeMap<String, Manager>;
type PackageNames = BTreeSet<String>;

fn read_scope(run: &Run<'_>, mise: &Mise<'_>) -> Result<Scope, Issue> {
    Scope::read_with(run, |args| mise_command(run, mise, args)).map_err(issue)
}

fn scoped_status_command(
    run: &Run<'_>,
    mise: &Mise<'_>,
    scope: &Scope,
    manager: &str,
) -> Result<Command, crate::Fail> {
    let command = mise_command(run, mise, ["bootstrap", "packages", "status", "--json"])?;
    Ok(scope.scoped_for(command, manager))
}

fn scoped_status(
    run: &Run<'_>,
    mise: &Mise<'_>,
    scope: &Scope,
    manager: &str,
) -> Result<Option<Manager>, Issue> {
    let mut status: Status = json(run, scoped_status_command(run, mise, scope, manager))?;
    Ok(status.remove(manager))
}

fn grouped_status(
    run: &Run<'_>,
    mise: &Mise<'_>,
    scope: &Scope,
    mas: Option<&Tool>,
) -> Result<Status, Issue> {
    let command =
        mise_command(run, mise, ["bootstrap", "packages", "status", "--json"]).map_err(issue)?;
    json(run, expose_mas(scope.scoped(command, mas.is_some()), mas))
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
    apply: Command,
    upgrade: Command,
    status: Command,
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
            scope: read_scope(run, mise)?,
            config,
            mas,
        })
    }

    pub(super) fn update_routine(&self) -> Option<Store> {
        if self.scope.managers().next().is_none() || self.run.cancelled() {
            return None;
        }
        let store_tool = self.prepare_store_tool();
        let mas = store_tool.as_ref().map(|(tool, _)| tool);
        // Only bare keys need a grouped probe to discover mise's assignment.
        // Native actions still use the full preference scope if that probe fails.
        let resolved = match self
            .scope
            .has_bare()
            .then(|| grouped_status(self.run, self.mise, &self.scope, mas))
            .transpose()
        {
            Ok(resolved) => resolved,
            Err(error) => {
                step(self.run, "Resolve unqualified packages", |id| {
                    self.run.issue(id, error);
                    Ok(Outcome::Deferred)
                });
                None
            }
        };

        for name in self.scope.managers().filter(|name| *name != "mas") {
            if self.run.cancelled() {
                break;
            }
            match self.inspect_manager(name, resolved.as_ref(), None) {
                Ok(Some(manager)) if manager.available && !manager.packages.is_empty() => {
                    self.update_manager(name, &manager, mas, resolved.as_ref());
                }
                Err(error) => {
                    step(self.run, format!("Inspect {name} packages"), |_| Err(error));
                }
                _ => (),
            }
        }
        let (mas, already_authorized) = store_tool.as_ref().filter(|_| !self.run.cancelled())?;
        let mut store = None;
        step(self.run, "Prepare Mac App Store updates", |id| {
            let Some(manager) = self
                .inspect_manager("mas", resolved.as_ref(), Some(mas))?
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
                apply: self
                    .package_command("mas", "apply", &specs, Some(mas))
                    .map_err(issue)?,
                upgrade: self
                    .package_command("mas", "upgrade", &specs, Some(mas))
                    .map_err(issue)?,
                status: scoped_status_command(self.run, self.mise, &self.scope, "mas")
                    .and_then(|command| mas.expose(command))
                    .map_err(issue)?,
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
        store.filter(|_| *already_authorized || self.authorize_store())
    }

    fn inspect_manager(
        &self,
        name: &str,
        resolved: Option<&Status>,
        mas: Option<&Tool>,
    ) -> Result<Option<Manager>, Issue> {
        let unresolved = self.scope.has_bare() && resolved.is_none();
        // A narrow inspection can force bare declarations onto this manager.
        // Native apply retains the original preference and needs real work here.
        if unresolved && self.scope.declared_names(name).next().is_none() {
            return Ok(None);
        }
        let mut status = if let Some(mas) = mas {
            let command = scoped_status_command(self.run, self.mise, &self.scope, name)
                .and_then(|command| mas.expose(command));
            let mut status: Status = json(self.run, command)?;
            status.remove(name)
        } else {
            scoped_status(self.run, self.mise, &self.scope, name)?
        };
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
        Ok(status.filter(|manager| {
            !unresolved
                || manager
                    .packages
                    .iter()
                    .any(|pkg| self.scope.declares(name, &pkg.package))
        }))
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
        let Some(specs) = select_rows(
            packages,
            &self.config.exceptions,
            &self.config.profile,
            manager,
            filtered,
        )?
        else {
            return Ok(None);
        };
        if specs.0.is_empty() {
            return Ok(Some(specs));
        }
        if self.scope.has_bare()
            && !resolved
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
        if manager == "brew-cask" {
            if let Some(spec) = specs.0.iter().find(|spec| {
                spec.strip_prefix("brew-cask:")
                    .is_some_and(|name| !self.scope.declares(manager, name))
            }) {
                return Err(Issue::new(
                    format!(
                        "Cannot filter the brew-cask pass: a positional {spec} cannot preserve appdir or adopt from its unqualified declaration."
                    ),
                    "Qualify the cask declaration as brew-cask:NAME, then rerun dots up.",
                ));
            }
            // A positional parent can install excluded cask dependencies.
            // Mise builds its appdir and adopt options from the named specs.
            if let Some(held) = packages
                .iter()
                .map(|pkg| format!("brew-cask:{}", pkg.package))
                .find(|spec| {
                    self.config
                        .exceptions
                        .held_by_for(spec, &self.config.profile)
                        .is_some()
                })
            {
                return Err(Issue::new(
                    format!(
                        "Cannot filter the brew-cask pass: mise can install held {held} as a dependency without preserving its appdir or adopt settings."
                    ),
                    "Manage the dependent cask through an installer exception, or remove the hold if mise can manage it, then rerun dots up.",
                ));
            }
        }
        Ok(Some(specs))
    }

    fn prepare_store_tool(&self) -> Option<(Tool, bool)> {
        if !cfg!(target_os = "macos") || !self.scope.includes("mas") || !self.mas {
            return None;
        }
        let eligible_declarations = self.scope.declared_names("mas").any(|name| {
            self.config
                .exceptions
                .held_by_for(&format!("mas:{name}"), &self.config.profile)
                .is_none()
        });
        if !eligible_declarations && !self.scope.has_bare() {
            return None;
        }
        let mut tool = app_store::locate(self.ctx, self.run, self.mise);
        if let Some(tool) = tool {
            return Some((tool, false));
        }
        // A missing manager cannot receive bare declarations through mise's
        // built-in availability check. Only qualified work can provision it.
        if !eligible_declarations {
            return None;
        }
        if tool.is_none() {
            let can_prepare = self.brew_can_prepare_store_tool();
            match can_prepare {
                Ok(true) => (),
                Ok(false) => {
                    step(self.run, "Mac App Store tool", |id| {
                        Ok(self.defer_store_tool(id))
                    });
                    return None;
                }
                Err(error) => {
                    step(self.run, "Mac App Store tool", |_| Err(error));
                    return None;
                }
            }
        }
        if !self.authorize_store() {
            return None;
        }
        if tool.is_none() {
            step(self.run, "Mac App Store tool", |id| {
                app_store::prepare(self.run, id, self.mise)?;
                tool = app_store::locate(self.ctx, self.run, self.mise);
                Ok(if tool.is_some() {
                    Outcome::Completed
                } else {
                    self.defer_store_tool(id)
                })
            });
        }
        tool.map(|tool| (tool, true))
    }

    fn brew_can_prepare_store_tool(&self) -> Result<bool, Issue> {
        if self
            .config
            .exceptions
            .held_by_for("brew:mas", &self.config.profile)
            .is_some()
            || !app_store::brew_can_prepare(self.run, self.mise)?
        {
            return Ok(false);
        }
        if self.scope.includes("brew")
            && let Some(manager) = scoped_status(self.run, self.mise, &self.scope, "brew")?
            && manager.packages.iter().any(|pkg| {
                pkg.package.rsplit('/').next() == Some("mas")
                    && (pkg.desired_state != "present" || pkg.requested_version != "latest")
            })
        {
            return Ok(false);
        }
        Ok(true)
    }

    fn defer_store_tool(&self, id: StageId) -> Outcome {
        if !self.run.cancelled() {
            self.run.issue(
                id,
                Issue::new(
                    "Mac App Store updates need mas to be installed and available.",
                    "Install or repair mas, then rerun dots up --mas.",
                ),
            );
        }
        Outcome::Deferred
    }

    fn authorize_store(&self) -> bool {
        step(self.run, "Mac App Store authorization", |id| {
            if self.cached_authorization()
                || (io::stdin().is_terminal()
                    && io::stdout().is_terminal()
                    && child(self.run, id, self.sudo(&["-v"]), Policy::Interactive).is_ok())
            {
                return Ok(Outcome::Completed);
            }
            if !self.run.cancelled() {
                self.run.issue(
                    id,
                    Issue::new(
                        "Mac App Store updates need administrator authorization.",
                        "Run dots up --mas in an interactive terminal.",
                    ),
                );
            }
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
        mas: Option<&Tool>,
        resolved: Option<&Status>,
    ) {
        let packages: Vec<_> = manager.packages.iter().collect();
        let specs = match self.selection(name, &packages, false, resolved.is_some()) {
            Ok(Some(specs)) => specs,
            Ok(None) => return,
            Err(error) => {
                step(self.run, format!("{name} packages"), |_| Err(error));
                return;
            }
        };
        let tracked = self.tracked_names(name, manager, resolved.is_some());
        for action in ["apply", "upgrade"] {
            let outcome = step(self.run, format!("{name} packages: {action}"), |id| {
                let Some(manager) = self
                    .inspect_manager(name, resolved, None)?
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
                let result = child(
                    self.run,
                    id,
                    self.package_command(name, action, specs, mas),
                    policy,
                );
                self.record(id, name, before, tracked.as_ref());
                result.map(|()| Outcome::Completed)
            });
            if outcome != Some(Outcome::Completed) {
                break;
            }
        }
    }

    fn package_command(
        &self,
        name: &str,
        action: &str,
        specs: &Specs,
        mas: Option<&Tool>,
    ) -> Result<Command, crate::Fail> {
        let args = [action, "--manager", name, "--yes"]
            .into_iter()
            .chain(specs.0.iter().map(String::as_str));
        let cmd = mise_command(
            self.run,
            self.mise,
            ["bootstrap", "packages"].into_iter().chain(args),
        )?;
        expose_mas(self.scope.scoped(cmd, mas.is_some()), mas)
    }

    pub(super) fn update_store(&self, id: StageId, store: Store) -> Step {
        let Store {
            apply,
            upgrade,
            status,
            before,
            unremovable,
            tracked,
        } = store;
        let result = self.update_store_packages(id, apply, upgrade, &unremovable);
        if !self.run.cancelled()
            && let Ok(output) = self.run.probe_for(id, status)
            && output.status.success()
            && let Ok(mut state) = serde_json::from_slice::<Status>(&output.stdout)
            && let Some(manager) = state.remove("mas").filter(|manager| manager.available)
        {
            record_changes(
                self.run,
                id,
                before,
                versions("mas", &manager, tracked.as_ref()),
            );
        }
        result
    }

    fn update_store_packages(
        &self,
        id: StageId,
        apply: Command,
        upgrade: Command,
        unremovable: &[String],
    ) -> Step {
        for (action, command) in [("apply", apply), ("upgrade", upgrade)] {
            if !self.worker_authorized(id) {
                return Ok(self.defer_store_authorization(id));
            }
            if let Err(error) = child(self.run, id, Ok(command), Policy::Ordinary) {
                if action == "apply" && !unremovable.is_empty() {
                    return Err(Issue::new(
                        format!(
                            "mise could not apply the App Store declarations. mas cannot remove {}.",
                            unremovable.join(", ")
                        ),
                        "Remove the apps by hand or delete their absent declarations, then rerun dots up --mas.",
                    ));
                }
                if !self.worker_authorized(id) {
                    return Ok(self.defer_store_authorization(id));
                }
                return Err(error);
            }
        }
        Ok(Outcome::Completed)
    }

    fn worker_authorized(&self, id: StageId) -> bool {
        !self.run.cancelled()
            && self.sudo(&["-n", "-v"]).is_ok_and(|command| {
                self.run
                    .probe_for(id, command)
                    .is_ok_and(|output| output.status.success())
            })
    }

    fn defer_store_authorization(&self, id: StageId) -> Outcome {
        if !self.run.cancelled() {
            self.run.issue(id, Issue::new(
                "Cached administrator authorization is unavailable in the App Store worker.",
                "Run sudo -v and retry dots up --mas. If authorization is limited to your terminal, update the declared apps in the App Store.",
            ));
        }
        Outcome::Deferred
    }

    fn record(&self, id: StageId, name: &str, before: Versions, tracked: Option<&PackageNames>) {
        if !self.run.cancelled()
            && let Ok(Some(manager)) = scoped_status(self.run, self.mise, &self.scope, name)
            && manager.available
        {
            record_changes(self.run, id, before, versions(name, &manager, tracked));
        }
    }
}

fn expose_mas(command: Command, mas: Option<&Tool>) -> Result<Command, crate::Fail> {
    match mas {
        Some(tool) => tool.expose(command),
        None => Ok(command),
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
    let scope = read_scope(run, mise).ok()?;
    if !scope.includes("brew") {
        return None;
    }
    let manager = scoped_status(run, mise, &scope, "brew")
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
            reason: None,
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
