use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::process::{Mise, is_executable, mise_binary};
use crate::run::{Issue, Policy, Run, StageId};
use crate::{Ctx, Fail};

use super::tools::probe_path;
use super::{child, json, mise_command, probe};

pub(super) struct Tool(PathBuf);

impl Tool {
    pub(super) fn expose(&self, mut command: Command) -> Result<Command, Fail> {
        let path = command
            .get_envs()
            .find_map(|(key, value)| (key == "PATH").then_some(value).flatten());
        let mut paths = path.into_iter().flat_map(std::env::split_paths);
        // Ctx's pinned dots must remain ahead of the real mas and any stale shim.
        let path = std::env::join_paths(
            paths
                .next()
                .into_iter()
                .chain([self.0.clone()])
                .chain(paths.filter(|entry| entry != &self.0)),
        )
        .map_err(|error| Fail::new(1, format!("could not expose mas on PATH: {error}")))?;
        command.env("PATH", path);
        Ok(command)
    }
}

pub(super) fn locate(ctx: &Ctx, run: &Run<'_>, mise: &Mise<'_>) -> Option<Tool> {
    if run.cancelled() {
        return None;
    }
    let mise_file = fs::metadata(mise_binary(&ctx.env).ok()?).ok()?;
    let usable = |candidate: &Path| {
        candidate.is_absolute()
            && is_executable(candidate)
            && candidate.canonicalize().is_ok_and(|real| {
                real.file_name() != Some(OsStr::new("mise"))
                    && fs::metadata(real).is_ok_and(|file| {
                        (file.dev(), file.ino()) != (mise_file.dev(), mise_file.ino())
                    })
            })
    };
    let mised = probe_path(run, mise, &["which", "mas"]);
    let prefix = || {
        probe_path(run, mise, &["bootstrap", "packages", "where", "brew:mas"])
            .map(|prefix| prefix.join("bin/mas"))
            .filter(|path| usable(path))
    };
    mised
        .filter(|path| usable(path))
        .or_else(prefix)
        .or_else(|| {
            ctx.env
                .get("PATH")
                .into_iter()
                .flat_map(std::env::split_paths)
                .map(|dir| ctx.env.cwd().join(dir).join("mas"))
                .find(|path| usable(path))
        })
        .and_then(|path| path.parent().map(|dir| Tool(dir.to_owned())))
}

pub(super) fn brew_can_prepare(run: &Run<'_>, mise: &Mise<'_>) -> Result<bool, Issue> {
    let version = probe(run, mise_command(run, mise, ["--version"]))?;
    if !String::from_utf8_lossy(&version).contains("macos-arm64") {
        return Ok(false);
    }
    let settings: Value = json(
        run,
        mise_command(
            run,
            mise,
            ["settings", "ls", "--all", "--json", "system_packages"],
        ),
    )?;
    let enabled = settings
        .pointer("/system_packages/managers")
        .is_none_or(|setting| {
            setting.is_null()
                || setting
                    .as_array()
                    .is_some_and(|list| list.iter().any(|manager| manager == "brew"))
        });
    Ok(enabled)
}

pub(super) fn prepare(run: &Run<'_>, id: StageId, mise: &Mise<'_>) -> Result<(), Issue> {
    for action in ["apply", "upgrade"] {
        let command = mise_command(
            run,
            mise,
            [
                "bootstrap",
                "packages",
                action,
                "--manager",
                "brew",
                "--yes",
                "brew:mas",
            ],
        )
        .map(|mut command| {
            command.env("MISE_SYSTEM_PACKAGES_MANAGERS", "brew");
            command
        });
        child(run, id, command, Policy::Ordinary)?;
    }
    Ok(())
}
