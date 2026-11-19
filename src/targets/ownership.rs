use super::names;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fmt::Write;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedApp {
    pub marker: PathBuf,
    pub repo: String,
    pub path: PathBuf,
    pub bundle_id: String,
}

impl OwnedApp {
    pub(crate) fn read_marker(
        marker: &Path,
        applications: &Path,
    ) -> Result<(Self, Option<AppInstallation>), OwnershipError> {
        let record = read_record(marker, applications)?;
        Ok((record.app?, record.installation))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn for_release(
        dir: &Path,
        applications: &Path,
        repo: &str,
        bundle_id: &str,
    ) -> Result<Vec<Self>, OwnershipError> {
        let id = format!("github:{repo}");
        let mut apps = Vec::new();
        for record in read(dir, applications)? {
            let record = record?;
            if record.id == id && record.bundle_id == bundle_id {
                apps.push(record.app?);
            }
        }
        Ok(apps)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppInstallation {
    device: u64,
    inode: u64,
    created: SystemTime,
}

impl AppInstallation {
    pub(crate) fn from_metadata(meta: &fs::Metadata) -> io::Result<Self> {
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            created: meta.created()?,
        })
    }

    pub(crate) fn matches(self, meta: &fs::Metadata) -> bool {
        Self::from_metadata(meta).is_ok_and(|current| current == self)
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
    installation: Option<AppInstallation>,
}

pub(super) struct Record {
    pub id: String,
    pub alias: String,
    pub app: Result<OwnedApp, OwnershipError>,
    #[cfg(target_os = "macos")]
    bundle_id: String,
    installation: Option<AppInstallation>,
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
            .and_then(|_| directory_or_absent(path))
            .and_then(|installed| {
                if let (Some(proof), Some(installed)) = (owner.installation, installed)
                    && !proof.matches(&installed)
                {
                    return Err(OwnershipError::at(
                        path,
                        "app installation identity differs",
                    ));
                }
                Ok(OwnedApp {
                    marker: marker.to_owned(),
                    repo,
                    path: path.to_owned(),
                    bundle_id: owner.bundle_id.clone(),
                })
            })
    };
    Ok(Record {
        id,
        alias: names::fold(alias),
        app,
        #[cfg(target_os = "macos")]
        bundle_id: owner.bundle_id,
        installation: owner.installation,
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

fn directory_or_absent(path: &Path) -> Result<Option<fs::Metadata>, OwnershipError> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() => Ok(Some(info)),
        Ok(_) => Err(OwnershipError::at(path, "expected a real app directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(OwnershipError::at(path, error)),
    }
}
