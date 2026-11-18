mod packages;
mod tools;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::process::Command;
use std::thread;

use crate::config::{Exceptions, Vars, selection};
use crate::history::{self, Checkpoint};
use crate::process::Mise;
use crate::report;
use crate::run::{Change, Issue, Outcome, Policy, Run, Sink, StageId, exit_code};
use crate::{Args, Ctx, Fail, exceptions, hooks, sync};

const NEXT: &str = "Fix the problem above, then rerun dots up.";
const NOT_INSTALLED: &str = "not installed";
type Versions = BTreeMap<String, String>;
type Step = Result<Outcome, Issue>;

pub fn up(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    Config::read(ctx)?;
    Mise::new(ctx)?;
    let mas = args.get_flag("mas");
    let run = Run::begin(ctx, Sink::Report(report::Command::Up)).with_rerun(if mas {
        "dots up --mas"
    } else {
        "dots up"
    });
    update(ctx, &run, mas);

    let failed = run
        .snapshot()
        .stages
        .iter()
        .any(|stage| stage.outcome == Some(Outcome::Failed));
    Ok(run.end(u8::from(failed)))
}

struct Config {
    vars: Vars,
    profile: String,
    exceptions: Exceptions,
}

impl Config {
    fn read(ctx: &Ctx) -> Result<Self, Fail> {
        let vars = Vars::read(&ctx.root)?;
        let selection = selection::read(&ctx.root, &ctx.env)?;
        let exceptions = Exceptions::read(&ctx.root)?;
        Ok(Self {
            vars,
            profile: selection.profile.unwrap_or_default(),
            exceptions,
        })
    }

    fn reload<'a>(ctx: &'a Ctx, run: &Run<'_>) -> Result<(Self, Mise<'a>), Issue> {
        let config = Self::read(ctx).map_err(issue)?;
        let mise = Mise::new(ctx).map_err(issue)?;
        probe(run, mise_command(&mise, ["config", "ls", "--json"]))?;
        Ok((config, mise))
    }
}

fn update(ctx: &Ctx, run: &Run<'_>, mas: bool) {
    if !matches!(
        step(run, "Pre-update settings checkpoint", |id| checkpoint(
            ctx, run, id
        )),
        Some(Outcome::Completed | Outcome::Skipped)
    ) {
        return;
    }

    if run.cancelled() {
        return;
    }
    let source = sync::load(ctx);
    let label = if matches!(source, sync::Loaded::LocalOnly) {
        "Sync configuration (no remote configured)"
    } else {
        "Sync configuration"
    };
    step(run, label, |id| sync_configuration(run, id, source));

    // Read selection and settings again: sync may have installed a different config.
    let mut loaded = None;
    step(run, "Reload configuration", |_| {
        loaded = Some(Config::reload(ctx, run)?);
        Ok(Outcome::Completed)
    });
    let Some((config, mise)) = loaded else { return };

    let manager = tools::manager_source(ctx, run, &mise, &config);
    let manager_before = manager
        .as_ref()
        .and_then(|source| tools::manager_version(ctx, run, &mise, source));
    let mut package_update = None;
    step(run, "Inspect declared packages", |_| {
        package_update = Some(packages::Update::inspect(ctx, run, &mise, &config, mas)?);
        Ok(Outcome::Completed)
    });
    let app_store = package_update
        .as_ref()
        .and_then(|packages| packages.update_routine());

    let valid = thread::scope(|scope| {
        if let Some(store) = app_store
            && let Some(packages) = &package_update
            && !run.cancelled()
        {
            let id = run.start_alongside("Mac App Store updates");
            scope.spawn(move || finish(run, id, packages.update_store(id, &store)));
        }

        let (tools_ok, tools_upgraded) = tools::update(ctx, run, &mise);

        if !validate_configuration(ctx, run) {
            return false;
        }
        if !run.cancelled()
            && let Err(error) = exceptions::run(run, &config.exceptions, exceptions::Mode::Update)
        {
            step(run, "Installer exceptions", |_| Err(issue(error)));
        }

        if !validate_configuration(ctx, run) {
            return false;
        }
        if !run.cancelled() {
            match hooks::list(&ctx.root, hooks::Dir::Update, &config.vars, &config.profile) {
                Ok(hooks) => {
                    for hook in hooks {
                        if run.cancelled() {
                            break;
                        }
                        hooks::run(
                            run,
                            &hook,
                            hooks::HookEnv::Update {
                                tools_ok,
                                tools_upgraded,
                            },
                        );
                        if !validate_configuration(ctx, run) {
                            return false;
                        }
                    }
                }
                Err(error) => {
                    step(run, "List update hooks", |_| Err(issue(error)));
                }
            }
        }
        true
    });

    if !valid {
        return;
    }

    tools::update_mise(ctx, run, &mise);
    tools::report_manager(ctx, run, &mise, manager, manager_before);
    step(run, "Post-update settings checkpoint", |id| {
        checkpoint(ctx, run, id)
    });
}

