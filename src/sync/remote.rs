use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use crate::{config::RepoSetting, run::Run};

use super::git::{Git, bounded, checked, invalid, oid, records, split};

const DEV: &str = "refs/heads/dev";
const MAIN: &str = "refs/heads/main";

pub(super) struct Origin(RepoSetting);

impl Origin {
    pub fn load(git: &Git<'_>, setting: Option<RepoSetting>) -> io::Result<Option<Self>> {
        let output = git.output(["config", "--null", "--get-all", "remote.origin.url"])?;
        if output.status.code() == Some(1) && setting.is_none() {
            return Ok(None);
        }
        let urls = checked(output)?;
        let raw = records(&urls)
            .next()
            .ok_or_else(|| invalid("origin has no URL"))?;
        let raw = std::str::from_utf8(raw).map_err(io::Error::other)?;
        let expected = match setting {
            Some(setting) => setting,
            None => RepoSetting::parse(raw).map_err(io::Error::other)?,
        };
        if !raw.contains(':') || !expected.matches(raw) {
            return Err(invalid(
                "configured origin does not match the source repository",
            ));
        }
        let origin = Self(expected);
        origin.check(git)?;
        Ok(Some(origin))
    }

    pub fn check(&self, git: &Git<'_>) -> io::Result<()> {
        for args in [
            &["remote", "get-url", "--all", "origin"][..],
            &["remote", "get-url", "--push", "--all", "origin"][..],
        ] {
            let urls = git.text(args)?;
            if urls.is_empty()
                || !urls
                    .lines()
                    .all(|url| url.contains(':') && self.0.matches(url))
            {
                return Err(invalid("origin does not match the configured source"));
            }
        }
        Ok(())
    }
}

pub(super) struct RemoteHeads(BTreeMap<String, String>);

impl RemoteHeads {
    pub fn dev(&self) -> Option<&str> {
        self.0.get(DEV).map(String::as_str)
    }
    pub fn main(&self) -> Option<&str> {
        self.0.get(MAIN).map(String::as_str)
    }
    pub fn published(&self) -> impl Iterator<Item = &str> {
        self.0.values().map(String::as_str)
    }

    pub fn withdraws(&self, git: &Git<'_>, earlier: &Self) -> io::Result<bool> {
        for (before, after) in [(earlier.dev(), self.dev()), (earlier.main(), self.main())] {
            if let Some(before) = before {
                match after {
                    Some(after) if git.ancestor(before, after)? => (),
                    _ => return Ok(true),
                }
            }
        }
        Ok(false)
    }

    pub fn same_incoming(&self, other: &Self) -> bool {
        self.dev() == other.dev() && self.main() == other.main()
    }
}

/// A private ref namespace keeps fetched objects alive without modifying
/// origin's configured remote-tracking branches or FETCH_HEAD.
pub(super) struct Fetch<'a> {
    git: Git<'a>,
    prefix: String,
}

impl<'a> Fetch<'a> {
    pub fn new(git: &Git<'a>, nonce: &str) -> Self {
        Self {
            git: git.clone(),
            prefix: format!("refs/dots-sync/{nonce}/"),
        }
    }

    pub fn heads(&self, run: &Run<'_>, origin: &Origin) -> io::Result<RemoteHeads> {
        origin.check(&self.git)?;
        let refspec = format!("+refs/heads/*:{}*", self.prefix);
        let cmd = self.git.command([
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
            "--no-recurse-submodules",
            "--prune",
            "--refmap=",
            "origin",
            &refspec,
        ])?;
        checked(bounded(cmd, run, Duration::from_secs(60))?)?;
        let mut heads = BTreeMap::new();
        let output = self.git.get([
            "for-each-ref",
            "--format=%(objectname)%00%(refname)",
            &self.prefix,
        ])?;
        for line in output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let (id, name) = split(line, 0)?;
            let name = std::str::from_utf8(name).map_err(io::Error::other)?;
            let leaf = name
                .strip_prefix(&self.prefix)
                .ok_or_else(|| invalid("unexpected fetched ref"))?;
            heads.insert(format!("refs/heads/{leaf}"), oid(id)?);
        }
        Ok(RemoteHeads(heads))
    }
}

impl Drop for Fetch<'_> {
    fn drop(&mut self) {
        let Ok(refs) = self
            .git
            .text(["for-each-ref", "--format=%(refname)", &self.prefix])
        else {
            return;
        };
        for name in refs.lines().filter(|name| name.starts_with(&self.prefix)) {
            let _ = self.git.get(["update-ref", "-d", name]);
        }
    }
}

pub(super) fn push(
    git: &Git<'_>,
    run: &Run<'_>,
    origin: &Origin,
    expected: Option<&str>,
    revision: &str,
) -> io::Result<()> {
    origin.check(git)?;
    // The exact lease is only an additional compare-and-swap predicate. Prove
    // that the immutable source can create or fast-forward the expected ref
    // before invoking Git, with replacement objects and grafts disabled.
    if let Some(expected) = expected {
        if !git.ancestor(expected, revision)? {
            return Err(invalid("push would not fast-forward dev"));
        }
    }
    let lease = format!("--force-with-lease={DEV}:{}", expected.unwrap_or(""));
    // push first tries an exact ref-name match, even for a full hex OID.
    // A peel suffix is invalid in ref names, so Git must resolve the
    // expression as an object. Verify it still names the approved commit.
    let source = format!("{revision}^{{commit}}");
    if git.id(["rev-parse", "--verify", &source])? != revision {
        return Err(invalid("push source does not match the validated commit"));
    }
    let refspec = format!("{source}:{DEV}");
    let cmd = git.command([
        "-c",
        "remote.origin.mirror=false",
        "push",
        "--porcelain",
        "--no-follow-tags",
        "--no-signed",
        "--recurse-submodules=no",
        &lease,
        "origin",
        &refspec,
    ])?;
    let stdout = checked(bounded(cmd, run, Duration::from_secs(60))?)?;
    // Git can report success when the remote already has the source and skip
    // the lease check. Accept that only if it was the validated expected tip.
    let mut applied = false;
    for row in stdout.split(|b| *b == b'\n') {
        if row.get(1) != Some(&b'\t') {
            continue;
        }
        match row[0] {
            b' ' | b'*' => applied = true,
            b'=' if expected == Some(revision) => applied = true,
            _ => return Err(invalid("push did not advance the expected tip")),
        }
    }
    applied
        .then_some(())
        .ok_or_else(|| invalid("missing verified push result"))
}
