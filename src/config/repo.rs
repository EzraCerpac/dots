use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Output;

use crate::Fail;

#[derive(Debug)]
pub struct RepoSetting {
    github: Option<String>,
    value: String,
}

impl RepoSetting {
    pub fn parse(value: &str) -> Result<Self, Fail> {
        if value.starts_with('-') {
            return Err(Fail::usage("a repository may not start with -"));
        }
        if value
            .split_once(':')
            .is_some_and(|(_, rest)| rest.starts_with(':'))
        {
            return Err(Fail::usage(
                "refusing a remote-helper address (TRANSPORT::ADDRESS)",
            ));
        }
        let invalid = || Fail::usage("expected OWNER/NAME or a Git URL");
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(invalid());
        }
        let credential = || Fail::usage("refusing a URL that carries a password or token");
        let github = if let Some((scheme, rest)) = value.split_once("://") {
            if !matches!(scheme, "https" | "http" | "ssh" | "git" | "file") {
                return Err(invalid());
            }
            let (authority, path) = rest.split_once('/').ok_or_else(invalid)?;
            if let Some((user, _)) = authority.rsplit_once('@')
                && (scheme != "ssh" || user.contains([':', '%', '@']))
            {
                return Err(credential());
            }
            if path.split_once('?').is_some_and(|(_, query)| {
                query.split('&').any(|entry| {
                    let key = entry
                        .split('=')
                        .next()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    key.contains("token") || key.contains("password") || key == "key"
                })
            }) {
                return Err(credential());
            }
            if path.is_empty()
                || authority.chars().any(char::is_whitespace)
                || (authority.is_empty() && scheme != "file")
            {
                return Err(invalid());
            }
            match (scheme, authority) {
                ("https", "github.com") | ("ssh", "git@github.com") => {
                    Some(github_name(path).ok_or_else(invalid)?)
                }
                _ => None,
            }
        } else if let Some((host, path)) = value.split_once(':') {
            if host.is_empty()
                || host.contains('/')
                || host.chars().any(char::is_whitespace)
                || path.is_empty()
            {
                return Err(invalid());
            }
            if host == "git@github.com" {
                Some(github_name(path).ok_or_else(invalid)?)
            } else {
                None
            }
        } else {
            Some(repository_name(value).ok_or_else(invalid)?)
        };
        Ok(Self {
            github,
            value: value.to_owned(),
        })
    }

    /// The OWNER/NAME of a GitHub repository, preserving the configured case.
    pub fn github(&self) -> Option<&str> {
        self.github.as_deref()
    }

    pub fn matches(&self, url: &str) -> bool {
        let Ok(other) = Self::parse(url) else {
            return false;
        };
        match (&self.github, &other.github) {
            (Some(expected), Some(actual)) => expected.eq_ignore_ascii_case(actual),
            (None, None) => self.value == other.value,
            _ => false,
        }
    }

    pub fn transport_url(&self) -> Cow<'_, str> {
        if self.value.contains(':') {
            Cow::Borrowed(&self.value)
        } else {
            Cow::Owned(format!("https://github.com/{}", self.value))
        }
    }
}

fn github_name(path: &str) -> Option<String> {
    repository_name(path.strip_suffix(".git").unwrap_or(path))
}

fn repository_name(path: &str) -> Option<String> {
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty()
        && owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && !repo.is_empty()
        && repo
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)))
    .then(|| path.to_owned())
}

#[derive(Debug)]
pub struct CheckoutShape {
    pub head: Option<OsString>,
    pub has_commit: bool,
    pub jj_workspace: bool,
    pub legacy_sync: bool,
    pub operations: Vec<&'static str>,
    pub git_too_old: bool,
    pub common_dir: PathBuf,
}

/// Inspect a Git worktree without changing it. The caller supplies its
/// read-only Git launcher rooted at the worktree.
pub fn checkout_shape(
    root: &Path,
    mut git: impl FnMut(&[&str]) -> Result<Output, Fail>,
) -> Result<CheckoutShape, Fail> {
    let mut read = |args: &[&str], absent_ok: bool| -> Result<Option<Vec<u8>>, Fail> {
        let out = git(args)?;
        match out.status.code() {
            Some(0) => Ok(Some(out.stdout)),
            Some(1) if absent_ok => Ok(None),
            _ => Err(Fail::new(1, "Git could not inspect the checkout shape")),
        }
    };
    let head = read(&["symbolic-ref", "--no-recurse", "-q", "HEAD"], true)?
        .map(|out| OsString::from_vec(unline(&out).to_vec()));
    let has_commit = read(&["rev-parse", "--verify", "-q", "HEAD^{commit}"], true)?.is_some();
    let common = read(&["rev-parse", "--git-common-dir"], false)?.unwrap_or_default();
    let common = root.join(OsStr::from_bytes(unline(&common)));
    let common_dir = common
        .canonicalize()
        .map_err(|err| super::file_error(&common, err))?;
    let version = read(&["--version"], false)?.unwrap_or_default();
    let mut exists = |marker: &str| -> Result<bool, Fail> {
        let path = read(&["rev-parse", "--git-path", marker], false)?.unwrap_or_default();
        let path = root.join(OsStr::from_bytes(unline(&path)));
        path.try_exists()
            .map_err(|err| super::file_error(&path, err))
    };
    let legacy_sync = exists("dots-dev.json")?;
    let mut operations = Vec::new();
    for marker in [
        "MERGE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "sequencer",
    ] {
        if exists(marker)? {
            operations.push(marker);
        }
    }
    Ok(CheckoutShape {
        head,
        has_commit,
        jj_workspace: root.join(".jj").is_dir(),
        legacy_sync,
        operations,
        git_too_old: git_too_old(&version),
        common_dir,
    })
}

pub(crate) fn unline(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

fn git_too_old(version: &[u8]) -> bool {
    let version = String::from_utf8_lossy(version);
    let mut fields = version.trim_start_matches("git version ").split('.');
    let major = fields.next().and_then(|part| part.parse::<u32>().ok());
    let minor = fields.next().and_then(|part| part.parse::<u32>().ok());
    major.zip(minor).is_some_and(|version| version < (2, 43))
}
