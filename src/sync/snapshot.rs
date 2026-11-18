use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tempfile::TempDir;

use crate::{process::Cwd, run::Run};

use super::guard::private_dir;

use super::git::{Git, Tree, bounded, checked, index_row, invalid, local, records, relative};

#[derive(Debug)]
pub(super) enum ScanError {
    Missing,
    Failed,
}

pub(super) struct Scratch<'a> {
    pub dir: TempDir,
    pub git: Git<'a>,
    pub live: Git<'a>,
    objects: PathBuf,
    scanner: PathBuf,
}

impl<'a> Scratch<'a> {
    pub fn new(live: &Git<'a>, git_dir: &Path, common: &Path) -> io::Result<Self> {
        let dir = private_dir(git_dir, "dots-sync-")?;
        let scanner = dir.path().join("scan.git");
        let format = live.text(["rev-parse", "--show-object-format"])?;
        let arg = format!("--object-format={format}");
        live.get([
            std::ffi::OsStr::new("init"),
            "--quiet".as_ref(),
            "--bare".as_ref(),
            "--template=".as_ref(),
            arg.as_ref(),
            scanner.as_os_str(),
        ])?;
        let objects = scanner.join("objects");
        let live_objects = common.join("objects").canonicalize()?;
        let bytes = live_objects.as_os_str().as_bytes();
        if bytes.contains(&b'\n') {
            return Err(invalid("unsupported Git object directory"));
        }
        let mut alternate = File::create(objects.join("info/alternates"))?;
        alternate.write_all(bytes)?;
        alternate.write_all(b"\n")?;
        let git = live
            .quarantined(&objects)
            .indexed(&dir.path().join("snapshot.index"));
        Ok(Self {
            dir,
            git,
            live: live.clone(),
            objects,
            scanner,
        })
    }

    pub fn nonce(&self) -> io::Result<&str> {
        self.dir
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("invalid temporary path"))
    }

    /// Transfer only objects reachable from the approved, frozen revision.
    /// Blobs from rejected candidates stay in quarantine.
    pub fn promote(&self, revision: &str, base: &str) -> io::Result<()> {
        if revision == base {
            return Ok(());
        }
        let mut revisions = tempfile::tempfile_in(self.dir.path())?;
        writeln!(revisions, "{revision}\n^{base}")?;
        revisions.rewind()?;
        let mut pack = tempfile::tempfile_in(self.dir.path())?;
        let output = self
            .git
            .command(["pack-objects", "--revs", "--stdout"])?
            .stdin(Stdio::from(revisions))
            .stdout(Stdio::from(pack.try_clone()?))
            .output()?;
        checked(output)?;
        pack.rewind()?;
        checked(
            self.live
                .command(["unpack-objects", "-q"])?
                .stdin(Stdio::from(pack))
                .output()?,
        )?;
        self.live
            .id(["rev-parse", "--verify", &format!("{revision}^{{commit}}")])?;
        Ok(())
    }

    pub fn capture(&self, base: &str) -> io::Result<String> {
        self.git.get(["read-tree", base])?;
        let listed = self.git.get([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])?;
        let mut paths = tempfile::tempfile_in(self.dir.path())?;
        for raw in records(&listed) {
            if !local(&relative(raw)?) {
                paths.write_all(raw)?;
                paths.write_all(&[0])?;
            }
        }
        if paths.stream_position()? != 0 {
            paths.rewind()?;
            checked(
                self.git
                    .command([
                        "--literal-pathspecs",
                        "-c",
                        "core.fsmonitor=false",
                        "-c",
                        "core.sparseCheckout=false",
                        "add",
                        "-A",
                        "--pathspec-from-file=-",
                        "--pathspec-file-nul",
                    ])?
                    .stdin(Stdio::from(paths))
                    .output()?,
            )?;
        }
        let tree = self.git.id(["write-tree"])?;
        let before = self.git.id(["rev-parse", &format!("{base}^{{tree}}")])?;
        if tree == before {
            Ok(base.into())
        } else {
            self.git.id([
                "-c",
                "commit.gpgSign=false",
                "commit-tree",
                &tree,
                "-p",
                base,
                "-m",
                "Save local configuration edits",
            ])
        }
    }

    pub fn omit(
        &self,
        base: &str,
        candidate: &str,
        rejected: &BTreeSet<PathBuf>,
    ) -> io::Result<String> {
        let before = self.git.tree(base)?;
        let after = self.git.tree(candidate)?;
        let intersects = |path: &Path| {
            rejected
                .iter()
                .any(|rejected| path.starts_with(rejected) || rejected.starts_with(path))
        };
        let mut index_info = Vec::new();
        for path in after.keys().filter(|path| intersects(path)) {
            index_row(&mut index_info, path, None, base.len());
        }
        for (path, entry) in before.iter().filter(|(path, _)| intersects(path)) {
            index_row(&mut index_info, path, Some(entry), base.len());
        }
        self.git.update_index(&index_info)?;
        let tree = self.git.id(["write-tree"])?;
        if tree == self.git.id(["rev-parse", &format!("{base}^{{tree}}")])? {
            Ok(base.into())
        } else {
            self.git.id([
                "-c",
                "commit.gpgSign=false",
                "commit-tree",
                &tree,
                "-p",
                base,
                "-m",
                "Save local configuration edits",
            ])
        }
    }

    pub fn scan(
        &self,
        run: &Run<'_>,
        include: &str,
        excludes: &[&str],
    ) -> Result<BTreeSet<PathBuf>, ScanError> {
        let mut revs = format!(
            "--full-history --diff-merges=first-parent --no-ext-diff --no-textconv --no-renames --text {include}"
        );
        for excluded in excludes {
            write!(&mut revs, " ^{excluded}").map_err(|_| ScanError::Failed)?;
        }
        let mut cmd = run
            .ctx()
            .command("gitleaks", Cwd::Root)
            .map_err(|_| ScanError::Failed)?;
        let git_environment = self
            .live
            .command(["version"])
            .map_err(|_| ScanError::Failed)?;
        for (name, value) in git_environment.get_envs() {
            if name.as_bytes().starts_with(b"GIT_") {
                match value {
                    Some(value) => {
                        cmd.env(name, value);
                    }
                    None => {
                        cmd.env_remove(name);
                    }
                }
            }
        }
        let report = self.dir.path().join("gitleaks.json");
        if report.exists() {
            fs::remove_file(&report).map_err(|_| ScanError::Failed)?;
        }
        cmd.args([
            "git",
            "--no-banner",
            "--no-color",
            "--log-level=error",
            "--redact=100",
            "--report-format=json",
            "--exit-code=97",
        ])
        .arg(format!("--log-opts={revs}"))
        .arg("--report-path")
        .arg(&report)
        .env_remove("GITLEAKS_CONFIG")
        .env_remove("GITLEAKS_CONFIG_TOML")
        // The private bare repository contains no graft file. Setting
        // GIT_GRAFT_FILE even to /dev/null makes Git warn on stderr.
        // Gitleaks can treat that as an empty scan and still return zero.
        .env_remove("GIT_GRAFT_FILE")
        .arg("--gitleaks-ignore-path")
        .arg(self.live.root());
        let config = self.live.root().join(".gitleaks.toml");
        match fs::symlink_metadata(&config) {
            Ok(md) if md.is_file() => {
                cmd.arg("--config").arg(config);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            _ => return Err(ScanError::Failed),
        }
        cmd.arg(&self.scanner).stdin(Stdio::null());
        let output = bounded(cmd, run, Duration::from_secs(60)).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ScanError::Missing
            } else {
                ScanError::Failed
            }
        })?;
        if !matches!(output.status.code(), Some(0 | 97)) || !output.stderr.is_empty() {
            return Err(ScanError::Failed);
        }
        #[derive(Deserialize)]
        struct Finding {
            #[serde(rename = "File")]
            file: String,
        }
        let findings = serde_json::from_reader::<_, Option<Vec<Finding>>>(
            File::open(&report).map_err(|_| ScanError::Failed)?,
        )
        .map_err(|_| ScanError::Failed)?
        .unwrap_or_default();
        if output.status.success() != findings.is_empty() {
            return Err(ScanError::Failed);
        }
        findings
            .into_iter()
            .map(|finding| relative(finding.file.as_bytes()).map_err(|_| ScanError::Failed))
            .collect()
    }

    pub fn objects(&self) -> &Path {
        &self.objects
    }
}

