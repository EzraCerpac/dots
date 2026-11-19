use std::fs::{self, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use toml_edit::DocumentMut;

use super::{file_error, physical};
use crate::Fail;

pub struct Doc {
    pub document: DocumentMut,
    requested: PathBuf,
    path: PathBuf,
    original: Option<String>,
    original_mode: Option<u32>,
    mode: u32,
    private: bool,
}

impl Doc {
    pub fn open(path: &Path) -> Result<Self, Fail> {
        let requested = std::path::absolute(path).map_err(|e| file_error(path, e))?;
        let path = physical(&requested).map_err(|e| file_error(path, e))?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Some(metadata),
            Ok(_) => return Err(file_error(&path, "expected a regular file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(file_error(&path, error)),
        };
        let original = metadata
            .as_ref()
            .map(|_| fs::read_to_string(&path))
            .transpose()
            .map_err(|e| file_error(&path, e))?;
        let document: DocumentMut = original
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|e: toml_edit::TomlError| file_error(&path, e.message()))?;
        let original_mode = metadata.map(|m| m.permissions().mode() & 0o7777);
        let mode = original_mode.unwrap_or(0o600);
        Ok(Self {
            document,
            requested,
            path,
            original,
            original_mode,
            mode,
            private: false,
        })
    }

    pub fn install(self) -> Result<(), Fail> {
        self.stage()?.install()
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn make_private(&mut self) {
        self.mode = 0o600;
        self.private = true;
    }

    pub(super) fn unchanged(&self) -> bool {
        self.original.as_deref() == Some(self.document.to_string().as_str())
            && self.original_mode == Some(self.mode)
    }

    pub(super) fn stage(self) -> Result<Staged, Fail> {
        let source = self.document.to_string();
        let changed =
            self.original.as_deref() != Some(&source) || self.original_mode != Some(self.mode);
        let directory = self.staging_dir().map_err(|e| file_error(&self.path, e))?;
        let file = tempfile::Builder::new()
            .prefix(".dots-edit-")
            .suffix(".toml")
            .tempfile_in(directory)
            .and_then(|mut file| {
                file.write_all(source.as_bytes())?;
                file.as_file()
                    .set_permissions(Permissions::from_mode(self.mode))?;
                file.as_file().sync_all()?;
                Ok(file)
            })
            .map_err(|e| file_error(&self.path, e))?;
        Ok(Staged {
            document: self,
            file,
            changed,
        })
    }

    fn staging_dir(&self) -> io::Result<PathBuf> {
        let referent_parent = self.path.parent().expect("absolute file path");
        if !self.private {
            return Ok(referent_parent.to_owned());
        }
        // Sync already keeps the root's .setup-state out of its snapshot.
        let state = self
            .requested
            .parent()
            .expect("absolute file path")
            .join(".setup-state");
        match fs::DirBuilder::new().mode(0o700).create(&state) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&state)?;
        if !metadata.is_dir() || metadata.dev() != fs::metadata(referent_parent)?.dev() {
            return Err(io::Error::other(
                ".setup-state must be a real directory on the same filesystem as the private file",
            ));
        }
        Ok(state)
    }
}

pub(super) struct Staged {
    document: Doc,
    pub(super) file: NamedTempFile,
    changed: bool,
}

impl Staged {
    pub(super) fn check_destination(&self) -> Result<(), Fail> {
        let Doc {
            requested,
            path,
            original,
            original_mode,
            ..
        } = &self.document;
        let unchanged_path = physical(requested).is_ok_and(|actual| actual == *path);
        let unchanged_source = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                *original_mode == Some(metadata.permissions().mode() & 0o7777)
                    && fs::read_to_string(path).ok().as_ref() == original.as_ref()
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => original.is_none(),
            _ => false,
        };
        if !unchanged_path || !unchanged_source {
            return Err(file_error(
                path,
                "configuration changed while staging the edit; retry",
            ));
        }
        Ok(())
    }

    pub(super) fn install(self) -> Result<(), Fail> {
        self.check_destination()?;
        if self.changed {
            let target = &self.document.path;
            if self.document.original.is_some() {
                self.file.persist(target)
            } else {
                self.file.persist_noclobber(target)
            }
            .map_err(|e| file_error(target, e.error))?;
        }
        Ok(())
    }
}
