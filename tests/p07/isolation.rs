//! Additional private paths for P-07's real mise, Git and JJ subprocesses.

use crate::support::{FixtureCommand, World};
use nix::libc;
use std::env;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
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
    const DIRECTORY: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY;
    const NO_FOLLOW: libc::c_int = libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let relative = candidate.strip_prefix(base).ok()?;
    let mut dir = OpenOptions::new()
        .read(true)
        .custom_flags(DIRECTORY | NO_FOLLOW)
        .open(base)
        .ok()?;
    let owner = dir.metadata().ok()?.uid();
    let mut entries = relative.components().peekable();
    while let Some(Component::Normal(name)) = entries.next() {
        let final_component = entries.peek().is_none();
        let flags = if final_component {
            access | libc::O_NONBLOCK
        } else {
            DIRECTORY
        };
        let name = CString::new(name.as_bytes()).ok()?;
        // Each component is opened beneath its already-open directory, so a
        // symlink replacement cannot redirect a subsequent open or read.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | NO_FOLLOW) };
        if fd < 0 {
            return None;
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().ok()?;
        if metadata.uid() != owner {
            return None;
        }
        if final_component {
            return metadata
                .is_file()
                .then_some(FixtureFile { file, parent: dir });
        }
        dir = file;
    }
    None
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

    fn boundary(&self) -> String {
        let interpreter = executable_on_path(&self.world.command("python3"), "python3")
            .expect("P-07 fixture boundary requires python3 on PATH");
        let program = self.private.join("native-boundary.py");
        fs::write(&program, include_bytes!("native-boundary.py")).unwrap();
        format!(
            "{} -I -S {} {}",
            quote(&interpreter),
            quote(&program),
            quote(self.world.base())
        )
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
