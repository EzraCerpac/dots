use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Ctx;
use crate::config::manager_tool;
use crate::config::passive;
use crate::process::{Cwd, Mise};
use crate::run::{Change, Issue, Outcome, Policy, Run, StageId};

use super::{
    Step, Versions, child, issue, json, manager, mise_command, probe, record_changes, step,
};

pub(super) type ToolList = BTreeMap<String, Vec<Tool>>;

#[derive(Deserialize)]
pub(super) struct Tool {
    #[serde(default)]
    pub(super) requested_version: String,
    version: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    pub(super) installed: Option<bool>,
    #[serde(default)]
    pub(super) install_path: PathBuf,
}

pub(super) fn list(run: &Run<'_>, mise: &Mise<'_>) -> Result<ToolList, Issue> {
    json(run, mise_command(run, mise, ["ls", "--current", "--json"]))
}

fn snapshot(tools: &ToolList) -> Versions {
    tools
        .iter()
        .filter_map(|(name, rows)| {
            let versions: BTreeSet<_> = rows
                .iter()
                .filter(|tool| tool.active && tool.installed == Some(true))
                .map(|tool| tool.version.as_str())
                .collect();
            (!versions.is_empty()).then(|| {
                (
                    name.clone(),
                    versions.into_iter().collect::<Vec<_>>().join(", "),
                )
            })
        })
        .collect()
}

fn run_and_record(
    run: &Run<'_>,
    id: StageId,
    mise: &Mise<'_>,
    before: Option<ToolList>,
    cmd: Result<std::process::Command, crate::Fail>,
) -> Step {
    let result = child(run, id, cmd, Policy::Ordinary);
    if !run.cancelled()
        && let Some(before) = before
        && let Ok(after) = list(run, mise)
    {
        record_changes(run, id, snapshot(&before), snapshot(&after));
    }
    result.map(|()| Outcome::Completed)
}

pub(super) fn update(
    ctx: &Ctx,
    run: &Run<'_>,
    mise: &Mise<'_>,
    direct: Option<&manager::Direct>,
) -> (bool, bool) {
    let installed = step(run, "Install declared mise tools", |id| {
        let before = list(run, mise).ok();
        let mut hints = ctx
            .env
            .get("MISE_DISABLE_HINTS")
            .unwrap_or_default()
            .to_os_string();
        if !hints.is_empty() {
            hints.push(",");
        }
        hints.push("system_packages_missing");
        // A filtered mise install may remove the excluded tool's shim.
        // Keep the live shims intact and refresh them with the full toolset.
        let shims = direct
            .map(|_| tempfile::tempdir())
            .transpose()
            .map_err(issue)?;
        let cmd = mise_command(run, mise, ["install"]).map(|mut cmd| {
            cmd.env("MISE_DISABLE_HINTS", hints);
            if let (Some(direct), Some(shims)) = (direct, &shims) {
                direct.exclude_from_install(&mut cmd);
                cmd.env("MISE_SHIMS_DIR", shims.path());
            }
            cmd
        });
        let result = run_and_record(run, id, mise, before, cmd);
        if shims.is_some() {
            let _ = probe(run, mise_command(run, mise, ["reshim"]));
        }
        result
    }) == Some(Outcome::Completed);

    let upgraded = installed
        && step(run, "Upgrade declared mise tools", |id| {
            let before = list(run, mise)?;
            let mut args = upgrade_args(&before);
            if let Some(direct) = direct {
                args.extend(["--exclude".to_owned(), direct.tool.clone()]);
            }
            run_and_record(run, id, mise, Some(before), mise_command(run, mise, &args))
        }) == Some(Outcome::Completed);
    (installed, upgraded)
}

fn upgrade_args(tools: &ToolList) -> Vec<String> {
    let mut args = vec!["upgrade".to_owned(), "--no-prune".to_owned()];
    for (tool, rows) in tools {
        let pinned = |row: &Tool| row.requested_version.starts_with("rev:");
        if !rows.is_empty() && rows.iter().all(pinned) {
            args.extend(["--exclude".to_owned(), tool.clone()]);
        }
    }
    args
}

