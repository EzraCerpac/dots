use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use toml_edit::DocumentMut;

use super::{Root, file_error, selection, vars};
use crate::process::{config_paths, mise_binary, quiet_probe};
use crate::{Env, Fail};

const TOML_LIMIT: u64 = 2 << 20;

pub fn toml(path: &Path) -> Result<DocumentMut, Fail> {
    parse(
        path,
        &read_regular(path).map_err(|error| file_error(path, error))?,
    )
}

pub(super) fn optional_toml(path: &Path) -> Result<DocumentMut, Fail> {
    match read_regular(path) {
        Ok(bytes) => parse(path, &bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(error) => Err(file_error(path, error)),
    }
}

fn parse(path: &Path, bytes: &[u8]) -> Result<DocumentMut, Fail> {
    let source = std::str::from_utf8(bytes).map_err(|error| file_error(path, error))?;
    source
        .parse()
        .map_err(|error: toml_edit::TomlError| file_error(path, error.message()))
}

pub(super) fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular file",
        ));
    }
    if metadata.len() > TOML_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration is too large",
        ));
    }
    let mut bytes = Vec::new();
    file.take(TOML_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > TOML_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration is too large",
        ));
    }
    Ok(bytes)
}

pub fn profiles(root: &Root) -> Result<Vec<String>, Fail> {
    let path = root.path().join("config.toml");
    vars::profiles(&toml(&path)?, &path)
}

/// Quiet native discovery, then physical containment and stable deduplication.
/// A failed probe gives no partial list. Callers decide how to handle an
/// individual unresolved path in a successful list.
pub fn active_files(root: &Root) -> Result<Vec<Result<PathBuf, Fail>>, Fail> {
    let env = Env::current().map_err(|error| Fail::usage(error.to_string()))?;
    active_files_in(root, &env)
}

pub(super) fn active_files_in(root: &Root, env: &Env) -> Result<Vec<Result<PathBuf, Fail>>, Fail> {
    let mise = mise_binary(env)?;
    let selected = selection::read(root, env)?;
    let scratch = tempfile::tempdir()
        .map_err(|error| Fail::usage(format!("could not create mise probe directory: {error}")))?;
    let mut command = quiet_probe(&mise, root.path(), scratch.path(), env);
    command.env("MISE_ENV", selected.active_environments());
    let paths = config_paths(command, Duration::from_millis(1200))?;
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for path in paths {
        if !path.is_absolute() {
            return Err(Fail::usage("mise returned a relative configuration path"));
        }
        match path.canonicalize() {
            Ok(path) if path.starts_with(root.path()) => {
                if seen.insert(path.clone()) {
                    results.push(Ok(path));
                }
            }
            Err(error) if path.starts_with(root.path()) => {
                results.push(Err(file_error(&path, error)))
            }
            _ => {}
        }
    }
    Ok(results)
}
