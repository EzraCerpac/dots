use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use nix::libc;
use serde::Deserialize;
use toml_edit::DocumentMut;

use crate::Fail;

use super::native::{Native, expand};
use super::{blocked, invalid};

#[derive(Deserialize)]
struct Inventory {
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    mode: String,
    path: PathBuf,
    variant: Option<String>,
    declared_in: Option<PathBuf>,
}

pub(super) fn paths(native: &Native<'_, '_>, host: Option<&str>) -> Result<Vec<PathBuf>, Fail> {
    let cmd = native.mise(&["bootstrap", "dotfiles", "paths", "--json"])?;
    let inventory: Inventory =
        serde_json::from_slice(&native.output(cmd, "could not read native tracked history paths")?)
            .map_err(|_| invalid("mise returned an invalid tracked history inventory"))?;
    let home = native.home()?;
    let physical_home = home
        .canonicalize()
        .map_err(|_| invalid("could not resolve HOME"))?;
    let mut sources = HashMap::new();
    let mut paths = BTreeSet::new();

    for entry in inventory
        .entries
        .into_iter()
        .filter(|entry| entry.mode == "track")
    {
        let target = target(home, &physical_home, &entry.path)?;
        let declared = entry.declared_in.ok_or_else(|| {
            invalid(format!(
                "history path {} has no active encrypted declaration",
                entry.path.display()
            ))
        })?;
        let declaring_file = expand(home, &declared)?;
        if !sources.contains_key(&declaring_file) {
            let doc = fs::read_to_string(&declaring_file)
                .ok()
                .and_then(|text| text.parse::<DocumentMut>().ok())
                .ok_or_else(|| {
                    invalid(format!(
                        "could not read dotfile declarations in {}",
                        declared.display()
                    ))
                })?;
            sources.insert(declaring_file.clone(), doc);
        }
        let dotfiles = sources[&declaring_file]
            .get("dotfiles")
            .and_then(|item| item.as_table_like())
            .ok_or_else(|| {
                invalid(format!(
                    "missing dotfile declarations in {}",
                    declared.display()
                ))
            })?;
        let mut found = false;
        for (spelling, declaration) in dotfiles.iter() {
            if lexical_target(home, &physical_home, Path::new(spelling))
                .as_ref()
                .ok()
                != Some(&target)
            {
                continue;
            }
            found = true;
            if !declaration
                .as_table_like()
                .and_then(|entry| entry.get("encrypt"))
                .and_then(|item| item.as_bool())
                .unwrap_or(false)
            {
                return Err(invalid(format!(
                    "history entry {} in {} must declare encrypt = true",
                    entry.path.display(),
                    declared.display()
                )));
            }
        }
        if !found {
            return Err(invalid(format!(
                "could not verify the encrypted history entry {} in {}",
                entry.path.display(),
                declared.display()
            )));
        }
        if let Some(host) = host
            && entry.variant.as_deref() != Some(host)
        {
            return Err(invalid(format!(
                "history entry {} in {} must be active through variant {host}",
                entry.path.display(),
                declared.display()
            )));
        }
        paths.insert(target);
    }
    Ok(paths.into_iter().collect())
}

fn lexical_target(home: &Path, physical_home: &Path, path: &Path) -> Result<PathBuf, Fail> {
    let absolute = expand(home, path)?;
    let relative = absolute
        .strip_prefix(home)
        .or_else(|_| absolute.strip_prefix(physical_home))
        .map_err(|_| invalid("a tracked history path lies outside HOME"))?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid(
            "a tracked history path has an invalid HOME-relative destination",
        ));
    }
    Ok(physical_home.join(relative))
}

fn target(home: &Path, physical_home: &Path, path: &Path) -> Result<PathBuf, Fail> {
    let target = lexical_target(home, physical_home, path)?;
    let mut cursor = target.as_path();
    while cursor != physical_home {
        match fs::symlink_metadata(cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "refusing symlink in tracked history path {}",
                    cursor.display()
                )));
            }
            Ok(meta) if cursor == target && !meta.is_file() => {
                return Err(invalid(format!(
                    "tracked history path is not a regular file: {}",
                    cursor.display()
                )));
            }
            Ok(meta) if cursor != target && !meta.is_dir() => {
                return Err(invalid(format!(
                    "tracked history path has a non-directory parent: {}",
                    cursor.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(blocked("could not inspect a tracked history destination")),
        }
        cursor = cursor.parent().expect("validated HOME ancestry");
    }
    Ok(target)
}

pub(super) fn secure(paths: &[PathBuf]) -> Result<(), Fail> {
    let opened = paths
        .iter()
        .filter_map(|path| {
            match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
            {
                Ok(file) if file.metadata().is_ok_and(|meta| meta.is_file()) => Some(Ok(file)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                _ => Some(Err(blocked(
                    "could not safely open a tracked history file to protect its mode",
                ))),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    for file in opened {
        file.set_permissions(Permissions::from_mode(0o600))
            .map_err(|_| blocked("could not set tracked history file mode to 0600"))?;
    }
    Ok(())
}
