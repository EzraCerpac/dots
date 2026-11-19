use std::collections::{BTreeMap, BTreeSet, HashSet};
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

use super::git::{Git, Tree, checked, index_row, invalid, local, oid, records, relative};

#[derive(Debug)]
pub(super) enum ScanError {
    Missing,
    Failed,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FindingLocation {
    pub line: Option<usize>,
    pub rule: Option<String>,
}

pub(super) struct Scratch<'a> {
    pub dir: TempDir,
    pub git: Git<'a>,
    pub live: Git<'a>,
}

impl<'a> Scratch<'a> {
    pub fn new(live: &Git<'a>, git_dir: &Path) -> io::Result<Self> {
        let dir = private_dir(git_dir, "dots-sync-")?;
        let git = live.indexed(&dir.path().join("snapshot.index"));
        Ok(Self {
            dir,
            git,
            live: live.clone(),
        })
    }

    pub fn nonce(&self) -> io::Result<&str> {
        self.dir
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("invalid temporary path"))
    }

    pub fn capture(&self, base: &str) -> io::Result<String> {
        self.live.ensure_dense_index()?;
        self.git
            .get(["-c", "core.sparseCheckout=false", "read-tree", base])?;
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
        self.live.ensure_dense_index()?;
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
        let mut index_info = Vec::new();
        for path in after.keys().filter(|path| overlaps(rejected, path)) {
            index_row(&mut index_info, path, None, base.len());
        }
        for (path, entry) in before.iter().filter(|(path, _)| overlaps(rejected, path)) {
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
    ) -> Result<BTreeMap<PathBuf, FindingLocation>, ScanError> {
        let include = oid(include.as_bytes()).map_err(|_| ScanError::Failed)?;
        let mut revs = format!(
            "--root --full-history --diff-merges=first-parent --no-ext-diff --no-textconv --no-renames --text {include}^{{commit}}"
        );
        for excluded in excludes {
            let excluded = oid(excluded.as_bytes()).map_err(|_| ScanError::Failed)?;
            write!(&mut revs, " ^{excluded}^{{commit}}").map_err(|_| ScanError::Failed)?;
        }
        revs.push_str(" --");
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
        .env("GIT_GRAFT_FILE", self.dir.path().join("no-grafts"));
        // Match the native scanner: explicitly select the live policy files
        // when present, and otherwise let gitleaks select its defaults.
        let config = self.live.root().join(".gitleaks.toml");
        let mut policy = RuleNames::Unknown;
        if config.exists() {
            cmd.arg("--config").arg(&config);
            if let Ok(text) = fs::read_to_string(config)
                && let Ok(document) = text.parse()
            {
                policy = RuleNames::Project(document);
            }
        } else if ["GITLEAKS_CONFIG", "GITLEAKS_CONFIG_TOML"]
            .iter()
            .all(|name| run.ctx().env.get(name).is_none())
        {
            policy = RuleNames::Default;
        }
        let ignore = self.live.root().join(".gitleaksignore");
        if ignore.exists() {
            cmd.arg("--gitleaks-ignore-path").arg(ignore);
        }
        cmd.arg(self.live.root()).stdin(Stdio::null());
        let output = run
            .probe_detached(cmd, Duration::from_secs(60))
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ScanError::Missing
                } else {
                    ScanError::Failed
                }
            })?;
        if !matches!(output.status.code(), Some(0 | 97)) || !output.stderr.is_empty() {
            return Err(ScanError::Failed);
        }
        let findings = read_findings(File::open(&report).map_err(|_| ScanError::Failed)?, &policy)?;
        if output.status.success() != findings.is_empty() {
            return Err(ScanError::Failed);
        }
        Ok(findings)
    }
}

enum RuleNames {
    Unknown,
    Default,
    Project(toml_edit::DocumentMut),
}

