// Each integration suite imports only the helpers it needs.
#![allow(dead_code)]

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

// One table sets the defaults and identifies the variables that must stay set.
pub const PRIVATE_DIRS: &[(&str, &str)] = &[
    ("TMPDIR", "tmp"),
    ("CODEX_HOME", "home/.codex"),
    ("CLAUDE_CONFIG_DIR", "home/.claude"),
    ("MISE_SYSTEM_CONFIG_DIR", "mise-system/config"),
    ("MISE_DATA_DIR", "mise/data"),
    ("MISE_CACHE_DIR", "mise/cache"),
    ("MISE_STATE_DIR", "mise/state"),
    ("MISE_TMP_DIR", "mise/tmp"),
    ("MISE_INSTALLS_DIR", "mise/data/installs"),
    ("MISE_INSTALL_STORE_DIR", "mise/data/store"),
    ("MISE_DOWNLOADS_DIR", "mise/data/downloads"),
    ("MISE_PLUGINS_DIR", "mise/data/plugins"),
    ("MISE_SHIMS_DIR", "mise/data/shims"),
    ("MISE_SYSTEM_DATA_DIR", "mise-system/data"),
    ("MISE_SYSTEM_INSTALLS_DIR", "mise-system/data/installs"),
    ("MISE_SYSTEM_SHIMS_DIR", "mise-system/data/shims"),
];

pub struct World {
    _temp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    root: PathBuf,
    dots_bin: PathBuf,
}

impl World {
    pub fn new() -> Self {
        Self::with_dots(
            env::var_os("DOTS_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_dots").into()),
        )
    }

    pub fn with_dots(dots_bin: impl AsRef<OsStr>) -> Self {
        let dots_bin = Path::new(&dots_bin);
        assert!(dots_bin.is_absolute(), "DOTS_BIN must be an absolute path");
        let dots_bin = dots_bin
            .canonicalize()
            .unwrap_or_else(|error| panic!("cannot resolve {}: {error}", dots_bin.display()));

        let temp = tempfile::Builder::new()
            .prefix("dots-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let base = temp.path().canonicalize().unwrap();
        let home = base.join("home");
        let root = base.join("config");
        fs::create_dir(base.join("tmp")).unwrap();
        for directory in [".config", ".cache", ".local/share", ".local/state"] {
            fs::create_dir_all(home.join(directory)).unwrap();
        }
        copy_fixture(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/config")
                .as_path(),
            &root,
        );
        fs::write(
            home.join(".gitconfig"),
            "[user]\nname = Dots Tests\nemail = dots-tests@example.invalid\n",
        )
        .unwrap();
        fs::write(
            home.join(".config/jj.toml"),
            "[user]\nname = 'Dots Tests'\nemail = 'dots-tests@example.invalid'\n",
        )
        .unwrap();
        Self {
            _temp: temp,
            base,
            home,
            root,
            dots_bin,
        }
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Builds an unstarted command. Keep this world alive until the child exits.
    pub fn dots(&self) -> Command {
        self.command(&self.dots_bin)
    }

    pub fn git(&self) -> Command {
        self.command(Path::new("git"))
    }

    pub fn bare_remote(&self, name: &str) -> PathBuf {
        let mut components = Path::new(name).components();
        let Some(Component::Normal(name)) = components.next() else {
            panic!("bare remote name must be a single path component");
        };
        assert!(
            components.next().is_none(),
            "bare remote name must be a single path component"
        );
        let remotes = self.root.with_file_name("remotes");
        fs::create_dir_all(&remotes).unwrap();
        let remote = remotes.join(name);
        fs::create_dir(&remote).unwrap();
        let output = self
            .git()
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&remote)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        remote
    }

    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        // Even when TMPDIR is inside a checkout, Git must stay inside this world.
        let git_ceiling =
            env::join_paths([&self.base]).expect("world path must be a valid Git ceiling");
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env_clear()
            .env(
                "PATH",
                env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
            )
            .env("HOME", &self.home)
            .env("DOTS_ROOT", &self.root)
            .env("MISE_CONFIG_DIR", self.home.join(".config/mise"))
            .env("MISE_CEILING_PATHS", &self.base)
            .env("DOTS_SKILL_LINKS", "0")
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("GIT_CONFIG_GLOBAL", self.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", git_ceiling)
            .env("JJ_CONFIG", self.home.join(".config/jj.toml"));
        for &(key, relative) in PRIVATE_DIRS {
            command.env(key, self.base.join(relative));
        }
        for (key, value) in env::vars_os() {
            let Some(name) = key.to_str() else { continue };
            if matches!(name, "LANG" | "LANGUAGE" | "TERM") || name.starts_with("LC_") {
                command.env(key, value);
            }
        }
        command
    }
}

fn copy_fixture(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_fixture(&entry.path(), &target);
        } else {
            assert!(kind.is_file(), "unsupported fixture: {:?}", entry.path());
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
