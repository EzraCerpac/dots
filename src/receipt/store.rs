use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use nix::{libc, unistd::geteuid};
use serde::{Deserialize, Serialize};

use super::Receipt;
use crate::Env;

const VERSION: u32 = 1;
pub(super) const MAX_RECEIPT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Stored<T> {
    version: u32,
    root: Vec<u8>,
    receipt: T,
}

fn directory(path: &Path, forbidden: u32, create: bool) -> io::Result<()> {
    if create {
        DirBuilder::new().mode(0o700).recursive(true).create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && metadata.uid() == geteuid().as_raw() && metadata.mode() & forbidden == 0
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "receipt directory is not private",
        ))
    }
}

fn location(env: &Env, root: &Path, create: bool) -> io::Result<PathBuf> {
    let state = env.state_dir()?;
    directory(&state, 0o022, create)?;
    let receipts = state.join("receipts");
    directory(&receipts, 0o077, create)?;
    // Stable filename across builds. The recorded root must also match on replay.
    let key = root
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    Ok(receipts.join(format!("{key:016x}.json")))
}

pub(super) fn save(env: &Env, root: &Path, receipt: &Receipt) -> io::Result<()> {
    let path = location(env, root, true)?;
    let mut pending = tempfile::Builder::new()
        .prefix(".pending-")
        .tempfile_in(path.parent().expect("receipt path includes its directory"))?;
    serde_json::to_writer(
        pending.as_file_mut(),
        &Stored {
            version: VERSION,
            root: root.as_os_str().as_encoded_bytes().to_vec(),
            receipt,
        },
    )?;
    if pending.as_file().metadata()?.len() > MAX_RECEIPT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sync receipt exceeds the storage limit",
        ));
    }
    pending.as_file().sync_all()?;
    pending.persist(path)?;
    Ok(())
}

pub(super) fn load(env: &Env, root: &Path) -> io::Result<Option<Receipt>> {
    let open = || {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(location(env, root, false)?)
    };
    let file = match open() {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sync receipt is not a private file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sync receipt exceeds the storage limit",
        ));
    }
    let stored: Stored<serde_json::Value> = serde_json::from_slice(&bytes)?;
    if stored.version != VERSION || stored.root != root.as_os_str().as_encoded_bytes() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value(stored.receipt)?))
}

pub(super) mod optional_path {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_ref()
            .map(|p| p.as_os_str().as_encoded_bytes())
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        let bytes = Option::<Vec<u8>>::deserialize(deserializer)?;
        Ok(bytes.map(|bytes| OsString::from_vec(bytes).into()))
    }
}

pub(super) mod paths {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(paths.iter().map(|p| p.as_os_str().as_encoded_bytes()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        let paths = Vec::<Vec<u8>>::deserialize(deserializer)?;
        Ok(paths
            .into_iter()
            .map(|bytes| OsString::from_vec(bytes).into())
            .collect())
    }
}