fn validate_configuration(ctx: &Ctx, run: &Run<'_>) -> bool {
    if run.cancelled() {
        return false;
    }
    match Config::reload(ctx, run) {
        Ok(_) => true,
        Err(error) => {
            step(run, "Validate update configuration", |_| Err(error));
            false
        }
    }
}

fn checkpoint(ctx: &Ctx, run: &Run<'_>, id: StageId) -> Step {
    match history::checkpoint(run, id, ctx)? {
        Checkpoint::Saved => Ok(Outcome::Completed),
        Checkpoint::NothingTracked => Ok(Outcome::Skipped),
    }
}

fn sync_configuration(run: &Run<'_>, id: StageId, source: sync::Loaded) -> Step {
    match source {
        sync::Loaded::LocalOnly => Ok(Outcome::Skipped),
        sync::Loaded::Problem(error) => {
            run.issue(id, error);
            Ok(Outcome::Deferred)
        }
        sync::Loaded::Ready(_) => {
            // P-08/P-09: wire sync::run(Display::Embedded) and consume Receipt's
            // issues and adopted revision once the producer replaces the typed stub.
            run.issue(
                id,
                Issue::new(
                    "The sync producer is not implemented in this build (P-08/P-09).",
                    "Rerun with a build that includes source sync.",
                ),
            );
            Ok(Outcome::Deferred)
        }
    }
}

fn step(run: &Run<'_>, label: &str, f: impl FnOnce(StageId) -> Step) -> Option<Outcome> {
    if run.cancelled() {
        return None;
    }
    let id = run.start(label);
    finish(run, id, f(id))
}

fn finish(run: &Run<'_>, id: StageId, result: Step) -> Option<Outcome> {
    if run.cancelled() {
        // Run owns the interrupted stage and derives the exit status at end.
        return None;
    }
    let outcome = result.unwrap_or_else(|error| {
        run.issue(id, error);
        Outcome::Failed
    });
    run.finish(id, outcome);
    Some(outcome)
}

fn issue(error: impl std::fmt::Display) -> Issue {
    Issue::new(error.to_string(), NEXT)
}

fn mise_command<I, S>(mise: &Mise<'_>, args: I) -> Result<Command, Fail>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = mise.command(args)?;
    cmd.env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
        .env("MISE_SELF_UPDATE_AUTO", "false");
    Ok(cmd)
}

fn child(
    run: &Run<'_>,
    id: StageId,
    cmd: Result<Command, Fail>,
    policy: Policy,
) -> Result<(), Issue> {
    let status = run.run(id, cmd.map_err(issue)?, policy).map_err(issue)?;
    if status.success() {
        Ok(())
    } else {
        Err(issue(format!(
            "Command failed (exit {}).",
            exit_code(status)
        )))
    }
}

fn probe(run: &Run<'_>, cmd: Result<Command, Fail>) -> Result<Vec<u8>, Issue> {
    if run.cancelled() {
        return Err(issue("Update cancelled."));
    }
    let output = run.probe(cmd.map_err(issue)?).map_err(issue)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        let tail = diagnostic
            .lines()
            .last()
            .filter(|line| !line.trim().is_empty());
        Err(issue(tail.map_or_else(
            || format!("Inspection exited with {}.", output.status),
            str::to_owned,
        )))
    }
}

fn json<T: serde::de::DeserializeOwned>(
    run: &Run<'_>,
    cmd: Result<Command, Fail>,
) -> Result<T, Issue> {
    let bytes = probe(run, cmd)?;
    serde_json::from_slice(&bytes).map_err(issue)
}

fn record_changes(run: &Run<'_>, id: StageId, before: Versions, after: Versions) {
    for change in differences(&before, &after) {
        run.change(id, change);
    }
}

fn differences(before: &Versions, after: &Versions) -> Vec<Change> {
    let names = before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>();
    names
        .into_iter()
        .filter_map(|item| {
            let old = before.get(item);
            let new = after.get(item);
            (old != new).then(|| Change {
                item: item.clone(),
                before: old.map_or(NOT_INSTALLED, String::as_str).to_owned(),
                after: new.map_or(NOT_INSTALLED, String::as_str).to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_records_installs_upgrades_and_removals() {
        let versions = |rows: &[(&str, &str)]| {
            rows.iter()
                .map(|&(name, version)| (name.to_owned(), version.to_owned()))
                .collect()
        };
        let before = versions(&[("old", "1"), ("removed", "a"), ("same", "s")]);
        let after = versions(&[("new", "b"), ("old", "2"), ("same", "s")]);
        let changes = differences(&before, &after);
        assert_eq!(
            changes
                .iter()
                .map(|c| (c.item.as_str(), c.before.as_str(), c.after.as_str()))
                .collect::<Vec<_>>(),
            [
                ("new", NOT_INSTALLED, "b"),
                ("old", "1", "2"),
                ("removed", "a", NOT_INSTALLED)
            ]
        );
    }
}
