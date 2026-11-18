mod mise;

pub use mise::Mise;

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus};

use crate::{Ctx, Fail};

pub(crate) use mise::{config_paths, mise_binary, quiet_probe, validate_selection};

pub enum Cwd<'a> {
    Root,
    Caller,
    Dir(&'a Path),
}

impl Ctx {
    pub fn command(&self, program: impl AsRef<OsStr>, cwd: Cwd<'_>) -> Result<Command, Fail> {
        let selected = crate::config::selection::read(&self.root, &self.env)?;
        self.command_with_profile(
            program,
            cwd,
            selected.profile.as_deref().unwrap_or_default().as_ref(),
        )
    }

    pub(crate) fn command_with_profile(
        &self,
        program: impl AsRef<OsStr>,
        cwd: Cwd<'_>,
        profile: &OsStr,
    ) -> Result<Command, Fail> {
        let bin = self
            .root
            .child_bin
            .get_or_init(|| {
                let temp = std::path::absolute(std::env::temp_dir())?;
                let temp = if temp.to_str().is_none_or(|path| path.contains(':')) {
                    Path::new("/tmp")
                } else {
                    temp.as_path()
                };
                let directory = tempfile::Builder::new()
                    .prefix("dots-run-")
                    .tempdir_in(temp)?;
                let source = std::env::current_exe()?.canonicalize()?;
                let pinned = directory.path().join("dots");
                // A version can be pruned while a child of this run is still active.
                if fs::hard_link(&source, &pinned).is_err() {
                    fs::copy(&source, &pinned)?;
                }
                Ok::<_, io::Error>(directory)
            })
            .as_ref()
            .map_err(|error| {
                Fail::new(
                    1,
                    format!("could not keep dots on its children's PATH: {error}"),
                )
            })?;
        let path = prepend_to_path(bin.path(), self.env.get("PATH"))?;
        let cwd = match cwd {
            Cwd::Root => self.root.path(),
            Cwd::Caller => self.env.cwd(),
            Cwd::Dir(path) => path,
        };
        let mut command = Command::new(program);
        command
            .current_dir(self.env.cwd().join(cwd))
            .env_clear()
            .envs(&self.env.vars)
            .env("DOTS_ROOT", self.root.path())
            .env("DOTS_PROFILE", profile)
            .env("PATH", path);
        for key in [
            "MISE_ENV",
            "DOTS_HOME",
            "DOTS_STAGES",
            "DOTS_CHANGES",
            "DOTS_TOOLS_OK",
            "DOTS_TOOLS_UPGRADED",
            "DOTS_PRIVATE_READY",
            "DOTS_RESTORED",
            "DOTS_BUNDLE_FD",
        ] {
            command.env_remove(key);
        }
        Ok(command)
    }
}

fn prepend_to_path(bin: &Path, existing: Option<&OsStr>) -> Result<OsString, Fail> {
    let other = existing
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|path| path != bin);
    std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(other))
        .map_err(|error| Fail::new(1, format!("could not construct child PATH: {error}")))
}

/// Entry point used only by dots-owned launches through mise exec. Mise has
/// already built the environment; replacing this process preserves its stdio
/// and any hook descriptors that the handler attached to the mise process.
pub fn exec_pinned(args: &[OsString]) -> Result<u8, Fail> {
    let executable = std::env::current_exe().map_err(|error| {
        Fail::new(
            1,
            format!("could not locate the running dots executable: {error}"),
        )
    })?;
    let bin = executable.parent().expect("absolute executable path");
    let [name] = args else {
        return Err(Fail::usage("missing argument file for dots child"));
    };
    if !name.as_encoded_bytes().starts_with(b"argv-")
        || Path::new(name).file_name() != Some(name.as_os_str())
    {
        return Err(Fail::usage("invalid argument file for dots child"));
    }
    let encoded = fs::read(bin.join(name))
        .map_err(|error| Fail::new(1, format!("could not read dots child arguments: {error}")))?;
    let mut argv = encoded
        .strip_suffix(&[0])
        .ok_or_else(|| Fail::usage("invalid arguments for dots child"))?
        .split(|byte| *byte == 0)
        .map(OsStr::from_bytes);
    let program = argv
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Fail::usage("missing program for dots child"))?;
    let existing = std::env::var_os("PATH");
    let path = prepend_to_path(bin, existing.as_deref())?;
    let error = Command::new(program).args(argv).env("PATH", path).exec();
    Err(Fail::new(
        1,
        format!("could not run {}: {error}", Path::new(program).display()),
    ))
}

fn stage_argv<I, S>(bin: &Path, program: &OsStr, args: I) -> Result<OsString, Fail>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // mise's CLI converts non-UTF-8 arguments to text. Hand it only the
    // UTF-8 private executable path and this filename.
    let stage = || -> io::Result<OsString> {
        let mut file = tempfile::Builder::new().prefix("argv-").tempfile_in(bin)?;
        let mut append = |arg: &OsStr| -> io::Result<()> {
            let bytes = arg.as_bytes();
            if bytes.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "argument contains NUL",
                ));
            }
            file.write_all(bytes)?;
            file.write_all(&[0])
        };
        append(program)?;
        for arg in args {
            append(arg.as_ref())?;
        }
        let filename = file
            .path()
            .file_name()
            .expect("temporary file has a name")
            .to_owned();
        file.into_temp_path().keep().map_err(|error| error.error)?;
        Ok(filename)
    };
    stage().map_err(|error| Fail::new(1, format!("could not stage dots child arguments: {error}")))
}

pub(crate) fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // access reads the path synchronously and does not retain the pointer.
    unsafe { nix::libc::access(c_path.as_ptr(), nix::libc::X_OK) == 0 }
}

pub(crate) fn status_code(status: ExitStatus) -> u8 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

pub struct BundlePipe {
    _private: (),
}

impl BundlePipe {
    pub fn new(_json: &[u8]) -> Result<Self, Fail> {
        Err(Fail::unported(
            "P-05",
            "preparing the bootstrap hook's secret descriptor",
        ))
    }

    /// The pipe lives through spawn. Attachment must affect only this child.
    pub fn attach(&self, _command: &mut Command) -> Result<(), Fail> {
        Err(Fail::unported(
            "P-05",
            "attaching the bootstrap hook's secret descriptor",
        ))
    }
}
