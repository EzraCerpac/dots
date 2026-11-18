use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::process::Cwd;
use crate::{Ctx, Fail};

use super::TEMPLATES;

const MAIN: &str = "refs/heads/main";
const DEV: &str = "refs/heads/dev";
const SEED_MESSAGE: &[u8] = b"Initialize dots configuration\n";

pub(super) struct Git<'a> {
    pub ctx: &'a Ctx,
    pub root: &'a Path,
    pub profile: &'a str,
}

pub(super) enum Shape {
    Absent,
    Empty,
    Existing,
}

fn git_error(action: &str) -> Fail {
    Fail::new(1, format!("Git could not {action} for dots init"))
}

impl Git<'_> {
    pub fn command(&self, cwd: &Path) -> Result<Command, Fail> {
        let mut cmd = self
            .ctx
            .command_with_profile("git", Cwd::Dir(cwd), self.profile.as_ref())?;
        // Init operates on the repository at DIR even when called from a Git hook.
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
            "GIT_QUARANTINE_PATH",
        ] {
            cmd.env_remove(var);
        }
        cmd.env("DOTS_ROOT", self.root).env("LC_ALL", "C");
        Ok(cmd)
    }

    fn output_at(&self, cwd: &Path, args: &[&str]) -> Result<Output, Fail> {
        output(self.command(cwd)?.args(args))
    }

    pub fn output(&self, args: &[&str]) -> Result<Output, Fail> {
        self.output_at(self.root, args)
    }

    pub fn checked(&self, args: &[&str], action: &str) -> Result<Output, Fail> {
        checked(self.output(args)?, action)
    }

    pub fn shape(&self, ancestor: &Path) -> Result<Shape, Fail> {
        let probe = self.output_at(ancestor, &["rev-parse", "--absolute-git-dir"])?;
        if !probe.status.success() {
            if String::from_utf8_lossy(&probe.stderr).starts_with("fatal: not a git repository") {
                // Do not replace malformed Git metadata.
                if ancestor
                    .ancestors()
                    .any(|dir| dir.join(".git").symlink_metadata().is_ok())
                {
                    return Err(git_error("inspect existing Git metadata"));
                }
                return Ok(Shape::Absent);
            }
            return Err(git_error("inspect the configuration directory"));
        }
        let top = self.output_at(ancestor, &["rev-parse", "--show-toplevel"])?;
        if !top.status.success() {
            return Err(Fail::usage(
                "dots init needs a Git worktree, not a bare repository or Git metadata directory",
            ));
        }
        let top = Path::new(OsStr::from_bytes(trim_line(&top.stdout)));
        if top != self.root {
            return Err(Fail::usage(format!(
                "{} is inside the Git repository at {}; choose the repository root or a directory outside it",
                self.root.display(),
                top.display(),
            )));
        }
        let refs = self.checked(
            &["rev-list", "--all", "--max-count=1"],
            "inspect commit history",
        )?;
        let index = self.checked(&["ls-files", "--stage", "-z"], "inspect the index")?;
        if refs.stdout.is_empty() && index.stdout.is_empty() {
            Ok(Shape::Empty)
        } else {
            Ok(Shape::Existing)
        }
    }

    pub fn seed(&self, shape: Shape) -> Result<(), Fail> {
        if matches!(shape, Shape::Absent) {
            self.checked(
                &["init", "--initial-branch=dev"],
                "initialize the repository",
            )?;
            println!("created Git repository");
        } else {
            println!("kept empty Git repository");
        }

        let scratch = tempfile::tempdir().map_err(|_| git_error("create a scratch index"))?;
        let index = scratch.path().join("index");
        checked(
            output(
                self.command(self.root)?
                    .env("GIT_INDEX_FILE", &index)
                    .args(["-c", "core.splitIndex=false", "read-tree", "--empty"]),
            )?,
            "initialize the scratch index",
        )?;
        let mut entries = String::new();
        for template in TEMPLATES {
            if self.matches_file(template)? && !self.ignored(template.path)? {
                let oid = self.template_object(template, true)?;
                entries.push_str(&format!(
                    "100{:o} {oid}\t{}\n",
                    template.mode, template.path
                ));
            }
        }
        input(
            self.command(self.root)?
                .env("GIT_INDEX_FILE", &index)
                .args([
                    "-c",
                    "core.splitIndex=false",
                    "update-index",
                    "--index-info",
                ]),
            entries.as_bytes(),
            "stage the embedded templates",
        )?;
        let tree = line(checked(
            output(
                self.command(self.root)?
                    .env("GIT_INDEX_FILE", &index)
                    .args(["-c", "core.splitIndex=false", "write-tree"]),
            )?,
            "write the seed tree",
        )?)?;

        let mut cmd = self.command(self.root)?;
        cmd.args(["-c", "user.useConfigOnly=true"]);
        for (kind, key, email_env, name_env) in [
            ("AUTHOR", "author", "GIT_AUTHOR_EMAIL", "GIT_AUTHOR_NAME"),
            (
                "COMMITTER",
                "committer",
                "GIT_COMMITTER_EMAIL",
                "GIT_COMMITTER_NAME",
            ),
        ] {
            if !self
                .output(&[
                    "-c",
                    "user.useConfigOnly=true",
                    "var",
                    &format!("GIT_{kind}_IDENT"),
                ])?
                .status
                .success()
            {
                cmd.env_remove(name_env).env_remove(email_env).args([
                    "-c",
                    &format!("{key}.name=dots"),
                    "-c",
                    &format!("{key}.email=dots@localhost"),
                ]);
            }
        }
        let oid = line(input(
            cmd.args(["commit-tree", "--no-gpg-sign", &tree, "-F", "-"]),
            SEED_MESSAGE,
            "create the seed commit",
        )?)?;

        let (destination, mut lock) = self.reserve_index()?;
        // The real index is still empty, and Git cannot stage changes while we
        // hold its index.lock. Prepare the complete replacement before moving refs.
        let mut prepared = File::open(&index).map_err(|err| super::file_error(&index, err))?;
        io::copy(&mut prepared, &mut lock).map_err(|err| super::file_error(lock.path(), err))?;
        lock.as_file()
            .sync_all()
            .map_err(|err| super::file_error(lock.path(), err))?;
        let previous = self.head()?.ok_or_else(|| git_error("read unborn HEAD"))?;
        if previous != DEV {
            self.checked(&["symbolic-ref", "HEAD", DEV], "select dev")?;
        }
        let refs =
            format!("option no-deref\ncreate {MAIN} {oid}\noption no-deref\ncreate {DEV} {oid}\n");
        if let Err(error) = input(
            self.command(self.root)?.args(["update-ref", "--stdin"]),
            refs.as_bytes(),
            "create main and dev",
        ) {
            self.restore_unborn_head(&previous)?;
            return Err(error);
        }
        if let Err(error) = lock.persist(&destination) {
            // Roll back both refs only if they still point at our candidate.
            let refs = format!(
                "option no-deref\ndelete {MAIN} {oid}\noption no-deref\ndelete {DEV} {oid}\n"
            );
            input(
                self.command(self.root)?.args(["update-ref", "--stdin"]),
                refs.as_bytes(),
                "roll back the seed refs after an index write failure",
            )?;
            self.restore_unborn_head(&previous)?;
            return Err(super::file_error(&destination, error.error));
        }
        println!("created seed commit {oid} on main and dev");
        Ok(())
    }

    fn reserve_index(&self) -> Result<(PathBuf, tempfile::NamedTempFile), Fail> {
        let location = self.checked(&["rev-parse", "--git-path", "index"], "locate the index")?;
        let index = self
            .root
            .join(OsStr::from_bytes(trim_line(&location.stdout)));
        let mut lock = index.as_os_str().to_os_string();
        lock.push(".lock");
        let path = PathBuf::from(lock);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&path)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::AlreadyExists {
                    Fail::new(
                        1,
                        format!(
                            "Git index is locked at {}; free the index before retrying init",
                            path.display()
                        ),
                    )
                } else {
                    super::file_error(&path, err)
                }
            })?;
        let temp_path = tempfile::TempPath::try_from_path(path.clone())
            .map_err(|err| super::file_error(&path, err))?;
        let lock = tempfile::NamedTempFile::from_parts(file, temp_path);
        let staged = self.checked(&["ls-files", "--stage", "-z"], "inspect the index")?;
        if !staged.stdout.is_empty() || self.commit("HEAD")?.is_some() {
            return Err(Fail::new(
                1,
                "the repository changed during init; retry with its current index and history",
            ));
        }
        Ok((index, lock))
    }

    fn restore_unborn_head(&self, previous: &str) -> Result<(), Fail> {
        if previous != DEV && self.head()?.as_deref() == Some(DEV) && self.commit("HEAD")?.is_none()
        {
            self.checked(&["symbolic-ref", "HEAD", previous], "restore unborn HEAD")?;
        }
        Ok(())
    }

    fn matches_file(&self, template: &super::files::Template) -> Result<bool, Fail> {
        let path = self.root.join(template.path);
        for parent in path.ancestors().skip(1).take_while(|p| *p != self.root) {
            if !parent.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                return Ok(false);
            }
        }
        match path.symlink_metadata() {
            Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o7777 == template.mode => {
                fs::read(&path)
                    .map(|bytes| bytes == template.bytes)
                    .map_err(|err| super::file_error(&path, err))
            }
            Ok(_) => Ok(false),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(super::file_error(&path, err)),
        }
    }

    fn ignored(&self, path: &str) -> Result<bool, Fail> {
        match self
            .output(&["check-ignore", "--no-index", "-q", "--", path])?
            .status
            .code()
        {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(git_error("check which templates are ignored")),
        }
    }

    fn template_object(
        &self,
        template: &super::files::Template,
        write: bool,
    ) -> Result<String, Fail> {
        let mut cmd = self.command(self.root)?;
        cmd.args(["hash-object", "--stdin"]);
        if write {
            cmd.arg("-w");
        }
        line(input(
            &mut cmd,
            template.bytes,
            "hash the embedded template",
        )?)
    }

    pub fn pristine(&self) -> Result<Option<String>, Fail> {
        if self.head()?.as_deref() != Some(DEV) {
            return Ok(None);
        }
        let Some(oid) = self.commit(DEV)? else {
            return Ok(None);
        };
        if self.commit(MAIN)?.as_ref() != Some(&oid) {
            return Ok(None);
        }

        // Read the raw objects. Replacement refs and grafts cannot make
        // unrelated history or a different tree qualify as a seed.
        let commit = self.checked(
            &["--no-replace-objects", "cat-file", "commit", &oid],
            "inspect the seed commit",
        )?;
        let Some(tree) = seed_tree(&commit.stdout) else {
            return Ok(None);
        };
        let actual = self.checked(
            &["--no-replace-objects", "ls-tree", "-r", "-t", "-z", tree],
            "inspect the seed tree",
        )?;
        let possible = TEMPLATES
            .iter()
            .map(|template| {
                let hash = self.template_object(template, false)?;
                Ok(format!(
                    "100{:o} blob {hash}\t{}",
                    template.mode, template.path
                ))
            })
            .collect::<Result<Vec<_>, Fail>>()?;
        let mut template_directory = false;
        let mut template_file = false;
        for entry in actual
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            if let Some(path) = tree_directory(entry) {
                if path != b"dots" {
                    return Ok(None);
                }
                template_directory = true;
            } else if let Some(template) = possible
                .iter()
                .find(|template| template.as_bytes() == entry)
            {
                template_file |= template.contains("\tdots/");
            } else {
                return Ok(None);
            }
        }
        if template_directory && !template_file {
            return Ok(None);
        }
        Ok(Some(oid))
    }

    pub fn head(&self) -> Result<Option<String>, Fail> {
        optional_line(self.output(&["symbolic-ref", "--no-recurse", "-q", "HEAD"])?)
    }

    pub fn commit(&self, name: &str) -> Result<Option<String>, Fail> {
        optional_line(self.output(&[
            "--no-replace-objects",
            "rev-parse",
            "--verify",
            "-q",
            name,
        ])?)
    }

    pub fn has_origin(&self) -> Result<bool, Fail> {
        let remotes = self.checked(&["remote"], "list remotes")?;
        Ok(remotes
            .stdout
            .split(|byte| *byte == b'\n')
            .any(|name| name == b"origin"))
    }
}

