//! Additional private paths for P-07's real mise and Git subprocesses.

use crate::support::{FixtureCommand, World};
use nix::libc;
use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[allow(dead_code)] // Only suites reading reported recovery paths need this.
#[derive(Debug)]
pub struct FixtureFile {
    pub file: File,
    pub parent: File,
}

#[allow(dead_code)]
pub fn fixture_file(base: &Path, candidate: &Path) -> Option<FixtureFile> {
    open_fixture_file(base, candidate, libc::O_RDONLY)
}

#[allow(dead_code)]
pub fn fixture_editor(base: &Path, candidate: &Path) -> Option<FixtureFile> {
    open_fixture_file(base, candidate, libc::O_RDWR)
}

fn open_fixture_file(base: &Path, candidate: &Path, access: libc::c_int) -> Option<FixtureFile> {
    let (file, parent) = open_inside(base, candidate, access, true).ok()??;
    let parent = parent?;
    file.metadata()
        .ok()?
        .is_file()
        .then_some(FixtureFile { file, parent })
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn open_inside(
    base: &Path,
    candidate: &Path,
    access: libc::c_int,
    required: bool,
) -> io::Result<Option<(File, Option<File>)>> {
    const DIRECTORY: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY;
    const NO_FOLLOW: libc::c_int = libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if !candidate.is_absolute()
        || candidate
            .components()
            .any(|part| part == Component::ParentDir)
    {
        return Err(invalid("path is not absolute and confined"));
    }
    let relative = candidate
        .strip_prefix(base)
        .map_err(|_| invalid("path is outside World"))?;
    let mut dir = OpenOptions::new()
        .read(true)
        .custom_flags(DIRECTORY | NO_FOLLOW)
        .open(base)?;
    let owner = dir.metadata()?.uid();
    let mut entries = relative.components().peekable();
    while let Some(Component::Normal(name)) = entries.next() {
        let final_component = entries.peek().is_none();
        let flags = if final_component {
            access | libc::O_NONBLOCK
        } else {
            DIRECTORY
        };
        let name = CString::new(name.as_bytes())?;
        // Each component is opened beneath its already-open directory, so a
        // symlink replacement cannot redirect a subsequent open or read.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | NO_FOLLOW) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if !required && error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            };
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if file.metadata()?.uid() != owner {
            return Err(invalid("path has another owner"));
        }
        if final_component {
            return Ok(Some((file, Some(dir))));
        }
        dir = file;
    }
    Ok(Some((dir, None)))
}

fn check_directory(base: &Path, path: &Path) -> io::Result<()> {
    open_inside(base, path, libc::O_RDONLY | libc::O_DIRECTORY, true).map(|_| ())
}

fn check_mise_paths(base: &Path, argv: &[OsString]) -> io::Result<()> {
    check_directory(base, &env::current_dir()?)?;
    for (index, arg) in argv.iter().enumerate() {
        let bytes = arg.as_bytes();
        let path = match bytes {
            b"-C" | b"--cd" => Some(
                argv.get(index + 1)
                    .ok_or_else(|| invalid("missing mise directory"))?
                    .as_os_str(),
            ),
            _ => bytes
                .strip_prefix(b"--cd=")
                .or_else(|| bytes.strip_prefix(b"-C"))
                .map(OsStr::from_bytes),
        };
        if let Some(path) = path {
            check_directory(base, Path::new(path))?;
        }
    }

    const REQUIRED: &[&str] = &[
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "TMPDIR",
        "MISE_CONFIG_DIR",
        "MISE_DATA_DIR",
        "MISE_STATE_DIR",
        "MISE_CACHE_DIR",
        "MISE_TMP_DIR",
        "MISE_SYSTEM_DIR",
        "MISE_SYSTEM_CONFIG_DIR",
        "MISE_SYSTEM_CONFIG_FILE",
        "MISE_SYSTEM_DATA_DIR",
        "MISE_SYSTEM_INSTALLS_DIR",
        "MISE_SYSTEM_SHIMS_DIR",
        "MISE_INSTALLS_DIR",
        "MISE_INSTALL_STORE_DIR",
        "MISE_DOWNLOADS_DIR",
        "MISE_PLUGINS_DIR",
        "MISE_SHIMS_DIR",
    ];
    const OPTIONAL: &[&str] = &[
        "DOTS_ROOT",
        "XDG_RUNTIME_DIR",
        "MISE_CONFIG_FILE",
        "MISE_GLOBAL_CONFIG_FILE",
        "MISE_GLOBAL_CONFIG_ROOT",
        "MISE_SKILLS_DIR",
        "BASH_ENV",
        "ENV",
        "MISE_LOCAL_CONFIG_FILE",
        "MISE_LOG_FILE",
        "MISE_SOPS_AGE_KEY_FILE",
        "SOPS_AGE_KEY_FILE",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_TEMPLATE_DIR",
        "GH_CONFIG_DIR",
    ];
    const LISTS: &[&str] = &[
        "MISE_CEILING_PATHS",
        "MISE_TRUSTED_CONFIG_PATHS",
        "MISE_SHARED_INSTALL_DIRS",
        "MISE_SYSTEM_PLUGINS_DIRS",
        "XDG_CONFIG_DIRS",
        "XDG_DATA_DIRS",
        "GIT_CEILING_DIRECTORIES",
    ];
    for &key in REQUIRED.iter().chain(OPTIONAL).chain(LISTS) {
        let value = env::var_os(key);
        if !REQUIRED.contains(&key) && value.as_ref().is_none_or(|path| path.is_empty()) {
            continue;
        }
        let refusal = || io::Error::other(format!("{key} points outside approved fixture paths"));
        let value = value.ok_or_else(refusal)?;
        let paths = if LISTS.contains(&key) {
            env::split_paths(&value).collect()
        } else {
            vec![PathBuf::from(value)]
        };
        for path in paths {
            // Git needs the exact private parent as a ceiling when starting at base.
            if key == "GIT_CEILING_DIRECTORIES" && base.parent() == Some(&path) {
                check_directory(&path, &path).map_err(|_| refusal())?;
            } else {
                open_inside(base, &path, libc::O_RDONLY, false).map_err(|_| refusal())?;
            }
        }
    }
    Ok(())
}

fn run_boundary(args: &[OsString], transport: &[String]) -> io::Result<i32> {
    let [base, mode, args @ ..] = args else {
        return Err(invalid("missing fixture boundary arguments"));
    };
    let base = Path::new(base);
    // The launcher saves the tool's stdout on fd 3 and silences libtest.
    // Restore stdout for bytes written by the fixture or the executable it runs.
    if unsafe { libc::dup2(3, libc::STDOUT_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::close(3) };
    let cwd = env::current_dir()?;
    let open = |path: &OsStr| {
        fixture_file(base, &cwd.join(path))
            .ok_or_else(|| invalid("input is not a confined regular file"))
    };
    let mut lock = None;
    let mut command = match (mode.as_bytes(), args) {
        (b"read", [path]) => {
            let mut stdout = io::stdout().lock();
            io::copy(&mut open(path)?.file, &mut stdout)?;
            stdout.flush()?;
            return Ok(0);
        }
        (b"compare", [left, right]) => {
            let bytes = |path: &OsStr| {
                let mut bytes = Vec::new();
                open(path)?.file.read_to_end(&mut bytes)?;
                Ok::<_, io::Error>(bytes)
            };
            return Ok(i32::from(bytes(left)? != bytes(right)?));
        }
        (b"mise", [program, argv @ ..]) => {
            check_mise_paths(base, argv)?;
            let mut command = Command::new(program);
            command.args(argv);
            command
        }
        (b"lock-and-sleep", [lock_path, pid]) => {
            let edit = |path: &OsStr| {
                fixture_editor(base, &cwd.join(path))
                    .ok_or_else(|| invalid("not a confined fixture file"))
            };
            let held = edit(lock_path)?.file;
            held.try_lock()?;
            writeln!(edit(pid)?.file, "{}", std::process::id())?;
            // Keep the locked file open in the same process across exec.
            if unsafe { libc::fcntl(held.as_raw_fd(), libc::F_SETFD, 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
            lock = Some(held);
            let mut command = Command::new("sleep");
            command.arg("68");
            command
        }
        _ => return Err(invalid("unknown fixture boundary operation")),
    };
    command.env_remove("DOTS_FIXTURE_ARG_COUNT");
    for key in transport {
        command.env_remove(key);
    }
    let error = command.exec();
    // If exec fails, dropping the lock here releases the fixture lifetime.
    drop(lock);
    Err(error)
}

#[test]
#[ignore = "invoked by the private fixture adapters"]
fn fixture_boundary_process() {
    let Ok(count) = env::var("DOTS_FIXTURE_ARG_COUNT") else {
        return;
    };
    let keys: Vec<_> = (0..count.parse::<usize>().unwrap())
        .map(|index| format!("DOTS_FIXTURE_ARG_{index}"))
        .collect();
    let args: Vec<_> = keys.iter().map(|key| env::var_os(key).unwrap()).collect();
    match run_boundary(&args, &keys) {
        Ok(status) => std::process::exit(status),
        Err(error) => {
            eprintln!("P-07 fixture boundary refused: {error}");
            std::process::exit(1);
        }
    }
}

pub struct IsolatedWorld {
    world: World,
    private: PathBuf,
    git: PathBuf,
}

impl IsolatedWorld {
    pub fn new() -> Self {
        let world = World::new();
        let private = world.root().with_file_name("p07-private");
        for name in [
            "mise-data",
            "mise-state",
            "mise-cache",
            "mise-system",
            "mise-installs",
            "mise-store",
            "xdg-config",
            "xdg-data",
            "runtime",
            "tmp",
            "git-template",
        ] {
            fs::create_dir_all(private.join(name)).unwrap();
        }
        let git = executable_on_path(&world.git(), "git").expect("P-07 requires real Git on PATH");
        Self {
            world,
            private,
            git,
        }
    }

    pub fn root(&self) -> &Path {
        self.world.root()
    }

    pub fn home(&self) -> &Path {
        self.world.home()
    }

    pub fn dots(&self) -> FixtureCommand<'_> {
        let mut command = self.world.dots();
        self.apply_private_paths(&mut command);
        command
    }

    pub fn git(&self) -> FixtureCommand<'_> {
        self.tool(&self.git)
    }

    pub fn tool(&self, program: impl AsRef<OsStr>) -> FixtureCommand<'_> {
        let mut command = self.world.command(program);
        self.apply_private_paths(&mut command);
        command
    }

    pub fn boundary(&self) -> String {
        let adapter = self.private.join("fixture-boundary");
        // Use the current test executable so filtered Cargo tests and native
        // runner archives need no separately compiled helper. Export argv as
        // bytes to keep it out of libtest's argument parser.
        fs::write(
            &adapter,
            format!(
                r#"#!/bin/sh
set -eu
index=0
for arg in {base} "$@"; do
    export "DOTS_FIXTURE_ARG_$index=$arg"
    index=$((index + 1))
done
DOTS_FIXTURE_ARG_COUNT=$index exec {test} --ignored --exact --nocapture isolation::fixture_boundary_process 3>&1 >/dev/null
"#,
                base = quote(self.world.base()),
                test = quote(&env::current_exe().unwrap()),
            ),
        )
        .unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();
        quote(&adapter)
    }

    #[allow(dead_code)] // Only fixtures reading a subprocess-selected input need this.
    pub fn fixture_reader(&self) -> PathBuf {
        let reader = self.private.join("read-fixture-file");
        fs::write(
            &reader,
            format!("#!/bin/sh\nexec {} read \"$@\"\n", self.boundary()),
        )
        .unwrap();
        fs::set_permissions(&reader, fs::Permissions::from_mode(0o700)).unwrap();
        reader
    }

    #[allow(dead_code)] // The receipt suite supplies a mise double.
    pub fn native_mise_adapter(&self, executable: &Path) -> PathBuf {
        let adapter = self.private.join("native-mise");
        let base = quote(self.root().parent().unwrap());
        let executable = quote(executable);
        let boundary = self.boundary();
        fs::write(
            &adapter,
            format!(
                r#"#!/bin/sh
set -eu
fixture={base}
case "$HOME" in "$fixture"/*) ;; *) exit 88 ;; esac

# The packaged oracle clears MISE_* for nested materialization. Retain its
# private per-call config, data and state, and fill only absent paths.
export MISE_CONFIG_DIR="${{MISE_CONFIG_DIR:-$HOME/.config/mise}}"
export MISE_DATA_DIR="${{MISE_DATA_DIR:-$HOME/.local/share/mise}}"
export MISE_STATE_DIR="${{MISE_STATE_DIR:-$HOME/.local/state/mise}}"
export MISE_CACHE_DIR="${{MISE_CACHE_DIR:-$HOME/.cache/mise}}"
export MISE_TMP_DIR="${{MISE_TMP_DIR:-$fixture/tmp}}"
export MISE_INSTALLS_DIR="${{MISE_INSTALLS_DIR:-$MISE_DATA_DIR/installs}}"
export MISE_INSTALL_STORE_DIR="${{MISE_INSTALL_STORE_DIR:-$MISE_DATA_DIR/store}}"
export MISE_DOWNLOADS_DIR="${{MISE_DOWNLOADS_DIR:-$MISE_DATA_DIR/downloads}}"
export MISE_PLUGINS_DIR="${{MISE_PLUGINS_DIR:-$MISE_DATA_DIR/plugins}}"
export MISE_SHIMS_DIR="${{MISE_SHIMS_DIR:-$MISE_DATA_DIR/shims}}"
export MISE_SHARED_INSTALL_DIRS="${{MISE_SHARED_INSTALL_DIRS:-$fixture/mise-shared-installs}}"
export MISE_SYSTEM_DIR="${{MISE_SYSTEM_DIR:-$fixture/mise-system}}"
export MISE_SYSTEM_CONFIG_DIR="${{MISE_SYSTEM_CONFIG_DIR:-$MISE_SYSTEM_DIR/config}}"
export MISE_SYSTEM_CONFIG_FILE="${{MISE_SYSTEM_CONFIG_FILE:-$MISE_SYSTEM_CONFIG_DIR/config.toml}}"
export MISE_SYSTEM_DATA_DIR="${{MISE_SYSTEM_DATA_DIR:-$MISE_SYSTEM_DIR/data}}"
export MISE_SYSTEM_INSTALLS_DIR="${{MISE_SYSTEM_INSTALLS_DIR:-$MISE_SYSTEM_DATA_DIR/installs}}"
export MISE_SYSTEM_SHIMS_DIR="${{MISE_SYSTEM_SHIMS_DIR:-$MISE_SYSTEM_DATA_DIR/shims}}"
export MISE_SYSTEM_PLUGINS_DIRS="${{MISE_SYSTEM_PLUGINS_DIRS:-$MISE_SYSTEM_DATA_DIR/plugins}}"
export MISE_CEILING_PATHS="${{MISE_CEILING_PATHS:-$fixture}}"
export MISE_OFFLINE=1 MISE_AUTO_INSTALL=0 MISE_EXEC_AUTO_INSTALL=0
export MISE_NOT_FOUND_AUTO_INSTALL=0 MISE_AUTO_UPDATE=0
exec {boundary} mise {executable} "$@"
"#
            ),
        )
        .unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();
        adapter
    }

    pub fn bare_remote(&self, name: &str) -> PathBuf {
        let mut components = Path::new(name).components();
        assert!(matches!(components.next(), Some(Component::Normal(_))));
        assert!(
            components.next().is_none(),
            "bare remote name must be a single component"
        );
        let remote = self.private.join(name);
        let output = self
            .git()
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&remote)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        remote
    }

    fn apply_private_paths(&self, command: &mut FixtureCommand<'_>) {
        let home = self.home();
        let private = &self.private;
        command
            .env("CODEX_HOME", home.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("XDG_RUNTIME_DIR", private.join("runtime"))
            .env("XDG_CONFIG_DIRS", private.join("xdg-config"))
            .env("XDG_DATA_DIRS", private.join("xdg-data"))
            .env("TMPDIR", private.join("tmp"))
            .env("MISE_CONFIG_DIR", self.root())
            .env("MISE_DATA_DIR", private.join("mise-data"))
            .env("MISE_STATE_DIR", private.join("mise-state"))
            .env("MISE_CACHE_DIR", private.join("mise-cache"))
            .env("MISE_TMP_DIR", private.join("tmp"))
            .env("MISE_SYSTEM_CONFIG_DIR", private.join("mise-system"))
            .env(
                "MISE_SYSTEM_CONFIG_FILE",
                private.join("mise-system/config.toml"),
            )
            .env("MISE_INSTALLS_DIR", private.join("mise-installs"))
            .env("MISE_INSTALL_STORE_DIR", private.join("mise-store"))
            .env("MISE_CEILING_PATHS", self.root().parent().unwrap())
            .env("MISE_OFFLINE", "1")
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_EXEC_AUTO_INSTALL", "0")
            .env("MISE_NOT_FOUND_AUTO_INSTALL", "0")
            .env("MISE_AUTO_UPDATE", "0")
            .env("GIT_CONFIG_SYSTEM", private.join("system.gitconfig"))
            .env("GIT_TEMPLATE_DIR", private.join("git-template"))
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0");
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

pub fn executable_on_path(command: &Command, name: &str) -> Option<PathBuf> {
    let path = command
        .get_envs()
        .find_map(|(key, value)| (key == "PATH").then_some(value).flatten())?;
    env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .filter(|directory| {
            !directory
                .components()
                .any(|part| part.as_os_str() == "shims")
        })
        .map(|directory| directory.join(name))
        .find(|path| {
            fs::metadata(path).is_ok_and(|info| info.is_file() && info.mode() & 0o111 != 0)
        })
}
