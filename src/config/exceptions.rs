use std::path::PathBuf;
use std::{fs, io};

use super::{Root, file_error, passive};
use crate::Fail;
use crate::process::is_executable;

#[derive(Debug)]
pub struct Exception {
    pub(crate) profile: String,
    pub name: String,
    pub path: PathBuf,
    pub install_only: bool,
    holds: Vec<String>,
}

#[derive(Debug)]
pub struct Exceptions(Vec<Exception>);

impl Exceptions {
    pub fn read(root: &Root) -> Result<Self, Fail> {
        let path = root.path().join("dots/exceptions.tsv");
        let bytes = match passive::read_regular(&path) {
            Ok(source) => source,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && fs::symlink_metadata(&path)
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(Self(Vec::new()));
            }
            Err(error) => return Err(file_error(&path, error)),
        };
        let source = std::str::from_utf8(&bytes).map_err(|error| file_error(&path, error))?;
        let profiles = passive::profiles(root)?;
        let mut rows = Vec::new();
        for (index, line) in source.lines().enumerate() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let invalid = || {
                file_error(
                    &path,
                    format_args!("invalid installer exception row {}", index + 1),
                )
            };
            let mut columns = line.split('\t');
            let profile = columns.next().ok_or_else(invalid)?;
            let name = columns.next().ok_or_else(invalid)?;
            if (profile != "base" && !profiles.iter().any(|p| p == profile))
                || !crate::config_commands::valid_name(name)
            {
                return Err(invalid());
            }
            let flags = columns.next().unwrap_or_default();
            if columns.next().is_some() {
                return Err(invalid());
            }
            let mut install_only = false;
            let mut holds = Vec::new();
            for flag in flags.split(',').filter(|_| !flags.is_empty()) {
                if flag == "install-only" {
                    install_only = true;
                } else if let Some(identity) =
                    flag.strip_prefix("hold=").filter(|id| valid_hold(id))
                {
                    holds.push(identity.to_owned());
                } else {
                    return Err(invalid());
                }
            }
            rows.push(Exception {
                profile: profile.to_owned(),
                name: name.to_owned(),
                path: root.path().join("dots/exceptions").join(name),
                install_only,
                holds,
            });
        }
        Ok(Self(rows))
    }

    pub fn held_by(&self, identity: &str) -> Option<&str> {
        let (manager, tail) = identity.split_once(':')?;
        let name = tail.rsplit('/').next()?;
        for key in [name, name.split('@').next()?] {
            let key = format!("{manager}:{key}");
            if let Some(row) = self.0.iter().rev().find(|row| row.holds.contains(&key)) {
                return Some(&row.name);
            }
        }
        None
    }

    pub fn selected(&self, profile: &str) -> Result<Vec<&Exception>, Fail> {
        self.0
            .iter()
            .filter(|row| row.profile == "base" || row.profile == profile)
            .map(|row| {
                if is_executable(&row.path) {
                    Ok(row)
                } else {
                    Err(file_error(
                        &row.path,
                        "installer exception is not executable",
                    ))
                }
            })
            .collect()
    }
}

fn valid_hold(identity: &str) -> bool {
    let Some((manager, name)) = identity.split_once(':') else {
        return false;
    };
    manager
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_lowercase)
        && manager
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"@._/+-".contains(&c))
}
