use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Seek, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    run: Option<&'a Run<'a>>,
}

impl<'a> Git<'a> {
    pub fn new(ctx: &'a Ctx, cwd: &Path) -> Self {
        Self {
            ctx,
            cwd: cwd.into(),
            index: None,
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
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_GRAFT_FILE", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/usr/bin/false")
            .env("SSH_ASKPASS", "/usr/bin/false")
            .env("GCM_INTERACTIVE", "Never")
            .stdin(Stdio::null());
        if let Some(index) = &self.index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        Ok(cmd)
    }

    pub fn output(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> io::Result<Output> {
        self.output_command(self.command(args)?)
    }

    pub fn output_command(&self, mut command: Command) -> io::Result<Output> {
        match self.run {
            Some(run) => run.probe(command),
            None => command.output(),
        }
    }

    pub fn output_with_stdin(
        &self,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
        input: Stdio,
    ) -> io::Result<Output> {
        let mut command = self.command(args)?;
        match self.run {
            Some(run) => run.probe_with_stdin(command, input),
            None => command.stdin(input).output(),
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

    pub fn commit(&self, tree: &str, parents: &[&str], message: &str) -> io::Result<String> {
        let git = || {
            let mut cmd = self.command(["-c", "commit.gpgSign=false"])?;
            cmd.env("LC_ALL", "C");
            Ok::<_, io::Error>(cmd)
        };
        let command = || {
            let mut cmd = git()?;
            cmd.args(["commit-tree", tree]);
            for parent in parents {
                cmd.args(["-p", parent]);
            }
            cmd.args(["-m", message]);
            Ok::<_, io::Error>(cmd)
        };
        let mut output = self.output_command(command()?)?;
        if !output.status.success()
            && identity_unknown(&first_stderr(&output))
            && self
                .output(["config", "--get", "user.useConfigOnly"])?
                .status
                .code()
                == Some(1)
        {
            let mut cmd = command()?;
            crate::process::fill_commit_identity(&mut cmd, |ident, defaults| {
                let mut probe = git()?;
                probe.args(["var", ident]).envs(defaults.iter().copied());
                self.output_command(probe)
            })?;
            output = self.output_command(cmd)?;
        }
        oid(checked(output)?.trim_ascii())
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

    pub fn ensure_dense_index(&self) -> io::Result<()> {
        let sparse = self.output(["config", "--type=bool", "--get", "core.sparseCheckout"])?;
        match (sparse.status.code(), sparse.stdout.as_slice()) {
            (Some(0), b"true\n") => return Err(invalid("sparse Git checkout")),
            (Some(0), b"false\n") | (Some(1), b"") => (),
            _ => return Err(invalid("could not verify Git sparse-checkout settings")),
        }
        // update-index --skip-worktree works independently of sparse checkout.
        let entries = self.get([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "--cached",
            "--sparse",
            "-t",
            "-z",
        ])?;
        if records(&entries).any(|entry| entry.starts_with(b"S ")) {
            return Err(invalid("Git index has skip-worktree entries"));
        }
        Ok(())
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
        let output =
            self.output_with_stdin(["update-index", "-z", "--index-info"], Stdio::from(data))?;
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
    // These root names can select the same on-disk file on macOS.
    let first = first.as_bytes().to_ascii_lowercase();
    matches!(
        first.as_slice(),
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
        let reason = first_stderr(&output);
        if reason.is_empty() {
            Err(invalid("Git subprocess did not succeed"))
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidData, reason))
        }
    }
}

fn first_stderr(output: &Output) -> String {
    let line = output
        .stderr
        .split(|&byte| byte == b'\n')
        .next()
        .unwrap_or_default();
    String::from_utf8_lossy(&line[..line.len().min(512)])
        .trim_end()
        .to_owned()
}

pub(super) fn identity_unknown(reason: &str) -> bool {
    matches!(
        reason,
        "Author identity unknown" | "Committer identity unknown"
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Root;
    use crate::run::Sink;
    use crate::sync::remote::{self, Origin, PushError};
    use crate::sync::tests::{fixture_commit, git_fixture};
    use crate::{Env, Flags};
    use nix::libc;
    use std::ffi::CString;
    use std::fs::{self, File, OpenOptions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::time::{Duration, Instant};

    const MODE: &str = "DOTS_TEST_SYNC_NETWORK_MODE";
    const FIFO: &str = "DOTS_TEST_SYNC_HOOK_FIFO";
    const CLEANUP: &str = "DOTS_TEST_SYNC_HOOK_CLEANUP";

    fn hook_writer(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
    }

    fn wait_for_hook<T>(mut check: impl FnMut() -> Option<T>) -> T {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = check() {
                return result;
            }
            assert!(Instant::now() < until, "pre-push hook did not change state");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Run in a separate process so a real SIGINT cannot cancel any parallel test.
    #[test]
    fn git_transport_waits_for_the_pre_push_group() {
        if let Ok(mode) = std::env::var(MODE) {
            transport_child(&mode);
            return;
        }
        for mode in ["graceful", "stubborn", "timeout"] {
            let (_world, ctx) = git_fixture();
            let output = Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .envs(&ctx.env.vars)
                .env(MODE, mode)
                .current_dir(ctx.root.path())
                .args([
                    "--exact",
                    "sync::git::tests::git_transport_waits_for_the_pre_push_group",
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{mode}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn transport_child(mode: &str) {
        let mut env = Env::current().unwrap();
        let root = Root::resolve(&env).unwrap();
        let fifo = root.path().with_file_name("hook-fifo");
        let marker = root.path().with_file_name("hook-cleanup");
        env.vars.insert(FIFO.into(), fifo.as_os_str().into());
        env.vars.insert(CLEANUP.into(), marker.as_os_str().into());
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: This is a valid C path inside the private fixture.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let ctx = Ctx {
            root,
            env,
            flags: Flags::default(),
        };
        let git = Git::new(&ctx, ctx.root.path());
        let remote_path = ctx.root.path().with_file_name("remote");
        let url = format!("file://{}", remote_path.display());
        git.get(["init", "--quiet", "--bare", remote_path.to_str().unwrap()])
            .unwrap();
        git.get(["remote", "add", "origin", &url]).unwrap();
        let revision = fixture_commit(&git, "candidate");
        let hook = ctx.root.path().join(".git/hooks/pre-push");
        let trap = if mode == "graceful" {
            "trap 'printf cleanup > \"$DOTS_TEST_SYNC_HOOK_CLEANUP\"; exit 0' INT"
        } else {
            "trap '' INT TERM"
        };
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\n{trap}\nexec 3<\"$DOTS_TEST_SYNC_HOOK_FIFO\"\nIFS= read -r wait <&3\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();

        let run = Run::begin(&ctx, Sink::Unreported);
        let git = Git::supervised(&run);
        let origin = Origin::load(&git, None).unwrap().unwrap();
        let interrupt = mode != "timeout";
        let controller = std::thread::spawn({
            let fifo = fifo.clone();
            move || {
                // Keep the writer alive so EOF cannot retire a stubborn hook.
                let writer = wait_for_hook(|| hook_writer(&fifo).ok());
                let attached = Instant::now();
                if interrupt {
                    // SAFETY: only this isolated child has installed the tested Run.
                    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGINT) }, 0);
                }
                (writer, attached)
            }
        });
        let budget = Duration::from_secs(2);
        let start = Instant::now();
        if interrupt {
            assert_eq!(
                remote::push(&git, &run, &origin, None, &revision),
                Err(PushError::Unconfirmed)
            );
            assert!(run.cancelled());
        } else {
            let command = git
                .command(["push", "origin", "HEAD:refs/heads/dev"])
                .unwrap();
            assert_eq!(
                run.probe_detached(command, budget).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            assert!(start.elapsed() >= budget);
            assert!(!run.cancelled());
        }
        let (_writer, attached) = controller.join().unwrap();
        if mode == "graceful" {
            assert_eq!(fs::read(marker).unwrap(), b"cleanup");
        } else if mode == "stubborn" {
            assert!(attached.elapsed() >= Duration::from_millis(500));
        }
        // ENXIO means the hook closed its read end, independent of PID reuse.
        assert_eq!(
            hook_writer(&fifo).unwrap_err().raw_os_error(),
            Some(libc::ENXIO)
        );
    }
}
