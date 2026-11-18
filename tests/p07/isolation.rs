//! Additional private paths for P-07's real mise, Git and JJ subprocesses.

use crate::support::World;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[allow(dead_code)] // Only suites reading reported recovery paths need this.
pub fn fixture_file(base: &Path, candidate: &Path) -> Option<PathBuf> {
    let relative = candidate.strip_prefix(base).ok()?;
    let mut path = base.to_path_buf();
    let mut entries = relative.components().peekable();
    while let Some(Component::Normal(name)) = entries.next() {
        path.push(name);
        let kind = fs::symlink_metadata(&path).ok()?.file_type();
        if entries.peek().is_none() {
            return kind.is_file().then_some(path);
        }
        if !kind.is_dir() {
            return None;
        }
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
        fs::write(private.join("mise-system/config.toml"), "").unwrap();
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

    pub fn dots(&self) -> Command {
        let mut command = self.world.dots();
        self.apply_private_paths(&mut command);
        command
    }

    pub fn git(&self) -> Command {
        self.tool(&self.git)
    }

    pub fn tool(&self, program: impl AsRef<OsStr>) -> Command {
        let isolated = self.dots();
        let mut command = Command::new(program);
        command
            .env_clear()
            .envs(
                isolated
                    .get_envs()
                    .filter_map(|(name, value)| value.map(|v| (name, v))),
            )
            .current_dir(self.root());
        command
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

    fn apply_private_paths(&self, command: &mut Command) {
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
        .find(|path| path.is_file())
}
