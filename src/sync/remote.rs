use std::collections::BTreeMap;
use std::io;
use std::process::{Command, Output};
use std::time::Duration;

use crate::{config::RepoSetting, run::Run};

use super::git::{Git, checked, invalid, oid, records, split};

const DEV: &str = "refs/heads/dev";
const MAIN: &str = "refs/heads/main";
const NETWORK_BUDGET: Duration = Duration::from_secs(60);

pub(super) struct Origin {
    fetch: String,
    push: String,
}

impl Origin {
    pub fn load(git: &Git<'_>, setting: Option<RepoSetting>) -> io::Result<Option<Self>> {
        // Read the relevant config once. `remote get-url --all` separates URLs
        // with newlines, which may themselves occur in a config value.
        let output = git.output([
            "config",
            "--null",
            "--get-regexp",
            r"^(remote\.origin\.(url|pushurl)|url\..*\.(insteadof|pushinsteadof))$",
        ])?;
        let config = if output.status.code() == Some(1) {
            Vec::new()
        } else {
            checked(output)?
        };
        let mut fetch = Vec::new();
        let mut push = Vec::new();
        let mut rewrites = Rewrites::default();
        let mut push_rewrites = Rewrites::default();
        for row in records(&config) {
            let row = std::str::from_utf8(row).map_err(io::Error::other)?;
            let (key, value) = row
                .split_once('\n')
                .ok_or_else(|| invalid("missing Git URL configuration value"))?;
            match key {
                "remote.origin.url" => fetch.push(value),
                "remote.origin.pushurl" => push.push(value),
                _ => {
                    let (base, kind) = key
                        .strip_prefix("url.")
                        .and_then(|key| key.rsplit_once('.'))
                        .ok_or_else(|| invalid("invalid Git URL rewrite rule"))?;
                    match kind {
                        "insteadof" => rewrites.add(base, value),
                        "pushinsteadof" => push_rewrites.add(base, value),
                        _ => return Err(invalid("invalid Git URL rewrite rule")),
                    }
                }
            }
        }
        if fetch.is_empty() && push.is_empty() && setting.is_none() {
            if super::source::origin_absent(git)? {
                return Ok(None);
            }
            return Err(invalid(
                "origin exists but has no supported URL configuration",
            ));
        }
        let raw = fetch.first().ok_or_else(|| invalid("origin has no URL"))?;
        let expected = match setting {
            Some(setting) => setting,
            None => RepoSetting::parse(raw).map_err(io::Error::other)?,
        };
        if !raw.contains(':') || !expected.matches(raw) {
            return Err(invalid(
                "configured origin does not match the source repository",
            ));
        }
        for url in fetch.iter().chain(&push) {
            RepoSetting::parse(url).map_err(io::Error::other)?;
        }
        let effective_fetch: Vec<_> = fetch.iter().map(|url| rewrites.apply(url)).collect();
        let effective_push: Vec<_> = if push.is_empty() {
            // Git uses pushInsteadOf on the original fetch URLs only when no
            // pushurl is configured. If none matches, it uses the fetch URLs.
            fetch
                .iter()
                .filter_map(|url| push_rewrites.rewrite(url))
                .collect()
        } else {
            push.iter().map(|url| rewrites.apply(url)).collect()
        };
        let effective_push = if effective_push.is_empty() {
            &effective_fetch
        } else {
            &effective_push
        };
        let matches = |url: &str| url.contains(':') && expected.matches(url);
        if !effective_fetch
            .iter()
            .chain(effective_push)
            .all(|url| matches(url))
        {
            return Err(invalid("origin does not match the configured source"));
        }
        Ok(Some(Self {
            fetch: effective_fetch[0].clone(),
            push: effective_push[0].clone(),
        }))
    }
}

#[derive(Default)]
struct Rewrites<'a>(Vec<(&'a str, Vec<&'a str>)>);

impl<'a> Rewrites<'a> {
    fn add(&mut self, base: &'a str, prefix: &'a str) {
        if let Some((_, prefixes)) = self.0.iter_mut().find(|(key, _)| *key == base) {
            prefixes.push(prefix);
        } else {
            self.0.push((base, vec![prefix]));
        }
    }