fn seed_tree(commit: &[u8]) -> Option<&str> {
    let separator = commit.windows(2).position(|pair| pair == b"\n\n")?;
    if &commit[separator + 2..] != SEED_MESSAGE {
        return None;
    }
    let mut headers = commit[..separator].split(|byte| *byte == b'\n');
    let tree = headers.next()?.strip_prefix(b"tree ")?;
    if headers.any(|line| line.starts_with(b"parent ")) {
        return None;
    }
    std::str::from_utf8(tree).ok()
}

fn tree_directory(entry: &[u8]) -> Option<&[u8]> {
    let head = entry.strip_prefix(b"040000 tree ")?;
    let tab = head.iter().position(|byte| *byte == b'\t')?;
    Some(&head[tab + 1..])
}

pub(super) fn trim_line(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

fn optional_line(out: Output) -> Result<Option<String>, Fail> {
    match out.status.code() {
        Some(0) => line(out).map(Some),
        Some(1) => Ok(None),
        _ => Err(git_error("read a repository reference")),
    }
}

fn checked(out: Output, action: &str) -> Result<Output, Fail> {
    if out.status.success() {
        Ok(out)
    } else {
        Err(git_error(action))
    }
}

fn line(out: Output) -> Result<String, Fail> {
    String::from_utf8(trim_line(&out.stdout).to_vec())
        .map_err(|_| git_error("read a Git object ID"))
}

fn output(cmd: &mut Command) -> Result<Output, Fail> {
    cmd.stdin(Stdio::null()).output().map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            Fail::new(1, "Git is required by dots init")
        } else {
            Fail::new(1, format!("could not run Git for dots init: {err}"))
        }
    })
}

fn input(cmd: &mut Command, bytes: &[u8], action: &str) -> Result<Output, Fail> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| git_error(action))?;
    let written = child.stdin.take().expect("piped stdin").write_all(bytes);
    let out = child.wait_with_output().map_err(|_| git_error(action))?;
    written.map_err(|_| git_error(action))?;
    checked(out, action)
}
