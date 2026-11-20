use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use nix::{libc, unistd::geteuid};

use crate::{Env, Fail};

pub fn version(env: &Env) -> Result<String, Fail> {
    let origin = ExecutableOrigin::current(env).map_err(|error| {
        Fail::new(
            1,
            format!("could not locate the running dots executable: {error}"),
        )
    })?;
    Ok(format!(
        "dots {} ({})",
        env!("CARGO_PKG_VERSION"),
        origin.installer
    ))
}

const ORIGIN_FILE: &str = "installation";
pub(crate) const PIN_PREFIX: &str = "dots-run-";

pub(crate) struct ExecutableOrigin {
    pub path: PathBuf,
    pub installer: &'static str,
}

impl ExecutableOrigin {
    pub fn current(env: &Env) -> io::Result<Self> {
        Self::from_executable(&std::env::current_exe()?.canonicalize()?, env)
    }

    pub fn from_executable(executable: &Path, env: &Env) -> io::Result<Self> {
        // A nested command runs the pinned binary. The pin's private directory
        // carries the original installation independently of the child's env.
        if executable.file_name() == Some(OsStr::new("dots"))
            && let Some(dir) = executable.parent()
            && dir
                .file_name()
                .is_some_and(|name| name.as_bytes().starts_with(PIN_PREFIX.as_bytes()))
            && let Some(origin) = Self::read_from_pin(dir)?
        {
            return Ok(origin);
        }
        Ok(Self {
            path: executable.to_owned(),
            installer: installer(executable, env),
        })
    }

    pub fn write(&self, directory: &Path) -> io::Result<()> {
        let mut data = self.installer.as_bytes().to_vec();
        data.push(0);
        data.extend_from_slice(self.path.as_os_str().as_bytes());
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(ORIGIN_FILE))?
            .write_all(&data)
    }

    fn read_from_pin(directory: &Path) -> io::Result<Option<Self>> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(directory)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c"installation".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // openat returned a fresh descriptor within the opened pin directory.
        let file = unsafe { File::from_raw_fd(fd) };
        let dir = directory.metadata()?;
        let marker = file.metadata()?;
        let uid = geteuid().as_raw();
        if dir.uid() != uid
            || dir.mode() & 0o022 != 0
            || marker.uid() != uid
            || marker.mode() & 0o077 != 0
            || !marker.is_file()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "untrusted dots pin installation",
            ));
        }
        let mut data = Vec::new();
        const LIMIT: u64 = 16 * 1024;
        file.take(LIMIT + 1).read_to_end(&mut data)?;
        if data.len() as u64 > LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized dots pin installation",
            ));
        }
        Self::decode(&data).map(Some)
    }

    fn decode(data: &[u8]) -> io::Result<Self> {
        let invalid =
            || io::Error::new(io::ErrorKind::InvalidData, "invalid dots pin installation");
        let delimiter = data
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(invalid)?;
        let (kind, path) = data.split_at(delimiter);
        let installer = match kind {
            b"homebrew" => "homebrew",
            b"mise" => "mise",
            b"cargo" => "cargo",
            b"other" => "other",
            _ => return Err(invalid()),
        };
        if path[1..].contains(&0) {
            return Err(invalid());
        }
        let path = PathBuf::from(OsStr::from_bytes(&path[1..]));
        if !path.is_absolute() {
            return Err(invalid());
        }
        Ok(Self { path, installer })
    }
}

fn installer(exe: &Path, env: &Env) -> &'static str {
    let components: Vec<_> = exe.iter().collect();
    if components.windows(5).any(|part| {
        part[0] == "Cellar" && part[1] == "dots" && part[3] == "bin" && part[4] == "dots"
    }) {
        return "homebrew";
    }

    let in_mise_data = mise_installs(env)
        .and_then(|dir| dir.canonicalize().ok())
        .is_some_and(|dir| exe.starts_with(dir));
    let in_mise_layout = components
        .windows(2)
        .any(|part| part == ["mise", "installs"]);
    if in_mise_data || in_mise_layout {
        return "mise";
    }

    if let Some(bin) = exe.parent()
        && bin.file_name().is_some_and(|name| name == "bin")
    {
        let cargo_home = env
            .get("CARGO_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| env.home().map(|home| home.join(".cargo")));
        let cargo_roots = cargo_home
            .into_iter()
            .chain(env.get("CARGO_INSTALL_ROOT").map(PathBuf::from));
        if cargo_roots
            .filter_map(|dir| env.cwd().join(dir).join("bin").canonicalize().ok())
            .any(|dir| dir == bin)
            || bin
                .parent()
                .is_some_and(|dir| dir.join(".crates2.json").is_file())
        {
            return "cargo";
        }
    }
    "other"
}

pub(crate) fn mise_installs(env: &Env) -> Option<PathBuf> {
    let mise_data = env
        .get("MISE_DATA_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("XDG_DATA_HOME")
                .filter(|path| !path.is_empty())
                .map(|path| Path::new(path).join("mise"))
        })
        .or_else(|| env.home().map(|home| home.join(".local/share/mise")));
    env.get("MISE_INSTALLS_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| mise_data.map(|dir| dir.join("installs")))
        .map(|dir| env.cwd().join(dir))
}
