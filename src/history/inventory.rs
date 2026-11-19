use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::Ctx;
use crate::config::valid_id;
use crate::process::Mise;
use crate::run::{Run, StageId};

#[derive(Deserialize)]
struct Inventory {
    entries: Vec<IgnoredAny>,
    invalid: Vec<InvalidEntry>,
    // Flattened fields require a JSON object. A derived struct also accepts
    // a positional array by default, including [[], []] for this inventory.
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct InvalidEntry {
    path: PathBuf,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct Status {
    files: Vec<StatusEntry>,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct StatusEntry {
    target: PathBuf,
    mode: String,
    state: String,
    reason: Option<String>,
    #[serde(flatten)]
    _other: HashMap<String, IgnoredAny>,
}

pub(super) fn empty(run: &Run<'_>, stage: StageId, ctx: &Ctx) -> bool {
    inspect(run, stage, ctx).unwrap_or(false)
}

fn inspect(run: &Run<'_>, stage: StageId, ctx: &Ctx) -> Option<bool> {
    let live_state = state_dir(ctx)?;
    if !can_isolate_state(&live_state) {
        return None;
    }
    // Dotfiles dispatch drains pending watcher notices even for paths and
    // status. Keep those calls off both live stores.
    let scratch = tempfile::tempdir().ok()?;
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
            .env_remove("MISE_CD")
            .env_remove("MISE_LOG_FILE")
            .env_remove("__MISE_HISTORY_LOCAL")
            .env("MISE_STATE_DIR", scratch.path())
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
    Some(local.entries.is_empty() && local.invalid.is_empty() && can_isolate_state(&live_state))
}

fn state_dir(ctx: &Ctx) -> Option<PathBuf> {
    let home = ctx.env.home()?;
    if !home.is_absolute() || !home.is_dir() {
        return None;
    }
    let setting = |key| {
        ctx.env
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| {
                let path = Path::new(value);
                path.strip_prefix("~/")
                    .map_or_else(|_| path.to_owned(), |relative| home.join(relative))
            })
    };
    let path = setting("MISE_STATE_DIR").unwrap_or_else(|| {
        setting("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"))
            .join("mise")
    });
    path.is_absolute().then_some(path)
}

fn can_isolate_state(state: &Path) -> bool {
    // A retained history manifest can keep entries absent from today's
    // declarations. Persisted trust, ignore and prompt state can also affect
    // the loaded configuration. Defer if a blank probe could hide any of it.
    [
        "history/repo.git",
        "history-local/history/repo.git",
        "trusted-configs",
        "ignored-configs",
        "vars.toml",
    ]
    .iter()
    .all(|path| matches!(state.join(path).symlink_metadata(), Err(e) if e.kind() == ErrorKind::NotFound))
}

fn inactive(row: &InvalidEntry, status: &Status) -> bool {
    if row.path.as_os_str().is_empty() {
        return false;
    }
    let mut matches = status.files.iter().filter(|entry| entry.target == row.path);
    matches.next().is_some_and(|entry| {
        entry.mode == "track"
            && entry.state == "tracked"
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