pub(super) fn changed(before: &Tree, after: &Tree) -> Vec<PathBuf> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}

pub(super) fn machine_local_in_history(
    git: &Git<'_>,
    revision: &str,
    published: &[&str],
) -> io::Result<bool> {
    let mut args = vec!["rev-list", revision, "--not"];
    args.extend_from_slice(published);
    let commits = git.text(args)?;
    let mut trees = HashSet::new();
    for commit in commits.lines() {
        let touched = git.get([
            "diff-tree",
            "--root",
            "-m",
            "-r",
            "--no-commit-id",
            "--no-renames",
            "--name-only",
            "-z",
            commit,
        ])?;
        for path in records(&touched) {
            if local(&relative(path)?) {
                return Ok(true);
            }
        }
        let tree = git.id(["rev-parse", &format!("{commit}^{{tree}}")])?;
        if trees.insert(tree.clone()) && git.tree(&tree)?.keys().any(|path| local(path)) {
            return Ok(true);
        }
    }
    Ok(false)
}

use std::fmt::Write as _;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        run::Sink,
        sync::tests::{fixture_commit, git_fixture},
    };
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rejects_a_scanner_error_even_with_success_status_and_empty_json() {
        let (fixture, mut ctx) = git_fixture();
        let bin = fixture.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let scanner = bin.join("gitleaks");
        fs::write(&scanner, b"#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --report-path ]; then shift; printf '[]' > \"$1\"; fi\n  shift\ndone\nprintf 'internal scanner error\\n' >&2\nexit 0\n").unwrap();
        fs::set_permissions(&scanner, fs::Permissions::from_mode(0o700)).unwrap();
        let path = ctx.env.get("PATH").unwrap();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(path))).unwrap();
        ctx.env.vars.insert("PATH".into(), path);

        let git = Git::new(&ctx, ctx.root.path());
        let revision = fixture_commit(&git, "seed");
        let scratch =
            Scratch::new(&git, &git.root().join(".git"), &git.root().join(".git")).unwrap();
        let run = Run::begin(&ctx, Sink::Unreported);
        assert!(matches!(
            scratch.scan(&run, &revision, &[]),
            Err(ScanError::Failed)
        ));
    }

    #[test]
    fn unpublished_lockfile_history_blocks_even_when_the_latest_tree_removes_it() {
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let before = fixture_commit(&git, "seed");
        let path = "mise.fixture.lock";
        fs::write(git.root().join(path), b"fixture lockfile").unwrap();
        git.get(["add", path]).unwrap();
        let tracked = fixture_commit(&git, "accidentally tracked");
        git.get(["rm", "--", path]).unwrap();
        let latest = fixture_commit(&git, "removed again");
        assert!(machine_local_in_history(&git, &latest, &[&before]).unwrap());
        assert!(machine_local_in_history(&git, &latest, &[&tracked]).unwrap());
    }
}