impl RuleNames {
    fn permits(&self, rule: &str) -> bool {
        if rule.len() > 64
            || !rule.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !rule.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
            })
        {
            return false;
        }
        // These are documented IDs in the built-in config. An unknown
        // detector name supplies no receipt text.
        let built_in = matches!(
            rule,
            "github-pat" | "github-fine-grained-pat" | "generic-api-key" | "private-key"
        );
        match self {
            Self::Unknown => false,
            Self::Default => built_in,
            Self::Project(doc) => {
                if doc
                    .get("rules")
                    .and_then(toml_edit::Item::as_array_of_tables)
                    .is_some_and(|rules| {
                        rules.iter().any(|table| {
                            table.get("id").and_then(toml_edit::Item::as_str) == Some(rule)
                        })
                    })
                {
                    return true;
                }
                let Some(extend) = doc.get("extend") else {
                    return false;
                };
                built_in
                    && extend.get("useDefault").and_then(toml_edit::Item::as_bool) == Some(true)
                    && !extend
                        .get("disabledRules")
                        .and_then(toml_edit::Item::as_array)
                        .is_some_and(|disabled| disabled.iter().any(|id| id.as_str() == Some(rule)))
            }
        }
    }
}

fn read_findings(
    report: impl std::io::Read,
    rules: &RuleNames,
) -> Result<BTreeMap<PathBuf, FindingLocation>, ScanError> {
    #[derive(Deserialize)]
    struct Finding {
        #[serde(rename = "File")]
        file: String,
        #[serde(rename = "StartLine")]
        line: Option<serde_json::Value>,
        #[serde(rename = "RuleID")]
        rule: Option<serde_json::Value>,
    }
    let findings = serde_json::from_reader::<_, Option<Vec<Finding>>>(report)
        .map_err(|_| ScanError::Failed)?
        .unwrap_or_default();
    let mut paths = BTreeMap::new();
    for finding in findings {
        let path = relative(finding.file.as_bytes()).map_err(|_| ScanError::Failed)?;
        if path
            .components()
            .collect::<PathBuf>()
            .as_os_str()
            .as_bytes()
            != finding.file.as_bytes()
        {
            return Err(ScanError::Failed);
        }
        let line = finding
            .line
            .as_ref()
            .and_then(serde_json::Value::as_u64)
            .and_then(|line| usize::try_from(line).ok())
            .filter(|&line| line > 0);
        let rule = finding
            .rule
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .filter(|rule| rules.permits(rule))
            .map(str::to_owned);
        paths.entry(path).or_insert(FindingLocation { line, rule });
    }
    Ok(paths)
}

