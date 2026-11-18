use std::path::{Path, PathBuf};

use crate::run::{Issue, Run, StageId};
use crate::{Ctx, Fail};

use super::native::{self, Native, exists};
use super::{blocked, tracked};

pub(super) const STAGING: &str = ".dots-restore-install";
pub(super) const MARKER: &str = "dots-restore-pending";

#[derive(Debug)]
pub enum Checkpoint {
    Saved,
    NothingTracked,
}

pub fn checkpoint(run: &Run<'_>, _stage: StageId, ctx: &Ctx) -> Result<Checkpoint, Issue> {
    let native = Native::new(run, ctx);
    let checkpoint = || {
        let paths = tracked::capture_paths(&native, None)?;
        if paths.is_empty() {
            return Ok(Checkpoint::NothingTracked);
        }
        let store = native.store()?;
        let _lock = native::lock(&store)?;
        require_no_pending(&store)?;
        save(&native, None, &paths)?;
        Ok(Checkpoint::Saved)
    };
    checkpoint().map_err(|error: Fail| {
        Issue::new(
            error.to_string(),
            "resolve the local history issue and retry",
        )
    })
}

pub(super) fn save(
    native: &Native<'_, '_>,
    host: Option<&str>,
    paths: &[PathBuf],
) -> Result<(), Fail> {
    // Path operands promote manual saves; automatic entries are still
    // captured. Recheck the full native inventory under the caller's lock.
    if tracked::capture_paths(native, host)? != paths {
        return Err(blocked("tracked history paths changed before saving"));
    }
    tracked::secure(native, paths)?;
    let mut cmd = native.mise(&["bootstrap", "dotfiles", "save"])?;
    cmd.env_remove("__MISE_HISTORY_LOCAL")
        .env("MISE_HISTORY_SYNC", "manual")
        .env("GIT_ALLOW_PROTOCOL", "");
    native.output(cmd, "mise could not save the encrypted settings checkpoint")?;
    Ok(())
}

pub(super) fn require_no_pending(store: &Path) -> Result<(), Fail> {
    if exists(&store.join(MARKER))?
        || exists(
            &store
                .parent()
                .expect("history store has a parent")
                .join(STAGING),
        )?
        || exists(&legacy_marker(store))?
    {
        return Err(blocked(
            "a first restore is incomplete; rerun dots restore before saving history",
        ));
    }
    Ok(())
}

pub(super) fn legacy_marker(store: &Path) -> PathBuf {
    store
        .parent()
        .expect("history store has a parent")
        .join(".repo.git.restore-pending")
}
