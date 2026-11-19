use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use nix::libc;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, value};

use crate::Fail;
use crate::config::RepoSetting;

use super::native::{Native, exists, expand};
use super::{blocked, invalid};

pub(super) fn read_identity(
    native: &Native<'_, '_>,
    path: &Path,
) -> Result<(PathBuf, Vec<String>), Fail> {
    let path = expand(native.home()?, path)?;
    let path = path.canonicalize().map_err(|_| invalid(format!(
        "age identity {} must be an existing readable regular file; supply an existing identity or generate one with age-keygen",
        path.display()
    )))?;
    if path.starts_with(native.ctx.root.path()) {
        return Err(invalid(
            "keep the private age identity outside the configuration root",
        ));
    }
    if !fs::metadata(&path).is_ok_and(|meta| meta.is_file()) {
        return Err(invalid("age identity must be a readable regular file"));
    }
    let mut cmd = native.command("age-keygen")?;
    cmd.arg("-y").arg(&path);
    let output = native.text(
        cmd,
        "age-keygen could not read public recipients from the supplied identity",
    )?;
    let recipients = output.lines().map(str::to_owned).collect::<Vec<_>>();
    if recipients.is_empty() || recipients.iter().any(|key| !recipient_shape(key)) {
        return Err(invalid(
            "age identity did not provide public age1 recipients",
        ));
    }
    Ok((path, recipients))
}

pub(super) struct LocalSettings {
    path: PathBuf,
    document: DocumentMut,
    original: Option<String>,
}

