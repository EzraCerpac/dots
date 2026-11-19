use std::path::{Path, PathBuf};

use crate::run::{Issue, Run, StageId};
use crate::{Ctx, Fail};

use super::blocked;
use super::native::{self, Native, exists};
use super::tracked::{self, TrackedPaths};

pub(super) const STAGING: &str = ".dots-restore-install";
pub(super) const MARKER: &str = "dots-restore-pending";
pub(super) const LEGACY_RESTORE: &str = "an older pending restore cannot be safely resumed by this build; finish it with the dots version that started it";

#[derive(Debug)]
pub enum Checkpoint {
    Saved,
    NothingTracked,
}

#[derive(Debug)]
pub enum CheckpointFailure {
    Preflight(Issue),
    NativeSave(Issue),
}

impl CheckpointFailure {
    fn preflight(error: Fail) -> Self {
        Self::Preflight(Issue::new(
            error.to_string(),
            "resolve the local history issue and retry",
        ))
    }

    fn into_issue(self) -> Issue {
        match self {
            Self::Preflight(issue) | Self::NativeSave(issue) => issue,
        }
    }
}

pub fn checkpoint(run: &Run<'_>, stage: StageId, ctx: &Ctx) -> Result<Checkpoint, Issue> {
    checkpoint_classified(run, stage, ctx).map_err(CheckpointFailure::into_issue)
}

pub fn checkpoint_classified(
    run: &Run<'_>,
    _stage: StageId,
    ctx: &Ctx,
) -> Result<Checkpoint, CheckpointFailure> {
    let native = Native::for_checkpoint(run, ctx);
    let preflight = CheckpointFailure::preflight;
    let paths = tracked::capture_paths(&native, None).map_err(preflight)?;
    if paths.is_empty() {
        return Ok(Checkpoint::NothingTracked);
    }
    let store = native.store().map_err(preflight)?;
    let _lock = native::lock(&store).map_err(preflight)?;
    require_no_pending(&store).map_err(preflight)?;
    let cmd = prepare_save(&native, None, &paths).map_err(preflight)?;
    run_save(&native, cmd).map_err(|error| {
        CheckpointFailure::NativeSave(Issue::new(
            error.to_string(),
            "resolve the local save issue and retry",
        ))
    })?;
    Ok(Checkpoint::Saved)
}

pub(super) fn save(
    native: &Native<'_, '_>,
    host: Option<&str>,
    paths: &TrackedPaths,
) -> Result<(), Fail> {
    let cmd = prepare_save(native, host, paths)?;
    run_save(native, cmd)
}

fn prepare_save(
    native: &Native<'_, '_>,
    host: Option<&str>,
    paths: &TrackedPaths,
) -> Result<std::process::Command, Fail> {
    // Path operands promote manual saves; automatic entries are still
    // captured. Recheck the full native inventory under the caller's lock.
    tracked::capture_paths(native, host)?.check_binding(paths.binding(), false)?;
    // Mise recovers interrupted native operations when it takes the save
    // lock. Only an explicit recovery may touch paths outside this inventory.
    native.require_no_pending_operations()?;
    paths.secure(native)?;
    let mut cmd = native.mise(&["bootstrap", "dotfiles", "save"])?;
    cmd.env_remove("__MISE_HISTORY_LOCAL")
        .env("MISE_HISTORY_SYNC", "manual")
        .env("GIT_ALLOW_PROTOCOL", "");
    paths.recheck(native)?;
    native.require_no_pending_operations()?;
    Ok(cmd)
}

fn run_save(native: &Native<'_, '_>, command: std::process::Command) -> Result<(), Fail> {
    let diagnostic = "mise could not save the encrypted settings checkpoint";
    let status = native.probe(command, diagnostic)?.status;
    if status.success() {
        Ok(())
    } else {
        Err(blocked(format!("{diagnostic} ({status})")))
    }
}

pub(super) fn require_no_pending(store: &Path) -> Result<(), Fail> {
    if exists(&store.join(MARKER))?
        || exists(
            &store
                .parent()
                .expect("history store has a parent")
                .join(STAGING),
        )?
    {
        return Err(blocked(
            "a first restore is incomplete; rerun dots restore before saving history",
        ));
    }
    if exists(&legacy_marker(store))? {
        return Err(blocked(LEGACY_RESTORE));
    }
    Ok(())
}

pub(super) fn legacy_marker(store: &Path) -> PathBuf {
    store
        .parent()
        .expect("history store has a parent")
        .join(".repo.git.restore-pending")
}
