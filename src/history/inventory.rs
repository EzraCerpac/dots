use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::Ctx;
use crate::config::valid_id;
use crate::process::Mise;
use crate::run::{Run, StageId};

#[derive(Deserialize)]
struct Inventory {
    entries: Vec<Value>,
    invalid: Vec<InvalidEntry>,
}

#[derive(Deserialize)]
struct InvalidEntry {
    path: PathBuf,
}

#[derive(Deserialize)]
struct Status {
    files: Vec<StatusEntry>,
}

#[derive(Deserialize)]
struct StatusEntry {
    target: PathBuf,
    mode: String,
    reason: Option<String>,
}

pub(super) fn empty(run: &Run<'_>, stage: StageId, ctx: &Ctx) -> bool {
    inspect(run, stage, ctx).unwrap_or(false)
}

fn inspect(run: &Run<'_>, stage: StageId, ctx: &Ctx) -> Option<bool> {
    let home = ctx.env.home()?;
    if !home.is_absolute() || !home.is_dir() {
        return None;
    }
    let mise = Mise::new(ctx).ok()?;
    let query = |args: &[&str], invalid: &[InvalidEntry]| -> Option<Vec<u8>> {
        let mut command = mise.command_active(args).ok()?;
        for key in ctx
            .env
            .vars
            .keys()
            .filter(|key| key.as_encoded_bytes().starts_with(b"GIT_TRACE"))
        {
            command.env_remove(key);
        }
        command
            .env_remove("MISE_GLOBAL_CONFIG_FILE")
            .env_remove("MISE_CD")
            .env_remove("MISE_LOG_FILE")
            .env_remove("__MISE_HISTORY_LOCAL")
            .env("MISE_OFFLINE", "1")
            .env("MISE_NO_HOOKS", "1")
            .env("MISE_AUTO_UPDATE", "0")
            .env("MISE_HISTORY_SYNC", "manual")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1");
        if !invalid.is_empty() {
            command
                .env("MISE_HISTORY_ENABLED", "false")
                .args(invalid.iter().map(|row| &row.path));
        }
        let output = run.probe_for(stage, command).ok()?;
        (output.status.success() && output.stderr.is_empty()).then_some(output.stdout)
    };
    let shared: Inventory =
        serde_json::from_slice(&query(&["bootstrap", "dotfiles", "paths", "--json"], &[])?).ok()?;
    if !shared.entries.is_empty() {
        return Some(false);
    }
    if !shared.invalid.is_empty() {
        // Native paths can validate an entry before selecting its variant.
        // Only status can prove that a malformed row belongs to another host.
        let status: Status = serde_json::from_slice(&query(
            &["bootstrap", "dotfiles", "status", "--json", "--"],
            &shared.invalid,
        )?)
        .ok()?;
        if shared.invalid.iter().any(|row| !inactive(row, &status)) {
            return Some(false);
        }
    }
    // Native save covers both scopes, including unencrypted track-local rows.
    let local: Inventory = serde_json::from_slice(&query(
        &["bootstrap", "dotfiles", "--local", "paths", "--json"],
        &[],
    )?)
    .ok()?;
    Some(local.entries.is_empty() && local.invalid.is_empty())
}

fn inactive(row: &InvalidEntry, status: &Status) -> bool {
    if row.path.as_os_str().is_empty() {
        return false;
    }
    let mut matches = status.files.iter().filter(|entry| entry.target == row.path);
    matches.next().is_some_and(|entry| {
        entry.mode == "track"
            && entry
                .reason
                .as_deref()
                .and_then(|reason| reason.strip_prefix("no variant matches this machine: "))
                .is_some_and(|variants| {
                    variants
                        .split(", ")
                        .all(|variant| variant.strip_prefix("host-").is_some_and(valid_id))
                })
    }) && matches.next().is_none()
}
