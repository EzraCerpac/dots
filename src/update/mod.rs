mod app_store;
mod packages;
mod tools;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::process::Command;
use std::thread;

use crate::config::{Exceptions, Vars, selection};
use crate::history::{self, Checkpoint};
use crate::hooks::exceptions;
use crate::process::Mise;
use crate::receipt::{self, Receipt};
use crate::report;
use crate::run::{Change, Issue, Outcome, Policy, Run, Sink, StageId, exit_code};
use crate::{Args, Ctx, Fail, hooks, sync};

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
    update(&run, mas);

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

    fn reload<'a>(ctx: &'a Ctx, run: &Run<'_>) -> Result<(Self, Mise<'a>, Vec<u8>), Issue> {
        let config = Self::read(ctx).map_err(issue)?;
        let mise = Mise::new(ctx).map_err(issue)?;
        let files = probe(run, mise_command(run, &mise, ["config", "ls", "--json"]))?;
        Ok((config, mise, files))
    }
}

fn update(run: &Run<'_>, mas: bool) {
    let ctx = run.ctx();
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
    if matches!(source, sync::Loaded::LocalOnly) {
        run.skip("Sync configuration (no remote configured)");
    } else {
        step(run, "Sync configuration", |id| {
            sync_configuration(run, id, source)
        });
    }

    // Read selection and settings again: sync may have installed a different config.
    let mut loaded = None;
    step(run, "Reload configuration", |_| {
        loaded = Some(Config::reload(ctx, run)?);
        Ok(Outcome::Completed)
    });
    let Some((config, mise, files)) = loaded else {
        return;
    };

    let manager = tools::manager_source(ctx, run, &mise, &files);
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
            if !finish_if_cancelled(run, id) {
                scope.spawn(move || finish(run, id, packages.update_store(id, store)));
            }
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
    let receipt = sync::run(run, id, source, receipt::Display::Embedded);
    Ok(record_sync_receipt(run, id, receipt))
}

fn record_sync_receipt(run: &Run<'_>, id: StageId, receipt: Receipt) -> Outcome {
    if let Some((before, after)) = receipt.adopted_revision {
        run.change(
            id,
            Change {
                item: "configuration revision".into(),
                before,
                after,
            },
        );
    }
    for issue in receipt.issues {
        run.issue(id, Issue::new(issue.diagnostic, issue.next_action));
    }
    receipt.stage_outcome
}

fn step(
    run: &Run<'_>,
    label: impl Into<String>,
    f: impl FnOnce(StageId) -> Step,
) -> Option<Outcome> {
    if run.cancelled() {
        return None;
    }
    let id = run.start(label);
    if finish_if_cancelled(run, id) {
        return Some(Outcome::Cancelled);
    }
    finish(run, id, f(id))
}

fn finish_if_cancelled(run: &Run<'_>, id: StageId) -> bool {
    if !run.cancelled() {
        return false;
    }
    run.finish(id, Outcome::Cancelled);
    true
}

fn finish(run: &Run<'_>, id: StageId, result: Step) -> Option<Outcome> {
    if finish_if_cancelled(run, id) {
        // Run owns the interrupted stage and derives the exit status at end.
        return Some(Outcome::Cancelled);
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

fn mise_command<I, S>(run: &Run<'_>, mise: &Mise<'_>, args: I) -> Result<Command, Fail>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = mise.command_in_run(run, args)?;
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
    if run.cancelled() {
        return Err(issue("Update cancelled."));
    }
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
    use std::path::Path;

    fn context(root: &Path) -> Ctx {
        std::fs::write(root.join("config.toml"), "[vars]\ndots_profiles = 'test'\n").unwrap();
        let env = crate::Env {
            vars: [("DOTS_ROOT".into(), root.as_os_str().into())].into(),
            cwd: root.into(),
        };
        Ctx {
            root: crate::config::Root::resolve(&env).unwrap(),
            env,
            flags: crate::Flags::default(),
        }
    }

    #[test]
    fn denied_stage_admission_does_not_run_the_step() {
        const CHILD: &str = "DOTS_TEST_DENIED_UPDATE_STAGE";
        let fixture = tempfile::tempdir_in("/tmp").unwrap();
        if std::env::var_os(CHILD).is_none() {
            let dirs = [
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
                "CODEX_HOME",
                "CLAUDE_CONFIG_DIR",
                "TMPDIR",
                "MISE_CONFIG_DIR",
                "MISE_DATA_DIR",
                "MISE_STATE_DIR",
                "MISE_CACHE_DIR",
                "MISE_TMP_DIR",
                "MISE_SYSTEM_CONFIG_DIR",
                "MISE_SYSTEM_DATA_DIR",
                "MISE_INSTALLS_DIR",
                "MISE_INSTALL_STORE_DIR",
                "MISE_DOWNLOADS_DIR",
                "MISE_PLUGINS_DIR",
                "MISE_SHIMS_DIR",
                "MISE_SYSTEM_INSTALLS_DIR",
                "MISE_SYSTEM_SHIMS_DIR",
            ];
            // Run the signal handler in a process with only this test active.
            let output = Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .envs(dirs.map(|name| (name, fixture.path())))
                .env(CHILD, "1")
                .current_dir(fixture.path())
                .args([
                    "--exact",
                    "update::tests::denied_stage_admission_does_not_run_the_step",
                ])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }

        struct InterruptOnAdmission;
        impl From<InterruptOnAdmission> for String {
            fn from(_: InterruptOnAdmission) -> Self {
                // Run converts the label after step's first check and before admission.
                assert_eq!(unsafe { nix::libc::raise(nix::libc::SIGINT) }, 0);
                "Interrupted step".into()
            }
        }

        let ctx = context(fixture.path());
        let run = Run::begin(&ctx, Sink::Unreported);
        let outcome = step(&run, InterruptOnAdmission, |_| {
            panic!("denied stage ran its closure");
        });
        assert_eq!(outcome, Some(Outcome::Cancelled));
        assert!(run.snapshot().stages.is_empty());
        assert_eq!(run.end(0), 130);
    }

    #[test]
    fn embedded_sync_records_the_verified_revision_and_issue() {
        let fixture = tempfile::tempdir().unwrap();
        let ctx = context(fixture.path());
        let run = Run::begin(&ctx, Sink::Unreported);
        let id = run.start("Sync configuration");
        let mut receipt = Receipt::new(None, "not pushed");
        receipt.stage_outcome = Outcome::Deferred;
        receipt.adopted_revision = Some(("a".repeat(40), "b".repeat(40)));
        receipt
            .issues
            .push(receipt::Issue::new("Shared update deferred", "dots sync"));

        let outcome = record_sync_receipt(&run, id, receipt);
        assert_eq!(outcome, Outcome::Deferred);
        let snapshot = run.snapshot();
        assert_eq!(
            snapshot.stages[0].changes,
            [Change {
                item: "configuration revision".into(),
                before: "a".repeat(40),
                after: "b".repeat(40),
            }]
        );
        assert_eq!(
            snapshot.stages[0].issues,
            [Issue::new("Shared update deferred", "dots sync")]
        );
    }

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
