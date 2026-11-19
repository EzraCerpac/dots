use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, Permissions};
use std::io::{ErrorKind, Read as _};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use crate::Fail;

use super::native::{Native, exists, read_native_file};
use super::{blocked, invalid};

const STORES: [&str; 2] = ["history", "history-local/history"];

pub(super) struct ProbeState {
    scratch: tempfile::TempDir,
    live: PathBuf,
    trust: BTreeMap<OsString, TrustRecord>,
}

#[derive(PartialEq, Eq)]
enum TrustRecord {
    Link(PathBuf),
    Hash(Vec<u8>),
}

impl ProbeState {
    pub(super) fn for_capture(native: &Native<'_, '_>) -> Result<Option<Self>, Fail> {
        if !native.checkpoint {
            return Ok(None);
        }
        let live = native.state_dir()?;
        if has_repository(&live)? {
            for scope in STORES {
                let repo = live.join(scope).join("repo.git");
                if exists(&repo)? {
                    native.bare(&repo)?;
                }
            }
            return Ok(None);
        }
        check_configuration_state(&live)?;
        let trust = trust_records(&live)?;
        let scratch = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .map_err(|_| invalid("could not isolate mise inventory state"))?;
        if !trust.is_empty() {
            let directory = scratch.path().join("trusted-configs");
            let failure = || invalid("could not copy mise trust records for the inventory probe");
            fs::create_dir(&directory).map_err(|_| failure())?;
            for (name, record) in &trust {
                let path = directory.join(name);
                match record {
                    TrustRecord::Link(target) => symlink(target, path),
                    TrustRecord::Hash(hash) => fs::write(path, hash),
                }
                .map_err(|_| failure())?;
            }
        }
        Ok(Some(Self {
            scratch,
            live,
            trust,
        }))
    }

    pub(super) fn path(&self) -> &Path {
        self.scratch.path()
    }

    pub(super) fn recheck(&self) -> Result<(), Fail> {
        check_configuration_state(&self.live)?;
        if has_repository(&self.live)? || trust_records(&self.live)? != self.trust {
            return Err(blocked(
                "mise history or trust state changed during inventory; retry the update",
            ));
        }
        Ok(())
    }
}

fn has_repository(state: &Path) -> Result<bool, Fail> {
    for scope in STORES {
        if exists(&state.join(scope).join("repo.git"))? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_configuration_state(state: &Path) -> Result<(), Fail> {
    if exists(&state.join("ignored-configs"))? || exists(&state.join("vars.toml"))? {
        return Err(invalid(
            "could not isolate mise inventory without hiding persisted ignores or variables",
        ));
    }
    Ok(())
}

fn trust_records(state: &Path) -> Result<BTreeMap<OsString, TrustRecord>, Fail> {
    let failure =
        || invalid("could not mirror the current mise trust records for the inventory probe");
    let directory = state.join("trusted-configs");
    match directory.symlink_metadata() {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Ok(metadata) if metadata.is_dir() => (),
        _ => return Err(failure()),
    }
    // Preserve both native path trust and content-bound trust. Other sidecar
    // types need their own proof.
    fs::read_dir(directory)
        .map_err(|_| failure())?
        .map(|entry| {
            let entry = entry.map_err(|_| failure())?;
            let name = entry.file_name();
            let filename = name.as_encoded_bytes();
            let stem = filename.strip_suffix(b".hash");
            let identity = stem
                .unwrap_or(filename)
                .rsplit(|byte| *byte == b'-')
                .next()
                .ok_or_else(failure)?;
            if identity.is_empty()
                || identity.len() > 16
                || !identity.iter().all(u8::is_ascii_hexdigit)
            {
                return Err(failure());
            }
            let path = entry.path();
            let record = if stem.is_some() {
                let mut hash = Vec::new();
                read_native_file(&path)
                    .map_err(|_| failure())?
                    .take(65)
                    .read_to_end(&mut hash)
                    .map_err(|_| failure())?;
                if hash.len() != 64 || !hash.iter().all(u8::is_ascii_hexdigit) {
                    return Err(failure());
                }
                TrustRecord::Hash(hash)
            } else {
                let target = fs::read_link(path).map_err(|_| failure())?;
                if !target.is_absolute() {
                    return Err(failure());
                }
                TrustRecord::Link(target)
            };
            Ok((name, record))
        })
        .collect()
}
