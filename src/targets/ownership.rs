use super::names;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fmt::Write;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedApp {
    pub marker: PathBuf,
    pub repo: String,
    pub path: PathBuf,
    pub bundle_id: String,
}

impl OwnedApp {
    pub(crate) fn read_marker(marker: &Path, applications: &Path) -> Result<Self, OwnershipError> {
        read_record(marker, applications)?.app
    }
}

/// Ownership is independent of declarations and can outlive one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnershipError {
    pub path: PathBuf,
    pub detail: String,
}

impl OwnershipError {
    pub(super) fn at(path: &Path, detail: impl std::fmt::Display) -> Self {
        Self {
            path: path.to_owned(),
            detail: detail.to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    repo: String,
    path: String,
    bundle_id: String,
}

pub(super) struct Record {
    pub id: String,
    pub alias: String,
    pub app: Result<OwnedApp, OwnershipError>,
}

pub(super) fn read(
    dir: &Path,
    applications: &Path,
) -> Result<impl Iterator<Item = Result<Record, OwnershipError>>, OwnershipError> {
    let entries = match fs::symlink_metadata(dir) {
        Ok(info) if info.is_dir() => {
            Some(fs::read_dir(dir).map_err(|error| OwnershipError::at(dir, error))?)
        }
        Ok(_) => return Err(OwnershipError::at(dir, "expected an ownership directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(OwnershipError::at(dir, error)),
    };

    Ok(entries.into_iter().flatten().filter_map(move |entry| {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Some(Err(OwnershipError::at(dir, error))),
        };
        if !entry.file_name().as_encoded_bytes().ends_with(b".json") {
            return None;
        }
        let path = entry.path();
        Some(match entry.file_type() {
            Ok(kind) if kind.is_file() => read_record(&path, applications),
            Ok(_) => Err(OwnershipError::at(
                &path,
                "expected a regular ownership record",
            )),
            Err(error) => Err(OwnershipError::at(&path, error)),
        })
    }))
}

fn read_record(marker: &Path, applications: &Path) -> Result<Record, OwnershipError> {
    let error = |detail| OwnershipError::at(marker, detail);
    let mut bytes = Vec::new();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(marker)
        .map_err(|err| error(err.to_string()))?;
    if !file
        .metadata()
        .map_err(|err| error(err.to_string()))?
        .is_file()
    {
        return Err(error("expected a regular ownership record".into()));
    }
    const LIMIT: u64 = 16 * 1024;
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| error(err.to_string()))?;
    if bytes.len() as u64 > LIMIT {
        return Err(error("ownership record exceeds 16 KiB".into()));
    }
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(error("ownership record must be a JSON object".into()));
    }
    let owner: Marker = serde_json::from_slice(&bytes).map_err(|err| error(err.to_string()))?;
    let path = Path::new(&owner.path);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let Some(alias) = name.strip_suffix(".app") else {
        return Err(error("expected an app bundle path".into()));
    };
    if !path.is_absolute() || owner.bundle_id.is_empty() {
        return Err(error("invalid ownership record".into()));
    }
    let repo = names::github_repo(&owner.repo)
        .ok_or_else(|| error("invalid GitHub repository in ownership record".into()))?;
    let id = format!("github:{repo}");

    // A schema-valid record still participates in name ambiguity. Only a
    // verified marker makes a standalone app eligible for completion.
    let app = if matches!(alias, "" | ".")
        || !names::safe_text(&owner.bundle_id)
        || !names::safe_text(&owner.path)
        || path.as_os_str() != applications.join(name).as_os_str()
        || marker.file_name() != Some(OsStr::new(&marker_filename(name)))
    {
        Err(error(
            "invalid marker filename, app path or bundle identifier".into(),
        ))
    } else {
        // A valid marker is useful even when the app is already absent. P-01
        // must revalidate the bundle and its identifier before moving it.
        directory_or_absent(applications)
            .and_then(|()| directory_or_absent(path))
            .map(|()| OwnedApp {
                marker: marker.to_owned(),
                repo,
                path: path.to_owned(),
                bundle_id: owner.bundle_id,
            })
    };
    Ok(Record {
        id,
        alias: names::fold(alias),
        app,
    })
}

pub(crate) fn marker_filename(bundle: &str) -> String {
    let mut filename = String::with_capacity(69);
    for byte in Sha256::digest(bundle.as_bytes()) {
        write!(filename, "{byte:02x}").expect("writing to a String cannot fail");
    }
    filename.push_str(".json");
    filename
}

fn directory_or_absent(path: &Path) -> Result<(), OwnershipError> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() => Ok(()),
        Ok(_) => Err(OwnershipError::at(path, "expected a real app directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OwnershipError::at(path, error)),
    }
}