pub(super) fn overlaps(paths: &BTreeSet<PathBuf>, path: &Path) -> bool {
    paths
        .iter()
        .any(|other| path.starts_with(other) || other.starts_with(path))
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

    fn scratch<'a>(git: &Git<'a>) -> Scratch<'a> {
        let git_dir = git.root().join(".git");
        Scratch::new(git, &git_dir).unwrap()
    }

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
        let scratch = scratch(&git);
        let run = Run::begin(&ctx, Sink::Unreported);
        assert!(matches!(
            scratch.scan(&run, &revision, &[]),
            Err(ScanError::Failed)
        ));
    }

    #[test]
    fn finding_metadata_discards_unverified_rule_names_and_invalid_lines() {
        let json = br#"[
            {"File":"nested/token","RuleID":"github-pat","StartLine":2,"Secret":"unused secret text"},
            {"File":"forged","RuleID":"not-a-built-in-rule","StartLine":"31337"},
            {"File":"control","RuleID":"github-pat\nforged diagnostic","StartLine":0},
            {"File":"negative","RuleID":42,"StartLine":-1},
            {"File":"missing"}
        ]"#;
        let found = read_findings(&json[..], &RuleNames::Default).unwrap();
        assert_eq!(
            found[Path::new("nested/token")],
            FindingLocation {
                rule: Some("github-pat".into()),
                line: Some(2)
            },
        );
        for path in ["forged", "control", "negative", "missing"] {
            assert_eq!(found[Path::new(path)], FindingLocation::default());
        }
        for path in [
            "../outside",
            "/absolute",
            "nested/./token",
            "nested//token",
            ".git/index",
        ] {
            let report = serde_json::to_vec(&[serde_json::json!({ "File": path })]).unwrap();
            assert!(matches!(
                read_findings(&report[..], &RuleNames::Default),
                Err(ScanError::Failed)
            ));
        }
        assert!(matches!(
            read_findings(
                &br#"[{"RuleID":"github-pat","StartLine":2}]"#[..],
                &RuleNames::Default
            ),
            Err(ScanError::Failed)
        ));
    }

    #[test]
    fn native_scan_reports_a_verified_default_rule_and_positive_line() {
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let base = fixture_commit(&git, "seed");
        fs::create_dir(git.root().join("nested")).unwrap();
        let fixture_token = ["ghp_", "Ab3De6Gh9Jk2Mn5Pq8St1Uv4Wx7Yz0Bc3Ef6"].concat();
        fs::write(
            git.root().join("nested/token"),
            format!("header\n{fixture_token}\n"),
        )
        .unwrap();
        git.get(["add", "nested/token"]).unwrap();
        let revision = fixture_commit(&git, "token fixture");
        let scratch = scratch(&git);
        let run = Run::begin(&ctx, Sink::Unreported);
        let found = scratch.scan(&run, &revision, &[&base]).unwrap();
        assert_eq!(
            found[Path::new("nested/token")],
            FindingLocation {
                rule: Some("github-pat".into()),
                line: Some(2)
            },
        );
    }

    #[test]
    fn native_scan_checks_intermediate_and_merge_resolutions_with_live_policy() {
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let base = fixture_commit(&git, "seed");
        git.get(["checkout", "-qb", "side"]).unwrap();
        let side_path = "Side marker.txt";
        let merge_path = "Merge marker.txt";
        let marker = b"dots-fixture-marker\n";
        fs::write(git.root().join(side_path), marker).unwrap();
        git.get(["add", "--", side_path]).unwrap();
        let added = fixture_commit(&git, "add fixture marker");
        git.get(["rm", "--", side_path]).unwrap();
        let side = fixture_commit(&git, "remove fixture marker");

        git.get(["checkout", "-q", "dev"]).unwrap();
        let lower_case_path = "side marker.txt";
        fs::write(git.root().join(lower_case_path), marker).unwrap();
        git.get(["add", "--", lower_case_path]).unwrap();
        let first = fixture_commit(&git, "nonmatching case");
        fs::write(git.root().join(merge_path), marker).unwrap();
        git.get(["add", "--", merge_path]).unwrap();
        let tree = git.id(["write-tree"]).unwrap();
        let merged = git
            .id([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit-tree",
                &tree,
                "-p",
                &first,
                "-p",
                &side,
                "-m",
                "merge resolution",
            ])
            .unwrap();

        fs::write(
            git.root().join(".gitleaks.toml"),
            r"[[rules]]
id = 'fixture-marker'
description = 'Fixture marker'
regex = 'dots-fixture-marker'
path = '^(Side|Merge) marker\.txt$'
",
        )
        .unwrap();
        let scratch = scratch(&git);
        let run = Run::begin(&ctx, Sink::Unreported);
        let found = match scratch.scan(&run, &merged, &[&base]) {
            Err(ScanError::Missing) => return,
            result => result.unwrap(),
        };
        assert_eq!(
            found.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([side_path.into(), merge_path.into()])
        );
        for path in [side_path, merge_path] {
            assert_eq!(
                found[Path::new(path)],
                FindingLocation {
                    rule: Some("fixture-marker".into()),
                    line: Some(1)
                }
            );
        }
        assert_eq!(
            scratch
                .scan(&run, &merged, &[&side])
                .unwrap()
                .into_keys()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([merge_path.into()])
        );

        fs::write(
            git.root().join(".gitleaksignore"),
            format!("{added}:{side_path}:fixture-marker:1\n"),
        )
        .unwrap();
        assert_eq!(
            scratch
                .scan(&run, &merged, &[&base])
                .unwrap()
                .into_keys()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([merge_path.into()])
        );
    }

    #[test]
    fn snapshot_omits_root_private_aliases_and_preserves_the_real_index() {
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        git.get(["config", "user.name", "Fixture"]).unwrap();
        git.get(["config", "user.email", "fixture@example.invalid"])
            .unwrap();
        let before = fixture_commit(&git, "seed");
        let private_paths = [
            "ConFiG.Local.TOML",
            "Config.Host-Fixture.Toml",
            "MiSeRc.toml",
            "MiSe.Fixture.LoCk",
            "MiSe.LoCk",
            "LOCKS/fixture",
            ".Setup-State/fixture",
        ];
        for path in private_paths {
            let file = git.root().join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, b"machine-local fixture").unwrap();
        }
        let public = "public/Config.Local.TOML";
        fs::create_dir(git.root().join("public")).unwrap();
        fs::write(git.root().join(public), b"public fixture").unwrap();
        let index_path = git.root().join(".git/index");
        let original_index = fs::read(&index_path).unwrap();
        let scratch = scratch(&git);
        let candidate = scratch.capture(&before).unwrap();

        assert_eq!(
            changed(
                &git.tree(&before).unwrap(),
                &scratch.git.tree(&candidate).unwrap()
            ),
            [PathBuf::from(public)]
        );
        assert_eq!(fs::read(index_path).unwrap(), original_index);
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), before);
        for path in private_paths {
            assert_eq!(
                fs::read(git.root().join(path)).unwrap(),
                b"machine-local fixture"
            );
        }
    }

    #[test]
    fn unpublished_private_history_blocks_even_when_the_latest_tree_removes_it() {
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let before = fixture_commit(&git, "seed");
        for path in ["MiSe.Fixture.LoCk", "CoNfIg.LoCaL.TOML"] {
            fs::write(git.root().join(path), b"machine-local fixture").unwrap();
            git.get(["add", path]).unwrap();
            let tracked = fixture_commit(&git, "accidentally tracked");
            git.get(["rm", "--", path]).unwrap();
            let latest = fixture_commit(&git, "removed again");
            assert!(machine_local_in_history(&git, &latest, &[&before]).unwrap());
            assert!(machine_local_in_history(&git, &latest, &[&tracked]).unwrap());
            assert!(!machine_local_in_history(&git, &latest, &[&latest]).unwrap());
        }
    }

    #[test]
    fn snapshot_refuses_configured_sparse_checkout_or_a_skip_worktree_absence() {
        let (_fixture, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let path = "tracked.txt";
        fs::write(git.root().join(path), b"tracked fixture").unwrap();
        git.get(["add", path]).unwrap();
        let before = fixture_commit(&git, "seed");
        let scratch = scratch(&git);
        let index_path = git.root().join(".git/index");

        git.get(["config", "core.sparseCheckout", "true"]).unwrap();
        assert!(scratch.capture(&before).is_err());
        git.get(["config", "core.sparseCheckout", "false"]).unwrap();
        assert_eq!(scratch.capture(&before).unwrap(), before);

        git.get(["update-index", "--skip-worktree", "--", path])
            .unwrap();
        fs::remove_file(git.root().join(path)).unwrap();
        let index = fs::read(&index_path).unwrap();
        assert!(scratch.capture(&before).is_err());
        assert_eq!(fs::read(index_path).unwrap(), index);
        assert!(!git.root().join(path).exists());
        assert_eq!(git.id(["rev-parse", "HEAD"]).unwrap(), before);
    }
}