    fn rewrite(&self, url: &str) -> Option<String> {
        // Git groups rules by replacement base, tries them in config order,
        // and keeps the first match among prefixes of equal length.
        let mut best: Option<(usize, &str, &str)> = None;
        for (base, prefixes) in &self.0 {
            for prefix in prefixes {
                if let Some(suffix) = url.strip_prefix(prefix)
                    && best.is_none_or(|(len, _, _)| prefix.len() > len)
                {
                    best = Some((prefix.len(), base, suffix));
                }
            }
        }
        best.map(|(_, base, suffix)| format!("{base}{suffix}"))
    }

    fn apply(&self, url: &str) -> String {
        self.rewrite(url).unwrap_or_else(|| url.to_owned())
    }
}

/// A command-local remote uses one whole-URL rewrite to reach the verified
/// endpoint. Neither an origin change nor a rule matching that endpoint can
/// cause Git to resolve another destination when it opens the transport.
struct PinnedRemote<'a> {
    name: String,
    endpoint: &'a str,
}

impl<'a> PinnedRemote<'a> {
    fn new(endpoint: &'a str) -> io::Result<Self> {
        let token = tempfile::Builder::new()
            .prefix("dots-sync-")
            .rand_bytes(24)
            .tempdir()?;
        let name = token
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("could not name a private Git remote"))?
            .to_owned();
        Ok(Self { name, endpoint })
    }

    fn command(&self, git: &Git<'_>, args: &[&str], tail: &[&str]) -> io::Result<Command> {
        let mut cmd = git.command(
            args.iter()
                .copied()
                .chain([self.name.as_str()])
                .chain(tail.iter().copied()),
        )?;
        // Git's launcher forbids ext transport even if the rewrite is absent.
        let alias = format!("ext::{}", self.name);
        // Keep config keys and values separate. An otherwise valid file or
        // SSH path can contain '=' in the rewrite base.
        for (i, key) in [
            format!("remote.{}.url", self.name),
            format!("remote.{}.pushurl", self.name),
            format!("url.{}.insteadOf", self.endpoint),
        ]
        .into_iter()
        .enumerate()
        {
            cmd.env(format!("GIT_CONFIG_KEY_{i}"), key)
                .env(format!("GIT_CONFIG_VALUE_{i}"), &alias);
        }
        cmd.env("GIT_CONFIG_COUNT", "3");
        Ok(cmd)
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

pub(super) fn fetch(git: &Git<'_>, run: &Run<'_>, origin: &Origin) -> io::Result<RemoteHeads> {
    const PREFIX: &str = "refs/remotes/origin/";
    const REFSPEC: &str = "+refs/heads/*:refs/remotes/origin/*";
    let cmd = PinnedRemote::new(&origin.fetch)?.command(
        git,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
            "--no-recurse-submodules",
            "--atomic",
            "--prune",
            "--no-prune-tags",
            "--refmap=",
        ],
        &[REFSPEC],
    )?;
    checked(run.probe_detached(cmd, NETWORK_BUDGET)?)?;
    let mut heads = BTreeMap::new();
    let output = git.get([
        "for-each-ref",
        "--format=%(objectname)%00%(refname)%00%(symref)",
        PREFIX,
    ])?;
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let (id, ref_info) = split(line, 0)?;
        let (name, symbolic) = split(ref_info, 0)?;
        if !symbolic.is_empty() {
            continue;
        }
        let name = std::str::from_utf8(name).map_err(io::Error::other)?;
        let leaf = name
            .strip_prefix(PREFIX)
            .ok_or_else(|| invalid("unexpected fetched ref"))?;
        heads.insert(format!("refs/heads/{leaf}"), oid(id)?);
    }
    Ok(RemoteHeads(heads))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PushError {
    NotAttempted,
    Rejected,
    Unconfirmed,
}

