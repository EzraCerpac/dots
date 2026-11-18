use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::{Ctx, process::Cwd, run::Run};

pub(super) type Tree = BTreeMap<PathBuf, Entry>;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(super) struct Entry {
    pub mode: String,
    pub oid: String,
}

#[derive(Clone)]
pub(super) struct Git<'a> {
    ctx: &'a Ctx,
    cwd: PathBuf,
    index: Option<PathBuf>,
    objects: Option<PathBuf>,
    run: Option<&'a Run<'a>>,
}

impl<'a> Git<'a> {
    pub fn new(ctx: &'a Ctx, cwd: &Path) -> Self {
        Self {
            ctx,
            cwd: cwd.into(),
            index: None,
            objects: None,
            run: None,
        }
    }

    pub fn supervised(run: &'a Run<'a>) -> Self {
        let ctx = run.ctx();
        Self {
            run: Some(run),
            ..Self::new(ctx, ctx.root.path())
        }
    }

    pub fn at(&self, cwd: &Path) -> Self {
        Self {
            run: self.run,
            ..Self::new(self.ctx, cwd)
        }
    }

    pub fn indexed(&self, index: &Path) -> Self {
        Self {
            index: Some(index.into()),
            ..self.clone()
        }
    }

    pub fn quarantined(&self, objects: &Path) -> Self {
        Self {
            objects: Some(objects.into()),
            ..self.clone()
        }
    }

    pub fn root(&self) -> &Path {
        &self.cwd
    }

