use std::ffi::OsStr;
use std::io;
use std::ops::Deref;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};
use std::process::{Child, Command, ExitStatus, Output, Stdio};

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
    ("MISE_SYSTEM_PLUGINS_DIRS", "mise-system/data/plugins"),
];

const OPTIONAL_PATHS: &[&str] = &[
    "DOTS_ROOT",
    "DOTS_HOME",
    "DOTS_STAGES",
    "DOTS_CHANGES",
    "DOTS_UP_STATE_DIR",
    "SETUP_CONFIG_ROOT",
    "XDG_RUNTIME_DIR",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "JJ_CONFIG",
    "CARGO_HOME",
    "MISE_SYSTEM_DIR",
    "MISE_CONFIG_FILE",
    "MISE_GLOBAL_CONFIG_FILE",
    "MISE_GLOBAL_CONFIG_ROOT",
    "MISE_SYSTEM_CONFIG_FILE",
    "MISE_LOG_FILE",
    "MISE_SKILLS_DIR",
    "BASH_ENV",
    "ENV",
];

const PATH_LISTS: &[&str] = &[
    "GIT_CEILING_DIRECTORIES",
    "MISE_CEILING_PATHS",
    "MISE_TRUSTED_CONFIG_PATHS",
    "MISE_SHARED_INSTALL_DIRS",
];

/// Uses the native command for arguments, environment, and stdio. Only
/// inspection is exposed through Deref so callers cannot skip launch checks.
pub struct FixtureCommand<'a> {
    inner: Command,
    base: &'a Path,
    allow_missing_home: bool,
    allow_config_fallback: bool,
}

impl<'a> FixtureCommand<'a> {
    pub(super) fn new(inner: Command, base: &'a Path) -> Self {
        Self {
            inner,
            base,
            allow_missing_home: false,
            allow_config_fallback: false,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    pub fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> &mut Self {
        self.inner.args(args);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env(key, value);
        self
    }

    pub fn envs<K, V>(&mut self, vars: impl IntoIterator<Item = (K, V)>) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.inner.env_clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stdin(stdio);
        self
    }

    pub fn stdout(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stdout(stdio);
        self
    }

    pub fn stderr(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stderr(stdio);
        self
    }

    /// Install a callback, for example to attach a PTY.
    ///
    /// # Safety
    /// The callback must follow CommandExt::pre_exec's fork-safety rules
    /// and preserve the fixture environment and working directory.
    pub unsafe fn pre_exec(
        &mut self,
        callback: impl FnMut() -> io::Result<()> + Send + Sync + 'static,
    ) -> &mut Self {
        unsafe { self.inner.pre_exec(callback) };
        self
    }

    /// For Rust CLI tests whose subject is an unset HOME. All other paths
    /// stay explicit, and the executable must not fall back to the OS home.
    pub fn without_home(&mut self) -> &mut Self {
        self.allow_missing_home = true;
        self.env_remove("HOME")
    }

    /// Exercise config lookup with explicit overrides removed. The guard
    /// checks both XDG and HOME fallback paths before each launch.
    pub fn allow_config_fallback(&mut self) -> &mut Self {
        self.allow_config_fallback = true;
        self
    }

    pub fn output(&mut self) -> io::Result<Output> {
        self.check()?;
        self.inner.output()
    }

    pub fn status(&mut self) -> io::Result<ExitStatus> {
        self.check()?;
        self.inner.status()
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        self.check()?;
        self.inner.spawn()
    }

    fn check(&self) -> io::Result<()> {
        if self.base.canonicalize()? != self.base {
            return Err(outside("fixture root"));
        }
        let cwd = self.inner.get_current_dir().ok_or_else(|| outside("cwd"))?;
        self.check_path("cwd", cwd)?;
        let value = |name: &str| {
            self.inner
                .get_envs()
                .find_map(|(key, value)| (key == name).then_some(value))
                .flatten()
        };
        for name in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "MISE_CONFIG_DIR",
        ]
        .into_iter()
        .chain(PRIVATE_DIRS.iter().map(|&(name, _)| name))
        {
            let Some(path) = value(name) else {
                if (name == "HOME" && self.allow_missing_home)
                    || (matches!(name, "MISE_CONFIG_DIR" | "XDG_CONFIG_HOME")
                        && self.allow_config_fallback
                        && value("HOME").is_some())
                {
                    continue;
                }
                return Err(outside(name));
            };
            if name == "MISE_SYSTEM_PLUGINS_DIRS" {
                for path in std::env::split_paths(path).filter(|path| !path.as_os_str().is_empty())
                {
                    if !path.is_absolute() {
                        return Err(outside(name));
                    }
                    self.check_path(name, &path)?;
                }
                continue;
            }
            if path.is_empty() || (name != "MISE_CONFIG_DIR" && !Path::new(path).is_absolute()) {
                return Err(outside(name));
            }
            self.check_path(name, &cwd.join(path))?;
        }
        for &name in OPTIONAL_PATHS {
            if let Some(path) = value(name).filter(|path| !path.is_empty()) {
                if name == "DOTS_UP_STATE_DIR" && !Path::new(path).is_absolute() {
                    return Err(outside(name));
                }
                self.check_path(name, &cwd.join(path))?;
            }
        }
        for &name in PATH_LISTS {
            if let Some(paths) = value(name).filter(|paths| !paths.is_empty()) {
                for path in std::env::split_paths(paths) {
                    self.check_path(name, &cwd.join(path))?;
                }
            }
        }
        if let Some(home) = value("HOME") {
            let home = Path::new(home);
            self.check_path("HOME/.agents", &home.join(".agents"))?;
            if self.allow_config_fallback {
                let config = value("XDG_CONFIG_HOME").map(Path::new);
                let config = config.map_or_else(|| home.join(".config"), Path::to_path_buf);
                self.check_path("mise config fallback", &config.join("mise"))?;
            }
        }
        let state = Path::new(value("XDG_STATE_HOME").unwrap()).join("dots");
        self.check_path("XDG_STATE_HOME/dots", &state)?;
        if value("DOTS_SKILL_LINKS") != Some(OsStr::new("0")) {
            for file in [
                "skill-links",
                "skill-links.tmp",
                "skill-notices",
                "skills/dots/SKILL.md",
            ] {
                self.check_path(file, &state.join(file))?;
            }
        }
        Ok(())
    }

    fn check_path(&self, name: &str, path: &Path) -> io::Result<()> {
        let relative = path.strip_prefix(self.base).map_err(|_| outside(name))?;
        let mut physical = self.base.to_path_buf();
        for part in relative.components() {
            match part {
                Component::Normal(part) => {
                    physical.push(part);
                    match physical.canonicalize() {
                        Ok(resolved) => physical = resolved,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            // Missing directories are allowed, dangling links are not.
                            match physical.symlink_metadata() {
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                _ => return Err(outside(name)),
                            }
                        }
                        // Some tests deliberately put a file where a
                        // directory is expected. Its descendants are inaccessible.
                        Err(error) if error.kind() == io::ErrorKind::NotADirectory => {}
                        Err(_) => return Err(outside(name)),
                    }
                }
                Component::ParentDir => {
                    physical.pop();
                }
                Component::CurDir => {}
                _ => return Err(outside(name)),
            }
            if !physical.starts_with(self.base) {
                return Err(outside(name));
            }
        }
        Ok(())
    }
}

impl Deref for FixtureCommand<'_> {
    type Target = Command;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

fn outside(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{name} must resolve inside the test fixture"),
    )
}