pub(super) fn push(
    git: &Git<'_>,
    run: &Run<'_>,
    origin: &Origin,
    expected: Option<&str>,
    revision: &str,
) -> Result<(), PushError> {
    // The exact lease is only an additional compare-and-swap predicate. Prove
    // that the immutable source can create or fast-forward the expected ref
    // before invoking Git, with replacement objects and grafts disabled.
    if let Some(expected) = expected
        && !git
            .ancestor(expected, revision)
            .map_err(|_| PushError::NotAttempted)?
    {
        return Err(PushError::NotAttempted);
    }
    let lease = format!("--force-with-lease={DEV}:{}", expected.unwrap_or(""));
    // push first tries an exact ref-name match, even for a full hex OID.
    // A peel suffix is invalid in ref names, so Git must resolve the
    // expression as an object. Verify it still names the approved commit.
    let source = format!("{revision}^{{commit}}");
    if git
        .id(["rev-parse", "--verify", &source])
        .map_err(|_| PushError::NotAttempted)?
        != revision
    {
        return Err(PushError::NotAttempted);
    }
    let refspec = format!("{source}:{DEV}");
    let remote = PinnedRemote::new(&origin.push).map_err(|_| PushError::NotAttempted)?;
    let mut cmd = remote
        .command(
            git,
            &[
                "-c",
                "push.negotiate=false",
                "push",
                "--porcelain",
                "--no-mirror",
                "--no-follow-tags",
                "--no-signed",
                "--recurse-submodules=no",
                &lease,
            ],
            &[&refspec],
        )
        .map_err(|_| PushError::NotAttempted)?;
    cmd.env("LC_ALL", "C");
    let output = run
        .probe_detached(cmd, NETWORK_BUDGET)
        .map_err(|_| PushError::Unconfirmed)?;
    push_status(&output, &refspec, expected == Some(revision))
}