pub(super) fn text(
    run: &Run<'_>,
    cmd: Result<std::process::Command, crate::Fail>,
) -> Option<String> {
    let result = probe(run, cmd).ok()?;
    let value = String::from_utf8(result).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn self_update_enabled(ctx: &Ctx, path: &Path) -> bool {
    match ctx
        .env
        .get("MISE_SELF_UPDATE_AVAILABLE")
        .and_then(|value| value.to_str())
    {
        Some("1" | "true" | "yes") => return true,
        Some("0" | "false" | "no") => return false,
        _ => (),
    }
    if ctx
        .env
        .get("MISE_SELF_UPDATE_INSTRUCTIONS")
        .is_some_and(|value| !value.is_empty())
    {
        return false;
    }
    let Ok(path) = path.canonicalize() else {
        return true;
    };
    let Some(base) = path.parent().and_then(Path::parent) else {
        return true;
    };
    !["lib", "lib/mise", "lib64/mise"].iter().any(|dir| {
        let dir = base.join(dir);
        dir.join(".disable-self-update").exists()
            || dir.join("mise-self-update-instructions.toml").exists()
    })
}

pub(super) fn update_mise(ctx: &Ctx, run: &Run<'_>, mise: &Mise<'_>) {
    let cmd = mise_command(run, mise, ["self-update", "--yes", "--no-plugins"]);
    let package_managed = cmd.as_ref().is_ok_and(|cmd| {
        !self_update_enabled(ctx, Path::new(cmd.get_program()))
            || text(run, mise_command(run, mise, ["self-update", "--help"]))
                .is_some_and(|help| help.contains("This build of mise has self-update disabled."))
    });
    if package_managed {
        step(
            run,
            "Standalone mise (updated by its package manager)",
            |_| Ok(Outcome::Skipped),
        );
        return;
    }

    step(run, "Standalone mise", |id| {
        let before = text(run, mise_command(run, mise, ["--version"]));
        let result = child(run, id, cmd, Policy::Ordinary);
        if !run.cancelled()
            && let Some(before) = before
            && let Some(after) = text(run, mise_command(run, mise, ["--version"]))
            && before != after
        {
            run.change(
                id,
                Change {
                    item: "mise".to_owned(),
                    before,
                    after,
                },
            );
        }
        result.map(|()| Outcome::Completed)
    });
}

pub(super) enum ManagerSource {
    Tool,
    Formula { name: String, missing: bool },
}

pub(super) enum ManagerBaseline {
    Missing,
    Installed(String),
}

pub(super) fn probe_path(run: &Run<'_>, mise: &Mise<'_>, args: &[&str]) -> Option<PathBuf> {
    let mut bytes = probe(run, mise_command(run, mise, args.iter().copied())).ok()?;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    let path = PathBuf::from(OsString::from_vec(bytes));
    path.is_absolute().then_some(path)
}

fn declared_manager_tool(root: &Path, files: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct ConfigFile {
        path: PathBuf,
    }

    let Ok(files) = serde_json::from_slice::<Vec<ConfigFile>>(files) else {
        return false;
    };
    files
        .into_iter()
        .filter(|file| file.path.is_absolute())
        .filter_map(|file| file.path.canonicalize().ok())
        .filter(|path| path.starts_with(root))
        .filter_map(|path| passive::toml(&path).ok())
        .any(|doc| {
            doc.get("tools")
                .and_then(toml_edit::Item::as_table_like)
                .is_some_and(|table| table.iter().any(|(name, _)| manager_tool(name)))
        })
}

pub(super) fn manager_source(
    ctx: &Ctx,
    run: &Run<'_>,
    mise: &Mise<'_>,
    files: &[u8],
) -> Option<ManagerSource> {
    if run.cancelled() {
        return None;
    }
    if declared_manager_tool(ctx.root.path(), files) {
        return Some(ManagerSource::Tool);
    }
    super::packages::declared_dots_formula(run, mise)
        .map(|(name, missing)| ManagerSource::Formula { name, missing })
}

fn manager_path(run: &Run<'_>, mise: &Mise<'_>, source: &ManagerSource) -> Option<PathBuf> {
    match source {
        ManagerSource::Tool => probe_path(run, mise, &["which", "dots"]),
        ManagerSource::Formula { name, .. } => {
            probe_path(run, mise, &["bootstrap", "packages", "where", name])
                .map(|prefix| prefix.join("bin/dots"))
        }
    }
}

pub(super) fn manager_baseline(
    ctx: &Ctx,
    run: &Run<'_>,
    mise: &Mise<'_>,
    source: &ManagerSource,
) -> Option<ManagerBaseline> {
    if let Some(executable) = manager_path(run, mise, source) {
        return manager_version(ctx, run, executable).map(ManagerBaseline::Installed);
    }
    let missing = match source {
        ManagerSource::Tool => {
            let tools = list(run, mise).ok()?;
            let mut rows = tools
                .iter()
                .filter(|(name, _)| manager_tool(name))
                .flat_map(|(_, rows)| rows)
                .peekable();
            rows.peek().is_some() && rows.all(|row| row.installed == Some(false))
        }
        ManagerSource::Formula { missing, .. } => *missing,
    };
    missing.then_some(ManagerBaseline::Missing)
}

fn manager_version(ctx: &Ctx, run: &Run<'_>, executable: PathBuf) -> Option<String> {
    let cmd = ctx.command(executable, Cwd::Root).map(|mut cmd| {
        cmd.env("DOTS_SKILL_LINKS", "0").arg("--version");
        cmd
    });
    let output = text(run, cmd)?;
    let mut words = output.split_whitespace();
    (words.next()? == "dots").then_some(())?;
    Some(words.next()?.to_owned())
}

pub(super) fn report_manager(
    ctx: &Ctx,
    run: &Run<'_>,
    mise: &Mise<'_>,
    source: Option<ManagerSource>,
    before: Option<ManagerBaseline>,
) {
    step(run, "dots manager", |id| {
        let Some(before) = &before else {
            return Ok(Outcome::Skipped);
        };
        let Some(after) = source
            .as_ref()
            .and_then(|source| manager_path(run, mise, source))
            .and_then(|path| manager_version(ctx, run, path))
        else {
            return Ok(Outcome::Skipped);
        };
        if matches!(before, ManagerBaseline::Installed(version) if version == &after) {
            return Ok(Outcome::Skipped);
        }
        let Some(running) = crate::config_commands::version(&ctx.env)
            .ok()
            .and_then(|version| version.split_whitespace().nth(1).map(str::to_owned))
        else {
            return Ok(Outcome::Skipped);
        };
        if running == after {
            return Ok(Outcome::Skipped);
        }
        run.change(
            id,
            Change {
                item: "dots".to_owned(),
                before: running,
                after: format!("{after}, used from the next run"),
            },
        );
        Ok(Outcome::Completed)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_active_installed_tool_versions_are_reported() {
        let tools = serde_json::from_str(
            r#"{"tool":[
            {"version":"old","installed":true},
            {"version":"missing","active":true},
            {"version":"3","active":true,"installed":true},
            {"version":"2","active":true,"installed":true}
        ]}"#,
        )
        .unwrap();
        assert_eq!(
            snapshot(&tools),
            BTreeMap::from([("tool".to_owned(), "2, 3".to_owned())])
        );
    }

    #[test]
    fn fully_revision_pinned_tools_are_excluded() {
        let tools = serde_json::from_str(
            r#"{
            "pinned":[{"version":"123abc","requested_version":"rev:123abc"}],
            "ordinary":[{"version":"1.2","requested_version":"1"}]
        }"#,
        )
        .unwrap();
        assert_eq!(
            upgrade_args(&tools),
            ["upgrade", "--no-prune", "--exclude", "pinned"]
        );
    }
}
