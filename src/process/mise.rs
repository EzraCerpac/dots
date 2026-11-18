use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use super::{Cwd, HOOK_ENV, is_executable, stage_argv};
use crate::config::selection::{self, Selection};
use crate::{Ctx, Env, Fail};

pub struct Mise<'a> {
    ctx: &'a Ctx,
    binary: PathBuf,
    selected: Selection,
}

impl<'a> Mise<'a> {
    pub fn new(ctx: &'a Ctx) -> Result<Self, Fail> {
        let selected = selection::read(&ctx.root, &ctx.env)?;
        Self::with_selection(ctx, selected)
    }

    pub fn with_selection(ctx: &'a Ctx, selected: Selection) -> Result<Self, Fail> {
        let binary = mise_binary(&ctx.env)?;
        Ok(Self {
            ctx,
            binary,
            selected,
        })
    }

    pub fn command<I, S>(&self, args: I) -> Result<Command, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<S> = args.into_iter().collect();
        let mut command = self.raw_command()?;
        #[cfg(target_os = "macos")]
        if matches!(args.as_slice(), [command, sub, ..] if command.as_ref() == "bootstrap"
            && matches!(sub.as_ref().to_str(), Some("status" | "packages")))
        {
            self.expose_ruby(&mut command)?;
        }
        command.args(args);
        Ok(command)
    }

    pub fn json<I, S>(&self, args: I) -> Result<Value, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self
            .command(args)?
            .env("MISE_ENV", self.selected.active_environments())
            .output()
            .map_err(|error| Fail::new(1, format!("could not run mise: {error}")))?;
        if !output.status.success() {
            return Err(Fail::new(
                1,
                format!("mise query failed ({})", output.status),
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| Fail::new(1, format!("mise returned invalid JSON: {error}")))
    }

    pub fn exec<I, S>(&self, program: impl AsRef<OsStr>, args: I) -> Result<Command, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.exec_with_env(program, args, |_| Ok(()))
    }

    /// Configure a dots-owned child after mise applies its environment. The
    /// callback may also attach an inherited descriptor to the launcher.
    pub fn exec_with_env<I, S>(
        &self,
        program: impl AsRef<OsStr>,
        args: I,
        configure: impl FnOnce(&mut Command) -> Result<(), Fail>,
    ) -> Result<Command, Fail>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.raw_command()?;
        configure(&mut command)?;
        command
            .env("DOTS_ROOT", self.ctx.root.path())
            .env(
                "DOTS_PROFILE",
                self.selected.profile.as_deref().unwrap_or_default(),
            )
            .env_remove("DOTS_HOME")
            .env_remove("DOTS_STAGES");
        let bin = self
            .ctx
            .root
            .child_bin
            .get()
            .and_then(|bin| bin.as_ref().ok())
            .expect("prepared child PATH")
            .path();
        let filename = stage_argv(bin, &command, program.as_ref(), args)?;
        for key in HOOK_ENV {
            command.env_remove(key);
        }
        command
            .env("MISE_ENV", self.selected.active_environments())
            .args(["exec", "--"])
            .arg(bin.join("dots"))
            .arg("__exec")
            .arg(filename);
        Ok(command)
    }

    fn raw_command(&self) -> Result<Command, Fail> {
        let profile = self.selected.profile.as_deref().unwrap_or_default();
        let mut command =
            self.ctx
                .command_with_profile(&self.binary, Cwd::Root, profile.as_ref())?;
        command
            .env("MISE_CONFIG_DIR", self.ctx.root.path())
            .env("MISE_ENV", self.selected.environments())
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_EXEC_AUTO_INSTALL", "0");
        Ok(command)
    }

    #[cfg(target_os = "macos")]
    fn expose_ruby(&self, command: &mut Command) -> Result<(), Fail> {
        use std::os::unix::ffi::OsStrExt;

        let Ok(output) = self.raw_command()?.args(["which", "ruby"]).output() else {
            return Ok(());
        };
        let ruby = PathBuf::from(OsStr::from_bytes(
            output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout),
        ));
        if !output.status.success() || !ruby.is_absolute() || !is_executable(&ruby) {
            return Ok(());
        }
        let Some(bin) = ruby.parent() else {
            return Ok(());
        };
        let original = command
            .get_envs()
            .find_map(|(key, value)| (key == "PATH").then_some(value).flatten());
        let mut paths = original.into_iter().flat_map(std::env::split_paths);
        let result = std::env::join_paths(
            paths
                .next()
                .into_iter()
                .chain([bin.to_path_buf()])
                .chain(paths),
        )
        .map_err(|error| Fail::new(1, format!("could not expose mise Ruby on PATH: {error}")))?;
        command.env("PATH", result);
        Ok(())
    }
}

pub(crate) fn mise_binary(env: &Env) -> Result<PathBuf, Fail> {
    if let Some(path) = env.get("DOTS_MISE_BIN") {
        let binary = env.cwd().join(path);
        if is_executable(&binary) {
            return Ok(binary);
        }
        return Err(Fail::usage(format!(
            "DOTS_MISE_BIN is not executable: {}",
            Path::new(path).display()
        )));
    }
    if let Some(home) = env.home() {
        let binary = env.cwd().join(home).join(".local/bin/mise");
        if is_executable(&binary) {
            return Ok(binary);
        }
    }
    env.get("PATH")
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|path| env.cwd().join(path).join("mise"))
        .find(|path| is_executable(path))
        .ok_or_else(|| {
            Fail::usage("mise not found; install it from https://mise.jdx.dev or set DOTS_MISE_BIN")
        })
}