fn push_status(output: &Output, refspec: &str, already_current: bool) -> Result<(), PushError> {
    let mut rows = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|row| row.get(1) == Some(&b'\t'));
    let row = rows.next().ok_or(PushError::Unconfirmed)?;
    let (mapping, reason) = split(&row[2..], b'\t').map_err(|_| PushError::Unconfirmed)?;
    if mapping != refspec.as_bytes() || rows.next().is_some() {
        return Err(PushError::Unconfirmed);
    }
    // Git can report success when the remote already has the source and skip
    // the lease check. Accept that only if it was the validated expected tip.
    match (row[0], output.status.success()) {
        (b' ' | b'*', true) => Ok(()),
        (b'=', true) if already_current => Ok(()),
        (b'=', true) => Err(PushError::Rejected),
        (b'!', false)
            if reason.starts_with(b"[rejected]") || reason.starts_with(b"[remote rejected]") =>
        {
            Err(PushError::Rejected)
        }
        _ => Err(PushError::Unconfirmed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Sink;
    use crate::sync::tests::{fixture_commit, git_fixture};

    #[test]
    fn validates_every_effective_destination_without_resplitting_urls() {
        let (_world, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let raw = "https://github.com/example/source.git";
        let ssh = "git@github.com:example/source.git";
        let other_ssh = "ssh://git@github.com/example/source";
        git.get(["remote", "add", "origin", raw]).unwrap();
        git.get(["config", &format!("url.{ssh}.insteadOf"), raw])
            .unwrap();
        git.get(["config", &format!("url.{other_ssh}.pushInsteadOf"), raw])
            .unwrap();
        let origin = Origin::load(&git, None).unwrap().unwrap();
        assert_eq!(origin.fetch, ssh);
        assert_eq!(origin.push, other_ssh);

        git.get(["config", "--add", "remote.origin.pushurl", raw])
            .unwrap();
        let origin = Origin::load(&git, None).unwrap().unwrap();
        assert_eq!(origin.push, ssh);
        git.get(["config", "--add", "remote.origin.pushurl", "alias:source"])
            .unwrap();
        git.get([
            "config",
            "url.https://github.com/example/other.insteadOf",
            "alias:source",
        ])
        .unwrap();
        assert!(Origin::load(&git, None).is_err());
        git.get([
            "config",
            "--replace-all",
            "remote.origin.pushurl",
            &format!("{ssh}\n{other_ssh}"),
            "^alias:source$",
        ])
        .unwrap();
        assert!(Origin::load(&git, None).is_err());
    }

    #[test]
    fn fetch_and_push_keep_verified_file_urls_after_origin_and_rewrites_change() {
        let (world, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let approved = fixture_commit(&git, "approved");
        let root = world.path().canonicalize().unwrap();
        let path = root.join("approved=remote");
        let other_path = root.join("other");
        for path in [&path, &other_path] {
            git.get(["init", "--quiet", "--bare", path.to_str().unwrap()])
                .unwrap();
        }
        let url = format!("file://{}", path.display());
        let other_url = format!("file://{}", other_path.display());
        git.get(["push", &url, "HEAD:refs/heads/main"]).unwrap();
        git.get(["remote", "add", "origin", &url]).unwrap();
        for _ in 0..2 {
            git.get(["config", "--add", "remote.origin.pushurl", &url])
                .unwrap();
        }
        let origin = Origin::load(&git, None).unwrap().unwrap();
        git.get(["remote", "set-url", "origin", &other_url])
            .unwrap();
        git.get([
            "config",
            "--replace-all",
            "remote.origin.pushurl",
            &other_url,
        ])
        .unwrap();
        git.get(["config", &format!("url.{other_url}.insteadOf"), &url])
            .unwrap();
        git.get(["config", "remote.origin.mirror", "true"]).unwrap();
        git.get(["config", "fetch.pruneTags", "true"]).unwrap();
        git.get(["config", "push.followTags", "true"]).unwrap();
        git.get(["config", "push.negotiate", "true"]).unwrap();
        git.get([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "tag",
            "-am",
            "private annotated tag",
            "private-tag",
        ])
        .unwrap();

        let run = Run::begin(&ctx, Sink::Unreported);
        let heads = fetch(&git, &run, &origin).unwrap();
        assert_eq!(heads.main(), Some(approved.as_str()));
        assert_eq!(heads.dev(), None);
        git.get(["show-ref", "--verify", "refs/tags/private-tag"])
            .unwrap();
        push(&git, &run, &origin, heads.dev(), &approved).unwrap();
        let updated = fetch(&git, &run, &origin).unwrap();
        assert_eq!(updated.dev(), Some(approved.as_str()));
        assert_eq!(
            push(&git, &run, &origin, None, &approved),
            Err(PushError::Rejected)
        );

        let later = fixture_commit(&git, "later");
        assert_eq!(
            push(&git, &run, &origin, None, &later),
            Err(PushError::Rejected)
        );
        assert_eq!(
            push(&git, &run, &origin, Some(&later), &approved),
            Err(PushError::NotAttempted)
        );
        push(&git, &run, &origin, updated.dev(), &later).unwrap();

        assert_eq!(
            Git::new(&ctx, &path)
                .text(["for-each-ref", "--format=%(refname)"])
                .unwrap(),
            "refs/heads/dev\nrefs/heads/main"
        );
        assert!(
            Git::new(&ctx, &other_path)
                .get(["for-each-ref"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reports_unconfirmed_if_remote_withdraws_dev_before_readback() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let (world, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let approved = fixture_commit(&git, "approved");
        let path = world.path().canonicalize().unwrap().join("remote");
        let url = format!("file://{}", path.display());
        git.get(["init", "--quiet", "--bare", path.to_str().unwrap()])
            .unwrap();
        git.get(["remote", "add", "origin", &url]).unwrap();
        fs::create_dir_all(path.join("hooks")).unwrap();
        let hook = path.join("hooks/post-receive");
        fs::write(&hook, "#!/bin/sh\ngit update-ref -d refs/heads/dev\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();

        let origin = Origin::load(&git, None).unwrap().unwrap();
        let run = Run::begin(&ctx, Sink::Unreported);
        push(&git, &run, &origin, None, &approved).unwrap();
        assert!(
            Git::new(&ctx, &path)
                .get(["for-each-ref"])
                .unwrap()
                .is_empty()
        );
    }
}
