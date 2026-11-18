use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tempfile::{Builder, TempDir};

use crate::Env;
use crate::report::{self, Issue, StageId};

const TAIL_BYTES: usize = 8192;
const TAIL_LINES: usize = 8;

#[derive(Default)]
pub(super) struct Logs {
    directory: Option<TempDir>,
    unavailable: bool,
    diagnostics: Vec<(StageId, Diagnostic)>,
    terminal_failures: Vec<(StageId, u8)>,
    warnings: Vec<String>,
}

impl Logs {
    pub(super) fn stage(&mut self, base: &io::Result<PathBuf>, id: StageId) -> Diagnostic {
        if let Some(index) = self.diagnostics.iter().position(|(key, _)| *key == id) {
            let mut diagnostic = self.diagnostics.swap_remove(index).1;
            if let Some(path) = &diagnostic.path {
                match OpenOptions::new().append(true).open(path) {
                    Ok(file) => diagnostic.file = Some(file),
                    Err(error) => {
                        self.warnings.push(warning(&error));
                        diagnostic.incomplete();
                    }
                }
            }
            return diagnostic;
        }
        if self.unavailable {
            return Diagnostic::default();
        }
        let result = (|| {
            if self.directory.is_none() {
                let state = base
                    .as_ref()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let root = logs_root(state)?;
                self.directory = Some(
                    Builder::new()
                        .prefix("run-")
                        .rand_bytes(6)
                        .permissions(fs::Permissions::from_mode(0o700))
                        .tempdir_in(root)?,
                );
            }
            let path = self
                .directory
                .as_ref()
                .unwrap()
                .path()
                .join(format!("{}.log", id.0));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            Ok((path, file))
        })();
        match result {
            Ok((path, file)) => Diagnostic {
                path: Some(path),
                file: Some(file),
                ..Diagnostic::default()
            },
            Err(error) => {
                self.warnings.push(warning(&error));
                self.unavailable = true;
                Diagnostic::default()
            }
        }
    }

    pub(super) fn store(&mut self, id: StageId, mut diagnostic: Diagnostic) {
        diagnostic.file = None;
        if let Some(warning) = diagnostic.warning.take() {
            self.warnings.push(warning);
        }
        self.diagnostics.push((id, diagnostic));
    }

    pub(super) fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    pub(super) fn failed(&mut self, id: StageId, terminal_exit: Option<u8>) {
        self.terminal_failures.retain(|(key, _)| *key != id);
        if let Some(code) = terminal_exit {
            self.terminal_failures.push((id, code));
        }
    }

    pub(super) fn id(&self) -> Option<String> {
        Some(
            self.directory
                .as_ref()?
                .path()
                .file_name()?
                .to_str()?
                .to_owned(),
        )
    }

    pub(super) fn keep(&mut self) {
        if let Some(dir) = self.directory.take() {
            let _ = dir.keep();
        }
    }

    pub(super) fn enrich(&self, id: StageId, issue: &mut Issue) {
        issue.tail = None;
        issue.log = None;
        issue.terminal_exit = self
            .terminal_failures
            .iter()
            .find(|(key, _)| *key == id)
            .map(|(_, code)| *code);
        if issue.terminal_exit.is_some() {
            return;
        }
        let Some((_, diagnostic)) = self.diagnostics.iter().find(|(key, _)| *key == id) else {
            return;
        };
        if !diagnostic.incomplete {
            issue.log = diagnostic.path.clone();
        }
        issue.tail = diagnostic.tail();
    }

    pub(super) fn replay(&self, id: StageId) -> Option<PathBuf> {
        let (_, diagnostic) = self.diagnostics.iter().find(|(key, _)| *key == id)?;
        diagnostic.path.clone()
    }
}

#[derive(Default)]
pub(super) struct Diagnostic {
    path: Option<PathBuf>,
    file: Option<File>,
    incomplete: bool,
    tail: Vec<u8>,
    warning: Option<String>,
}

impl Diagnostic {
    pub(super) fn incomplete(&mut self) {
        self.file = None;
        self.incomplete = true;
    }

    pub(super) fn record(&mut self, bytes: &[u8]) {
        if let Some(file) = &mut self.file
            && let Err(error) = file.write_all(bytes)
        {
            self.warning = Some(warning(&error));
            // A truncated file does not qualify as the step's full log.
            self.incomplete();
        }
        let bytes = &bytes[bytes.len().saturating_sub(TAIL_BYTES)..];
        let excess = (self.tail.len() + bytes.len()).saturating_sub(TAIL_BYTES);
        self.tail.drain(..excess);
        self.tail.extend_from_slice(bytes);
    }

    fn tail(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.tail)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let lines: Vec<_> = text
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(TAIL_LINES)
            .collect();
        (!lines.is_empty()).then(|| lines.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }
}

fn warning(error: &io::Error) -> String {
    format!("dots: could not save the stage log for dots up: {error}")
}

fn logs_root(state: &Path) -> io::Result<PathBuf> {
    report::ensure_directory(state, 0o022)?;
    let root = state.join("logs");
    report::ensure_directory(&root, 0o077)?;
    Ok(root)
}

pub(crate) fn remove_run_logs(env: &Env, id: &str) -> io::Result<()> {
    // Reports store only this generated ID. Issue paths never authorize a removal.
    if !id.strip_prefix("run-").is_some_and(|token| {
        token.len() == 6 && token.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid run log ID",
        ));
    }
    let directory = logs_root(&env.state_dir()?)?.join(id);
    match report::check_directory(&directory, 0o077).and_then(|()| fs::remove_dir_all(directory)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