pub(crate) fn quiet_probe(binary: &Path, root: &Path, scratch: &Path, env: &Env) -> Command {
    let mut command = Command::new(binary);
    command
        .arg("--quiet")
        .arg("-C")
        .arg(root)
        .current_dir(root)
        .env_clear()
        .envs(&env.vars)
        .env_remove("MISE_ENV")
        .env_remove("MISE_LOG_FILE")
        .env("MISE_CONFIG_DIR", root)
        .env("MISE_CACHE_DIR", scratch.join("cache"))
        .env("MISE_STATE_DIR", scratch.join("state"))
        .env("MISE_TMP_DIR", scratch.join("tmp"))
        .env("MISE_OFFLINE", "1")
        .env("MISE_SAFE", "1")
        .env("MISE_NO_ENV", "1")
        .env("MISE_NO_HOOKS", "1")
        .env("MISE_AUTO_INSTALL", "0")
        .env("MISE_EXEC_AUTO_INSTALL", "0")
        .env("MISE_AUTO_UPDATE", "0")
        .env("MISE_HISTORY_ENABLED", "0");
    command
}

pub(crate) fn config_paths(mut command: Command, limit: Duration) -> Result<Vec<PathBuf>, Fail> {
    const MAX_OUTPUT: u64 = 2 << 20;

    let failure = |error: &dyn std::fmt::Display| {
        Fail::usage(format!("could not list active mise configuration: {error}"))
    };
    let mut output = tempfile::tempfile().map_err(|e| failure(&e))?;
    let stdout = output.try_clone().map_err(|e| failure(&e))?;
    let mut child = command
        .args(["config", "ls", "--json"])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| failure(&e))?;
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return match result {
                    Ok(_) => Err(failure(&"mise timed out")),
                    Err(error) => Err(failure(&error)),
                };
            }
        }
    };
    if !status.success() {
        return Err(failure(&status));
    }
    output.rewind().map_err(|e| failure(&e))?;
    let mut bytes = Vec::new();
    output
        .take(MAX_OUTPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| failure(&e))?;
    if bytes.len() as u64 > MAX_OUTPUT {
        return Err(failure(&"mise returned too many paths"));
    }
    #[derive(Deserialize)]
    struct Entry {
        path: PathBuf,
    }
    let list: Vec<Entry> = serde_json::from_slice(&bytes).map_err(|e| failure(&e))?;
    Ok(list.into_iter().map(|entry| entry.path).collect())
}

pub(crate) fn validate_selection(
    env: &Env,
    early: &File,
    local: &File,
    profile: &str,
    host: &str,
) -> Result<(), Fail> {
    let mise = mise_binary(env)?;
    let scratch = tempfile::tempdir()
        .map_err(|error| Fail::new(1, format!("could not stage mise validation: {error}")))?;
    let scratch_path = scratch.path().canonicalize().map_err(|error| {
        Fail::new(
            1,
            format!("could not locate mise validation directory: {error}"),
        )
    })?;
    let root = scratch_path.join("config");
    let profile_path = root.join(format!("config.{profile}.toml"));
    let host_path = root.join(format!("config.{host}.toml"));
    let local_path = root.join("config.local.toml");
    let early_path = root.join("miserc.toml");
    let stage = || -> std::io::Result<()> {
        fs::create_dir(&root)?;
        fs::write(&profile_path, "")?;
        fs::write(&host_path, "")?;
        for (source, target) in [(early, &early_path), (local, &local_path)] {
            let mut source = source.try_clone()?;
            source.rewind()?;
            std::io::copy(&mut source, &mut File::create(target)?)?;
        }
        Ok(())
    };
    stage().map_err(|error| Fail::new(1, format!("could not stage mise validation: {error}")))?;
    let mut command = quiet_probe(&mise, &root, &scratch_path, env);
    let keys: Vec<OsString> = env
        .vars
        .keys()
        .filter(|key| {
            key.as_encoded_bytes().starts_with(b"MISE_")
                || key.as_encoded_bytes().starts_with(b"__MISE_")
        })
        .cloned()
        .collect();
    let overrides = command
        .get_envs()
        .map(|(key, _)| key.to_os_string())
        .collect::<Vec<_>>();
    for key in keys {
        if !overrides.contains(&key) {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", &scratch_path)
        .env("XDG_CONFIG_HOME", scratch_path.join("xdg-config"))
        .env("XDG_DATA_HOME", scratch_path.join("xdg-data"))
        .env("XDG_STATE_HOME", scratch_path.join("xdg-state"))
        .env("XDG_CACHE_HOME", scratch_path.join("xdg-cache"))
        .env("MISE_SYSTEM_CONFIG_DIR", scratch_path.join("system"))
        .env("MISE_DATA_DIR", scratch_path.join("data"))
        .env("MISE_CEILING_PATHS", &scratch_path)
        .env("MISE_TRUSTED_CONFIG_PATHS", &root);
    let active = config_paths(command, Duration::from_secs(10)).map_err(|error| {
        Fail::usage(format!(
            "mise rejected staged miserc.toml or config.local.toml: {error}"
        ))
    })?;
    if ![&profile_path, &host_path, &local_path]
        .iter()
        .all(|path| active.contains(path))
    {
        return Err(Fail::usage(
            "mise did not load the staged profile, machine ID and config.local.toml",
        ));
    }
    Ok(())
}
