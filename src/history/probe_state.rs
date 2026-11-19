use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::Fail;

use super::native::{Native, exists, read_native_file};
use super::{blocked, invalid};

const STORES: [&str; 2] = ["history", "history-local/history"];

pub(super) struct ProbeState {
    scratch: tempfile::TempDir,
    live: PathBuf,
    trust: BTreeMap<OsString, PathBuf>,
}

impl ProbeState {
    pub(super) fn for_capture(native: &Native<'_, '_>) -> Result<Option<Self>, Fail> {
        if !native.checkpoint {
            return Ok(None);
        }
        let live = native.state_dir()?;
        if has_repository(&live)? {
            check_queued_notices(native)?;
            for scope in STORES {
                let repo = live.join(scope).join("repo.git");
                if exists(&repo)? {
                    native.bare(&repo)?;
                }
            }
            return Ok(None);
        }
        check_configuration_state(&live)?;
        let trust = trust_links(&live)?;
        let scratch =
            tempfile::tempdir().map_err(|_| invalid("could not isolate mise inventory state"))?;
        if !trust.is_empty() {
            let directory = scratch.path().join("trusted-configs");
            let failure = || invalid("could not copy mise trust records for the inventory probe");
            fs::create_dir(&directory).map_err(|_| failure())?;
            for (name, target) in &trust {
                symlink(target, directory.join(name)).map_err(|_| failure())?;
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
        if has_repository(&self.live)? || trust_links(&self.live)? != self.trust {
            return Err(blocked(
                "mise history or trust state changed during inventory; retry the update",
            ));
        }
        Ok(())
    }
}

pub(super) fn check_queued_notices(native: &Native<'_, '_>) -> Result<(), Fail> {
    let live = native.state_dir()?;
    let unknown = || invalid("could not inspect the mise history notice queue");
    for scope in STORES {
        match read_native_file(&live.join(scope).join("notices")) {
            Ok(file) if file.metadata().map_err(|_| unknown())?.len() == 0 => (),
            Ok(_) => {
                return Err(blocked(
                    "mise history has queued notices; read them with mise dotfiles paths in the corresponding scope, then retry",
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(_) => return Err(unknown()),
        }
    }
    Ok(())
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

fn trust_links(state: &Path) -> Result<BTreeMap<OsString, PathBuf>, Fail> {
    let failure =
        || invalid("could not mirror the current mise trust records for the inventory probe");
    let directory = state.join("trusted-configs");
    match directory.symlink_metadata() {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Ok(metadata) if metadata.is_dir() => (),
        _ => return Err(failure()),
    }
    // Preserve native path trust, including the link created by mise install.
    // Hash and monorepo sidecar files require a different proof.
    fs::read_dir(directory)
        .map_err(|_| failure())?
        .map(|entry| {
            let entry = entry.map_err(|_| failure())?;
            let name = entry.file_name();
            let hash = name
                .as_encoded_bytes()
                .rsplit(|byte| *byte == b'-')
                .next()
                .ok_or_else(failure)?;
            let target = fs::read_link(entry.path()).map_err(|_| failure())?;
            if hash.is_empty()
                || hash.len() > 16
                || !hash.iter().all(u8::is_ascii_hexdigit)
                || !target.is_absolute()
            {
                return Err(failure());
            }
            Ok((name, target))
        })
        .collect()
}