impl LocalSettings {
    pub fn read(native: &Native<'_, '_>) -> Result<Self, Fail> {
        let path = native.ctx.root.path().join("config.local.toml");
        let original = if exists(&path)? {
            let mut input = String::new();
            use std::io::Read as _;
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
                .and_then(|mut file| {
                    if !file.metadata()?.is_file() {
                        return Err(std::io::Error::other("not a regular file"));
                    }
                    file.read_to_string(&mut input)
                })
                .map_err(|_| invalid("could not read a regular config.local.toml"))?;
            Some(input)
        } else {
            None
        };
        let document = original
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|_| invalid("config.local.toml is not valid TOML"))?;
        Ok(Self {
            path,
            document,
            original,
        })
    }

    pub fn env(&self, name: &str) -> Option<&str> {
        nested(&self.document, &["env", name]).and_then(Item::as_str)
    }

    pub fn origin(&self) -> Option<&str> {
        nested(&self.document, &["history", "origin", "url"]).and_then(Item::as_str)
    }

    pub fn branch(&self) -> Option<&str> {
        nested(&self.document, &["history", "origin", "branch"]).and_then(Item::as_str)
    }

    pub fn recipients(&self) -> Result<BTreeSet<String>, Fail> {
        let Some(item) = nested(&self.document, &["history", "encryption", "recipients"]) else {
            return Ok(BTreeSet::new());
        };
        item.as_array()
            .ok_or_else(|| invalid("config.local.toml history recipients must be an array"))?
            .iter()
            .map(|key| {
                key.as_str()
                    .filter(|key| recipient_shape(key))
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        invalid("config.local.toml history recipients must be public age1 keys")
                    })
            })
            .collect()
    }

    pub fn set_history(
        &mut self,
        branch: &str,
        origin: &RepoSetting,
        identity: &Path,
        recipients: &BTreeSet<String>,
    ) -> Result<(), Fail> {
        let identity = path_text(identity)?;
        self.set_origin(branch, origin)?;
        let root = self.document.as_table_mut();
        let env = table(root, "env")?;
        if !env.contains_key("SETUP_AGE_IDENTITY") {
            env.insert("SETUP_AGE_IDENTITY", value(identity));
        }
        let settings = table(root, "settings")?;
        table(settings, "history")?.insert("sync", value("manual"));
        let age = table(settings, "age")?;
        if !age.contains_key("identity_files") {
            age.insert("identity_files", value(Array::new()));
        }
        let identities = age
            .get_mut("identity_files")
            .and_then(Item::as_array_mut)
            .filter(|files| files.iter().all(|file| file.as_str().is_some()))
            .ok_or_else(|| {
                invalid("config.local.toml age identity files must be an array of paths")
            })?;
        if !identities
            .iter()
            .any(|file| file.as_str() == Some(identity))
        {
            identities.push(identity);
        }
        let mut keys = Array::new();
        for recipient in recipients {
            keys.push(recipient.as_str());
        }
        table(table(root, "history")?, "encryption")?.insert("recipients", value(keys));
        Ok(())
    }

    pub fn set_origin(&mut self, branch: &str, origin: &RepoSetting) -> Result<(), Fail> {
        let origin = origin.transport_url();
        let root = self.document.as_table_mut();
        let env = table(root, "env")?;
        env.insert("SETUP_HISTORY_ORIGIN", value(origin.as_ref()));
        let history = table(root, "history")?;
        let remote = table(history, "origin")?;
        remote.insert("url", value(origin.as_ref()));
        remote.insert("branch", value(branch));
        Ok(())
    }

    pub fn set_recovery_identity(&mut self, identity: &Path) -> Result<(), Fail> {
        table(self.document.as_table_mut(), "env")?
            .insert("SETUP_AGE_IDENTITY", value(path_text(identity)?));
        Ok(())
    }

    pub fn install(self, native: &Native<'_, '_>) -> Result<(), Fail> {
        // Prepare the replacement in the same filesystem and keep it private.
        let root = self
            .path
            .parent()
            .expect("machine-local config has a parent");
        let stage = tempfile::Builder::new()
            .prefix(".dots-history-config-")
            .tempdir_in(root)
            .map_err(|_| blocked("could not stage private history settings"))?;
        let path = stage.path().join("config.local.toml");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|_| blocked("could not stage private history settings"))?;
        file.write_all(self.document.to_string().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|_| blocked("could not write private history settings"))?;
        let mut cmd = native.mise(&["config", "get", "--file"])?;
        cmd.arg(&path);
        native.output(cmd, "mise rejected staged history settings")?;
        let unchanged = match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.is_file() => {
                fs::read_to_string(&self.path).ok() == self.original
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.original.is_none(),
            _ => false,
        };
        if !unchanged {
            return Err(blocked(
                "config.local.toml changed while preparing history settings; retry",
            ));
        }
        fs::rename(&path, &self.path).map_err(|_| blocked("could not install config.local.toml"))
    }
}

pub(super) fn check_recipients(
    native: &Native<'_, '_>,
    recipients: &BTreeSet<String>,
) -> Result<(), Fail> {
    let mut cmd = native.command("age")?;
    for key in recipients {
        if !recipient_shape(key) {
            return Err(invalid("history recipient must be a public age1 key"));
        }
        cmd.arg("-r").arg(key);
    }
    native
        .output(cmd, "could not validate public age recipients")
        .map(|_| ())
        .map_err(|_| {
            invalid("could not validate public age recipients; check age and the configured keys")
        })
}

pub(super) fn recipient_shape(key: &str) -> bool {
    key.len() == 62
        && key.starts_with("age1")
        && key[4..]
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn path_text(path: &Path) -> Result<&str, Fail> {
    path.to_str()
        .ok_or_else(|| invalid("age identity path must be representable in local configuration"))
}

fn nested<'a>(doc: &'a DocumentMut, names: &[&str]) -> Option<&'a Item> {
    let (first, rest) = names.split_first()?;
    rest.iter().try_fold(doc.get(first)?, |item, name| {
        item.as_table_like()?.get(name)
    })
}

fn table<'a>(parent: &'a mut dyn TableLike, name: &str) -> Result<&'a mut dyn TableLike, Fail> {
    if !parent.contains_key(name) {
        parent.insert(name, Item::Table(Table::new()));
    }
    parent
        .get_mut(name)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| invalid("config.local.toml has an invalid history settings section"))
}
