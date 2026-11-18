use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix::libc;

use crate::config::{RepoSetting, Vars, selection::Selection};
use crate::process::{Cwd, Mise};
use crate::run::Run;
use crate::{Ctx, Fail};

use super::{blocked, invalid};

pub(super) struct Native<'a, 'ctx> {
    pub ctx: &'ctx Ctx,
    pub run: &'a Run<'ctx>,
    selection: Option<Selection>,
}

impl<'a, 'ctx> Native<'a, 'ctx> {
    pub fn new(run: &'a Run<'ctx>, ctx: &'ctx Ctx) -> Self {
        Self {
            ctx,
            run,
            selection: None,
        }
    }

    pub fn select(&mut self, selection: Selection) {
        self.selection = Some(selection);
    }

    pub fn mise(&self, args: &[&str]) -> Result<Command, Fail> {
        let mise = match &self.selection {
            Some(selected) => Mise::with_selection(
                self.ctx,
                Selection {
                    profile: selected.profile.clone(),
                    machine_id: selected.machine_id.clone(),
                    selectors: selected.selectors.clone(),
                },
            )?,
            None => Mise::new(self.ctx)?,
        };
        let mut cmd = mise.command(args)?;
        cmd.env("MISE_AUTO_INSTALL", "0")
            .env("MISE_EXEC_AUTO_INSTALL", "0");
        clean_git_env(&mut cmd);
        Ok(cmd)
    }

    pub fn git(&self, store: Option<&Path>) -> Result<Command, Fail> {
        let mut cmd = self.command("git")?;
        clean_git_env(&mut cmd);
        if let Some(store) = store {
            cmd.arg("--git-dir").arg(store);
        }
        Ok(cmd)
    }

    pub fn command(&self, program: &str) -> Result<Command, Fail> {
        let mut cmd = self.ctx.command(program, Cwd::Root)?;
        cmd.env("LC_ALL", "C");
        Ok(cmd)
    }

    pub fn probe(&self, cmd: Command, diagnostic: &str) -> Result<Output, Fail> {
        if self.run.cancelled() {
            return Err(blocked("history operation was cancelled"));
        }
        self.run.probe(cmd).map_err(|_| blocked(diagnostic))
    }

    pub fn output(&self, cmd: Command, diagnostic: &str) -> Result<Vec<u8>, Fail> {
        let output = self.probe(cmd, diagnostic)?;
        if !output.status.success() {
            return Err(blocked(diagnostic));
        }
        Ok(output.stdout)
    }

    pub fn text(&self, cmd: Command, diagnostic: &str) -> Result<String, Fail> {
        String::from_utf8(self.output(cmd, diagnostic)?).map_err(|_| blocked(diagnostic))
    }

    pub fn home(&self) -> Result<&Path, Fail> {
        self.ctx
            .env
            .home()
            .filter(|home| home.is_absolute())
            .ok_or_else(|| invalid("HOME must name an absolute directory"))
    }

    pub fn store(&self) -> Result<PathBuf, Fail> {
        let env = &self.ctx.env;
        let home = self.home()?;
        let state = match env.get("MISE_STATE_DIR").filter(|path| !path.is_empty()) {
            Some(path) => expand(home, Path::new(path))?,
            None => env
                .get("XDG_STATE_HOME")
                .filter(|path| !path.is_empty())
                .map_or_else(|| home.join(".local/state"), PathBuf::from)
                .join("mise"),
        };
        if !state.is_absolute() {
            return Err(invalid("the mise state directory must be absolute"));
        }
        Ok(state.join("history/repo.git"))
    }

    pub fn local_origin(&self, store: &Path) -> Result<Option<String>, Fail> {
        let mut cmd = self.git(Some(store))?;
        cmd.args(["config", "--local", "--get-all", "remote.origin.url"]);
        optional_config(self.probe(cmd, "could not read the history store origin")?)
    }

    pub fn effective_origin(&self) -> Result<(String, String), Fail> {
        let cmd = self.mise(&["bootstrap", "dotfiles", "origin"])?;
        let text = self
            .text(cmd, "could not read the effective mise history origin")
            .map_err(|_| invalid("could not read the effective mise history origin"))?;
        let (url, tail) = text
            .trim_end()
            .split_once(" (branch ")
            .ok_or_else(|| invalid("mise has no effective history origin"))?;
        let (branch, _) = tail
            .split_once(") declared in ")
            .ok_or_else(|| invalid("could not read the effective mise history branch"))?;
        if url.is_empty() || branch.is_empty() || text.lines().count() != 1 {
            return Err(invalid(
                "could not read the effective mise history origin and branch",
            ));
        }
        Ok((url.to_owned(), branch.to_owned()))
    }

    pub fn bare(&self, store: &Path) -> Result<(), Fail> {
        if !fs::symlink_metadata(store).is_ok_and(|meta| meta.is_dir()) {
            return Err(invalid("the local history store must be a real directory"));
        }
        let mut cmd = self.git(Some(store))?;
        cmd.args(["rev-parse", "--is-bare-repository"]);
        if self
            .text(cmd, "could not inspect the local history store")?
            .trim_end()
            != "true"
        {
            return Err(invalid(
                "the local history store must be a bare Git repository",
            ));
        }
        Ok(())
    }