    pub fn command(
        &self,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> io::Result<Command> {
        if self.run.is_some_and(Run::cancelled) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        // Checkout selection must be independent of the caller's Git overrides.
        // Keep the user's normal Git authentication environment.
        let mut cmd = self
            .ctx
            .command_with_profile("git", Cwd::Dir(&self.cwd), OsStr::new(""))
            .map_err(io::Error::other)?;
        for name in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_NAMESPACE",
            "GIT_INDEX_FILE",
            "GIT_ATTR_SOURCE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_QUARANTINE_PATH",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_PREFIX",
            "GIT_SHALLOW_FILE",
            "GIT_REPLACE_REF_BASE",
            "GIT_TRACE",
            "GIT_TRACE_PACKET",
            "GIT_TRACE_PACKFILE",
            "GIT_TRACE_CURL",
            "GIT_TRACE2",
            "GIT_TRACE2_EVENT",
            "GIT_TRACE2_PERF",
            "GIT_CURL_VERBOSE",
        ] {
            cmd.env_remove(name);
        }
        cmd.args(["-c", "protocol.ext.allow=never", "-c", "gc.auto=0"])
            .args(args)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_GRAFT_FILE", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/usr/bin/false")
            .env("SSH_ASKPASS", "/usr/bin/false")
            .env("GCM_INTERACTIVE", "Never")
            .stdin(Stdio::null());
        if let Some(index) = &self.index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        if let Some(objects) = &self.objects {
            cmd.env("GIT_OBJECT_DIRECTORY", objects);
        }
        Ok(cmd)
    }

    pub fn output(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> io::Result<Output> {
        let mut command = self.command(args)?;
        match self.run {
            Some(run) => run.probe(command),
            None => command.output(),
        }
    }

    pub fn get(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> io::Result<Vec<u8>> {
        checked(self.output(args)?)
    }

    pub fn text(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> io::Result<String> {
        let mut bytes = self.get(args)?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        String::from_utf8(bytes).map_err(io::Error::other)
    }

    pub fn file_path(
        &self,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> io::Result<PathBuf> {
        let mut bytes = self.get(args)?;
        if bytes.pop() != Some(b'\n') || bytes.contains(&0) || bytes.is_empty() {
            return Err(invalid("invalid Git path"));
        }
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }

    pub fn id(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> io::Result<String> {
        oid(self.text(args)?.as_bytes())
    }

    pub fn tree(&self, revision: &str) -> io::Result<Tree> {
        let output = self.get(["ls-tree", "-r", "-z", revision])?;
        let mut tree = Tree::new();
        for record in records(&output) {
            let (header, name) = split(record, b'\t')?;
            let mut fields = header.split(|b| *b == b' ');
            let mode = fields.next().ok_or_else(|| invalid("missing tree mode"))?;
            if !matches!(mode, b"100644" | b"100755" | b"120000" | b"160000") {
                return Err(invalid("unsupported tree mode"));
            }
            let _kind = fields.next().ok_or_else(|| invalid("missing tree kind"))?;
            let id = oid(fields
                .next()
                .ok_or_else(|| invalid("missing tree object"))?)?;
            tree.insert(
                relative(name)?,
                Entry {
                    mode: String::from_utf8_lossy(mode).into_owned(),
                    oid: id,
                },
            );
        }
        Ok(tree)
    }

    pub fn ancestor(&self, ancestor: &str, descendant: &str) -> io::Result<bool> {
        match self
            .output(["merge-base", "--is-ancestor", ancestor, descendant])?
            .status
            .code()
        {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(invalid("could not verify commit ancestry")),
        }
    }

    pub fn update_index(&self, input: &[u8]) -> io::Result<()> {
        let mut data = tempfile::tempfile()?;
        data.write_all(input)?;
        data.rewind()?;
        let output = self
            .command(["update-index", "-z", "--index-info"])?
            .stdin(Stdio::from(data))
            .output()?;
        checked(output).map(drop)
    }
}

pub(super) fn relative(path: &[u8]) -> io::Result<PathBuf> {
    let path = PathBuf::from(OsStr::from_bytes(path));
    if path.as_os_str().is_empty() || path.components().any(|part| {
        !matches!(part, Component::Normal(name) if !name.as_bytes().eq_ignore_ascii_case(b".git"))
    }) {
        return Err(invalid("unsafe repository path"));
    }
    Ok(path)
}

pub(super) fn local(path: &Path) -> bool {
    let Some(Component::Normal(first)) = path.components().next() else {
        return false;
    };
    let first = first.as_bytes();
    matches!(
        first,
        b"miserc.toml" | b"config.local.toml" | b"mise.lock" | b"locks" | b".setup-state"
    ) || (first.starts_with(b"config.host-") && first.ends_with(b".toml"))
        || (first.starts_with(b"mise.") && first.ends_with(b".lock"))
}

pub(super) fn oid(raw: &[u8]) -> io::Result<String> {
    if matches!(raw.len(), 40 | 64)
        && raw
            .iter()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Ok(String::from_utf8_lossy(raw).into_owned())
    } else {
        Err(invalid("invalid object ID"))
    }
}

pub(super) fn records(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    output.split(|b| *b == 0).filter(|row| !row.is_empty())
}

pub(super) fn split(input: &[u8], delimiter: u8) -> io::Result<(&[u8], &[u8])> {
    let position = input
        .iter()
        .position(|b| *b == delimiter)
        .ok_or_else(|| invalid("invalid Git output"))?;
    Ok((&input[..position], &input[position + 1..]))
}

pub(super) fn checked(output: Output) -> io::Result<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(invalid("Git subprocess did not succeed"))
    }
}

pub(super) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(super) fn index_row(buf: &mut Vec<u8>, path: &Path, entry: Option<&Entry>, oid_len: usize) {
    match entry {
        Some(entry) => write!(buf, "{} {}\t", entry.mode, entry.oid).unwrap(),
        None => write!(buf, "0 {}\t", "0".repeat(oid_len)).unwrap(),
    }
    buf.extend_from_slice(path.as_os_str().as_bytes());
    buf.push(0);
}

/// No network child owns the user's terminal. Killing the process group
/// also stops transports and credential helpers spawned by Git.
pub(super) fn bounded(mut cmd: Command, run: &Run<'_>, budget: Duration) -> io::Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    cmd.stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    // SAFETY: setsid is async-signal-safe. The child gets its own process
    // group and has no controlling terminal to open for a credential prompt.
    unsafe {
        cmd.pre_exec(|| {
            if nix::libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    if run.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let deadline = Instant::now() + budget;
    let mut child = cmd.spawn()?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if !run.cancelled() && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            result => {
                // SAFETY: setsid created a group with child.id() as
                // its PGID. Its leader is still owned by this Child.
                unsafe { nix::libc::kill(-(child.id() as i32), nix::libc::SIGKILL) };
                let _ = child.wait();
                result?;
                let kind = if run.cancelled() {
                    io::ErrorKind::Interrupted
                } else {
                    io::ErrorKind::TimedOut
                };
                return Err(kind.into());
            }
        }
    };
    stdout.rewind()?;
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    stdout.read_to_end(&mut output.stdout)?;
    stderr.rewind()?;
    stderr.read_to_end(&mut output.stderr)?;
    Ok(output)
}

pub(super) fn repository_dirs(git: &Git<'_>) -> io::Result<(PathBuf, PathBuf)> {
    let root = git
        .file_path(["rev-parse", "--show-toplevel"])?
        .canonicalize()?;
    if root != git.root() {
        return Err(invalid("configuration must be a Git checkout root"));
    }
    Ok((
        git.file_path(["rev-parse", "--absolute-git-dir"])?
            .canonicalize()?,
        git.file_path(["rev-parse", "--path-format=absolute", "--git-common-dir"])?
            .canonicalize()?,
    ))
}
