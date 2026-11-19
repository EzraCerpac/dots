mod mise;

pub use mise::Mise;

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus};

use nix::libc;

use crate::{Ctx, Fail};

pub(crate) use mise::{config_paths, mise_binary, quiet_probe, validate_selection};

const HOOK_ENV: &[&str] = &[
    "DOTS_CHANGES",
    "DOTS_TOOLS_OK",
    "DOTS_TOOLS_UPGRADED",
    "DOTS_PRIVATE_READY",
    "DOTS_RESTORED",
    "DOTS_BUNDLE_FD",
];

fn staged_env() -> impl Iterator<Item = &'static str> {
    [
        "DOTS_ROOT",
        "DOTS_PROFILE",
        "DOTS_HOME",
        "DOTS_STAGES",
        "HOMEBREW_NO_INSTALL_CLEANUP",
    ]
    .into_iter()
    .chain(HOOK_ENV.iter().copied())
}

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

    pub(crate) fn pinned_executable(&self) -> io::Result<std::path::PathBuf> {
        Ok(self
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
                    .permissions(fs::Permissions::from_mode(0o700))
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
                error.raw_os_error().map_or_else(
                    || io::Error::new(error.kind(), error.to_string()),
                    io::Error::from_raw_os_error,
                )
            })?
            .path()
            .join("dots"))
    }

    pub(crate) fn command_with_profile(
        &self,
        program: impl AsRef<OsStr>,
        cwd: Cwd<'_>,
        profile: &OsStr,
    ) -> Result<Command, Fail> {
        let pinned = self.pinned_executable().map_err(|error| {
            Fail::new(
                1,
                format!("could not keep dots on its children's PATH: {error}"),
            )
        })?;
        let bin = pinned.parent().expect("pinned dots directory");
        let path = prepend_to_path(bin, self.env.get("PATH"))?;
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
        for key in ["MISE_ENV", "DOTS_HOME", "DOTS_STAGES"]
            .into_iter()
            .chain(HOOK_ENV.iter().copied())
        {
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
    let invalid = || Fail::usage("invalid handoff for dots child");
    let mut fields = encoded
        .strip_suffix(&[0])
        .ok_or_else(invalid)?
        .split(|byte| *byte == 0);
    let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
    for key in staged_env() {
        match fields.next().ok_or_else(invalid)?.split_first() {
            Some((b'+', value)) => {
                environment.insert(key.into(), OsStr::from_bytes(value).to_owned());
            }
            // Up stamps this value for its own stages. Ordinary commands
            // otherwise keep the effective config's Homebrew setting.
            None if key == "HOMEBREW_NO_INSTALL_CLEANUP" => {}
            None => {
                environment.remove(OsStr::new(key));
            }
            _ => return Err(invalid()),
        }
    }

    let program = fields
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(invalid)?;
    let path = prepend_to_path(
        bin,
        environment.get(OsStr::new("PATH")).map(OsString::as_os_str),
    )?;
    environment.insert("PATH".into(), path.clone());
    let argv = std::iter::once(program)
        .chain(fields)
        .map(|arg| CString::new(arg).map_err(|_| invalid()))
        .collect::<Result<Vec<_>, _>>()?;
    let environment = environment
        .into_iter()
        .map(|(key, value)| {
            let mut entry = key.into_encoded_bytes();
            entry.push(b'=');
            entry.extend(value.into_encoded_bytes());
            CString::new(entry).map_err(|_| invalid())
        })
        .collect::<Result<Vec<_>, _>>()?;

    let pointers = |strings: &[CString]| {
        strings
            .iter()
            .map(|s| s.as_ptr())
            .chain([std::ptr::null()])
            .collect::<Vec<_>>()
    };
    let argv_pointers = pointers(&argv);
    let env_pointers = pointers(&environment);
    // Rust ignores SIGPIPE in the launcher. Give the executed program the
    // native default, as Command does when starting a child.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let error = exec_search(OsStr::from_bytes(program), &path, |candidate| {
        let Ok(candidate) = CString::new(candidate.as_os_str().as_bytes()) else {
            return io::Error::from_raw_os_error(libc::EINVAL);
        };
        // The filename and both nul-terminated pointer arrays live across execve.
        unsafe {
            libc::execve(
                candidate.as_ptr(),
                argv_pointers.as_ptr(),
                env_pointers.as_ptr(),
            )
        };
        io::Error::last_os_error()
    });
    Err(Fail::new(
        1,
        format!(
            "could not run {}: {error}",
            Path::new(OsStr::from_bytes(program)).display()
        ),
    ))
}

fn exec_search(
    program: &OsStr,
    path: &OsStr,
    mut exec: impl FnMut(&Path) -> io::Error,
) -> io::Error {
    if program.as_bytes().contains(&b'/') {
        return exec(Path::new(program));
    }
    let mut denied = None;
    for dir in std::env::split_paths(path) {
        let error = exec(&dir.join(program));
        match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR) => {}
            Some(libc::EACCES) => denied = Some(error),
            // A file that exists but has no executable format must fail here.
            _ => return error,
        }
    }
    denied.unwrap_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
}

fn stage_argv<I, S>(
    bin: &Path,
    command: &Command,
    program: &OsStr,
    args: I,
) -> Result<OsString, Fail>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // Pass mise only the UTF-8 launcher path and this filename. The private
    // file holds raw argv and the designated environment, never bundle bytes.
    let stage = || -> io::Result<OsString> {
        let mut file = tempfile::Builder::new().prefix("argv-").tempfile_in(bin)?;
        let mut append = |bytes: &[u8], present: bool| -> io::Result<()> {
            if bytes.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "handoff contains NUL",
                ));
            }
            if present {
                file.write_all(b"+")?;
            }
            file.write_all(bytes)?;
            file.write_all(&[0])
        };
        for key in staged_env() {
            let value = command
                .get_envs()
                .find(|(name, _)| *name == key)
                .and_then(|(_, value)| value);
            if key == "DOTS_BUNDLE_FD"
                && value.is_some_and(|value| {
                    value
                        .to_str()
                        .and_then(|fd| fd.parse::<u32>().ok())
                        .is_none()
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid bundle descriptor",
                ));
            }
            append(value.map_or(&[], OsStr::as_bytes), value.is_some())?;
        }
        append(program.as_bytes(), false)?;
        for arg in args {
            append(arg.as_ref().as_bytes(), false)?;
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

pub use crate::bootstrap::bundle_pipe::BundlePipe;

#[cfg(test)]
mod launcher_tests {
    use super::*;

    #[test]
    fn path_search_skips_missing_and_denied_entries_and_stops_on_enoexec() {
        let mut candidates = Vec::new();
        let error = exec_search(
            "example".as_ref(),
            "/missing:/denied:/plain:/later".as_ref(),
            |path| {
                candidates.push(path.to_owned());
                io::Error::from_raw_os_error(match path.parent().unwrap().to_str().unwrap() {
                    "/missing" => libc::ENOENT,
                    "/denied" => libc::EACCES,
                    _ => libc::ENOEXEC,
                })
            },
        );
        assert_eq!(error.raw_os_error(), Some(libc::ENOEXEC));
        assert_eq!(
            candidates,
            ["/missing/example", "/denied/example", "/plain/example"].map(std::path::PathBuf::from)
        );
    }
}