    /// Resolve the URL as an explicit operand (mise does not push to the
    /// configured remote name) and inspect the store's named remote as well.
    pub fn destinations(
        &self,
        store: Option<&Path>,
        raw: &str,
        expected: &RepoSetting,
        vars: &Vars,
    ) -> Result<(), Fail> {
        let invalid_destination = || {
            invalid(
                "effective history destinations must match private dots_state_repo and differ from the source repository",
            )
        };
        let source = self.source(vars)?;
        let check = |url: &str| {
            if RepoSetting::parse(url).is_err()
                || !expected.matches(url)
                || source.as_ref().is_some_and(|repo| repo.matches(url))
            {
                return Err(invalid_destination());
            }
            Ok(())
        };
        check(raw)?;

        // The bare directory may not exist until the first native save.
        // Resolve global rewrites in a temporary bare repo in that case;
        // publication rechecks in the actual newly created repository.
        let temporary = if store.is_none() {
            let temp = tempfile::tempdir()
                .map_err(|_| blocked("could not stage history URL validation"))?;
            let mut cmd = self.git(None)?;
            cmd.args(["init", "--bare"]).arg(temp.path());
            self.output(cmd, "could not stage history URL validation")?;
            Some(temp)
        } else {
            None
        };
        let resolver = store.or_else(|| temporary.as_ref().map(|temp| temp.path()));
        let mut cmd = self.git(resolver)?;
        cmd.args(["remote", "show", "-n", "--", raw]);
        let output = self
            .text(cmd, "could not resolve effective history URLs")
            .map_err(|_| invalid_destination())?;
        let mut fetch = 0;
        let mut push = 0;
        for line in output.lines() {
            if let Some(url) = line.strip_prefix("  Fetch URL: ") {
                check(url)?;
                fetch += 1;
            }
            if let Some(url) = line.strip_prefix("  Push  URL: ") {
                check(url)?;
                push += 1;
            }
        }
        if fetch != 1 || push == 0 {
            return Err(invalid_destination());
        }
        if let Some(store) = store
            && let Some(configured) = self.local_origin(store)?
        {
            check(&configured)?;
            for arg in [None, Some("--push")] {
                let mut cmd = self.git(Some(store))?;
                cmd.args(["remote", "get-url", "--all"])
                    .args(arg)
                    .arg("origin");
                let output = self
                    .text(cmd, "could not resolve effective history URLs")
                    .map_err(|_| invalid_destination())?;
                if output.is_empty() {
                    return Err(invalid_destination());
                }
                for url in output.lines() {
                    check(url)?;
                }
            }
        }
        Ok(())
    }

    fn source(&self, vars: &Vars) -> Result<Option<RepoSetting>, Fail> {
        if let Some(source) = &vars.source_repo {
            return RepoSetting::parse(&source.transport_url()).map(Some);
        }
        let mut cmd = self.git(None)?;
        cmd.args(["config", "--get-all", "remote.origin.url"]);
        let output = self.probe(cmd, "could not inspect the source origin")?;
        optional_config(output)?
            .map(|url| {
                RepoSetting::parse(&url)
                    .map_err(|_| invalid("could not validate the configured source origin"))
            })
            .transpose()
    }
}

fn optional_config(output: Output) -> Result<Option<String>, Fail> {
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok(None);
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| invalid("could not read the repository origin"))?;
    let mut lines = text.lines();
    match (output.status.success(), lines.next(), lines.next()) {
        (true, Some(url), None) if !url.is_empty() => Ok(Some(url.to_owned())),
        _ => Err(invalid("could not identify a single repository origin")),
    }
}

pub(super) fn expand(home: &Path, path: &Path) -> Result<PathBuf, Fail> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else if let Ok(suffix) = path.strip_prefix("~")
        && !suffix.as_os_str().is_empty()
    {
        Ok(home.join(suffix))
    } else {
        Err(invalid("expected an absolute path or one starting with ~/"))
    }
}

pub(super) fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).recursive(true).create(path)
}

pub(super) fn exists(path: &Path) -> Result<bool, Fail> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(blocked("could not inspect local history state")),
    }
}

/// Keep the inode in place: unlinking a flock file would allow another writer
/// to enter on a different inode while an existing holder is still active.
pub(super) fn lock(store: &Path) -> Result<File, Fail> {
    let directory = store.parent().expect("history store has a parent");
    private_dir(directory).map_err(|_| blocked("could not create the local history directory"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(".dots-history.lock"))
        .map_err(|_| blocked("could not open the history operation lock"))?;
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return Err(blocked("history operation lock is not a regular file"));
    }
    // SAFETY: the open file owns the descriptor for the duration of the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(blocked("another settings history operation is active"));
    }
    Ok(file)
}

fn clean_git_env(cmd: &mut Command) {
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(name);
    }
}
