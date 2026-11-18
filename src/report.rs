use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufReader};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nix::{libc, unistd::geteuid};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::Builder;

use crate::Env;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Command {
    Status,
    Plan,
    Up,
    Sync,
    Apply,
    Test,
    Add,
    Remove,
    Bootstrap,
    Remote,
    Bundle,
    Backup,
    Restore,
    Publish,
}

impl Command {
    pub const ALL: [Self; 14] = [
        Self::Status,
        Self::Plan,
        Self::Up,
        Self::Sync,
        Self::Apply,
        Self::Test,
        Self::Add,
        Self::Remove,
        Self::Bootstrap,
        Self::Remote,
        Self::Bundle,
        Self::Backup,
        Self::Restore,
        Self::Publish,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Plan => "plan",
            Self::Up => "up",
            Self::Sync => "sync",
            Self::Apply => "apply",
            Self::Test => "test",
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Bootstrap => "bootstrap",
            Self::Remote => "remote",
            Self::Bundle => "bundle",
            Self::Backup => "backup",
            Self::Restore => "restore",
            Self::Publish => "publish",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StageId(pub(crate) usize);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub item: String,
    pub before: String,
    pub after: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub diagnostic: String,
    pub next: String,
    pub tail: Option<String>,
    pub log: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_exit: Option<u8>,
}

impl Issue {
    pub fn new(diagnostic: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            diagnostic: diagnostic.into(),
            next: next.into(),
            tail: None,
            log: None,
            terminal_exit: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Completed,
    Deferred,
    Skipped,
    Failed,
    Cancelled,
}

impl Outcome {
    pub fn from_hook_status(status: u8) -> Self {
        match status {
            0 => Self::Completed,
            3 => Self::Deferred,
            4 => Self::Skipped,
            _ => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    pub id: StageId,
    pub label: String,
    pub alongside: bool,
    pub outcome: Option<Outcome>,
    pub changes: Vec<Change>,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub command: Command,
    pub finished_at_ms: u64,
    pub status: u8,
    #[serde(default)]
    pub cancelled: bool,
    pub next: Option<String>,
    pub stages: Vec<Stage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Id(String);

#[derive(Debug)]
pub struct Entry {
    pub id: Id,
    pub command: Command,
    pub finished_at_ms: u64,
    pub status: u8,
}

pub(crate) fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn dir(env: &Env) -> io::Result<PathBuf> {
    let state = env.state_dir()?;
    ensure_directory(&state, 0o022)?;
    let dir = state.join("reports");
    ensure_directory(&dir, 0o077)?;
    Ok(dir)
}

fn ensure_directory(path: &Path, forbidden_mode: u32) -> io::Result<()> {
    check_directory(path, forbidden_mode).or_else(|error| {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
        DirBuilder::new().mode(0o700).recursive(true).create(path)?;
        check_directory(path, forbidden_mode)
    })
}

fn check_directory(path: &Path, forbidden_mode: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.mode() & forbidden_mode != 0
        || metadata.uid() != geteuid().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not an owner-controlled state directory",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let file: File = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.mode() & 0o077 != 0 || metadata.uid() != geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a private file owned by this user",
                path.display()
            ),
        ));
    }
    Ok(serde_json::from_reader(BufReader::new(file))?)
}

pub(crate) fn save(env: &Env, report: &Report) -> io::Result<()> {
    let dir = dir(env)?;
    let mut pending = Builder::new().prefix(".pending-").tempfile_in(&dir)?;
    serde_json::to_writer(pending.as_file_mut(), report)?;
    let token = pending
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("ASCII generated name");
    let final_path = dir.join(format!("{}-{token}.json", report.finished_at_ms));
    pending.persist_noclobber(final_path)?;
    Ok(())
}

pub fn list(env: &Env, command: Option<Command>) -> io::Result<Vec<Entry>> {
    let state = env.state_dir()?;
    let dir = state.join("reports");
    for (path, forbidden_mode) in [(&state, 0o022), (&dir, 0o077)] {
        match check_directory(path, forbidden_mode) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
    }
    let mut reports = Vec::new();
    for file in fs::read_dir(dir)? {
        let file = file?;
        let Some(name) = file
            .file_name()
            .into_string()
            .ok()
            .filter(|name| name.ends_with(".json"))
        else {
            continue;
        };
        let Ok(report) = read_json::<Report>(&file.path()) else {
            continue;
        };
        if command.is_none_or(|wanted| wanted == report.command) {
            reports.push(Entry {
                id: Id(name),
                command: report.command,
                finished_at_ms: report.finished_at_ms,
                status: report.status,
            });
        }
    }
    reports.sort_unstable_by(|a, b| {
        b.finished_at_ms
            .cmp(&a.finished_at_ms)
            .then_with(|| b.id.0.cmp(&a.id.0))
    });
    Ok(reports)
}

pub fn load(env: &Env, id: &Id) -> io::Result<Report> {
    let state = env.state_dir()?;
    check_directory(&state, 0o022)?;
    let dir = state.join("reports");
    check_directory(&dir, 0o077)?;
    read_json(&dir.join(&id.0))
}

/// The saved file identified by an entry returned from `list`.
pub(crate) fn path(env: &Env, id: &Id) -> io::Result<PathBuf> {
    Ok(env.state_dir()?.join("reports").join(&id.0))
}

fn fragment_dir(path: &Path) -> io::Result<&Path> {
    let dir = path
        .parent()
        .filter(|dir| dir.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "stage fragment must have an absolute path",
            )
        })?;
    check_directory(dir, 0o077)?;
    Ok(dir)
}

pub(crate) fn save_fragment(path: &Path, stages: &[Stage]) -> io::Result<()> {
    let dir = fragment_dir(path)?;
    let mut pending = Builder::new().prefix(".fragment-").tempfile_in(dir)?;
    serde_json::to_writer(pending.as_file_mut(), stages)?;
    pending.persist(path)?;
    Ok(())
}

pub(crate) fn load_fragment(path: &Path) -> io::Result<Vec<Stage>> {
    fragment_dir(path)?;
    read_json(path)
}
