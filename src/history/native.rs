use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix::libc;
use serde::Deserialize;

use crate::config::{
    RepoSetting, Vars,
    selection::{self, Selection},
};
use crate::process::{Cwd, Mise};
use crate::run::Run;
use crate::{Ctx, Fail};

use super::{blocked, invalid};

pub(super) struct Native<'a, 'ctx> {
    pub ctx: &'ctx Ctx,
    pub run: &'a Run<'ctx>,
    selection: Option<Selection>,
    pub(super) checkpoint: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct PendingOperation {
    pub uuid: String,
    pub local: bool,
}

impl<'a, 'ctx> Native<'a, 'ctx> {
    pub fn new(run: &'a Run<'ctx>, ctx: &'ctx Ctx) -> Self {
        Self {
            ctx,
            run,
            selection: None,
            checkpoint: false,
        }
    }

    pub(super) fn for_checkpoint(run: &'a Run<'ctx>, ctx: &'ctx Ctx) -> Self {
        Self {
            checkpoint: true,
            ..Self::new(run, ctx)
        }
    }

    pub fn select(&mut self, selection: Selection) {
        self.selection = Some(selection);
    }

    pub(super) fn selection(&self) -> Result<Selection, Fail> {
        match &self.selection {
            Some(selected) => Ok(selected.clone()),
            None => selection::read(&self.ctx.root, &self.ctx.env),
        }
    }

    pub fn mise(&self, args: &[&str]) -> Result<Command, Fail> {
        let selected = self.selection()?;
        let environments = selected.active_environments();
        let mise = Mise::with_selection(self.ctx, selected)?;
        let mut cmd = mise.command(args)?;
        // Backup and restore use the managed layers. Update checkpoints must
        // see the same effective overrides for inventory and native save.
        if !self.checkpoint {
            cmd.env_remove("MISE_GLOBAL_CONFIG_FILE")
                .env_remove("MISE_CONFIG_FILE");
        }
        cmd.env_remove("MISE_CD")
            .env_remove("MISE_LOG_FILE")
            .env("MISE_ENV", environments)
            .env("MISE_STATE_DIR", self.state_dir()?);
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

    pub(super) fn state_dir(&self) -> Result<PathBuf, Fail> {
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
        Ok(state)
    }

    pub fn store(&self) -> Result<PathBuf, Fail> {
        Ok(self.state_dir()?.join("history/repo.git"))
    }

    pub fn require_no_pending_operations(&self) -> Result<(), Fail> {
        if !self.pending_operations()?.is_empty() {
            return Err(blocked("mise has an unresolved settings history operation"));
        }
        Ok(())
    }

    /// The native operation lock resumes every pending record. Inspect the
    /// journals without running mise, even before a bare store is installed.
    pub fn pending_operations(&self) -> Result<Vec<PendingOperation>, Fail> {
        let unknown = || invalid("could not validate pending mise history operations");
        let mut operations = Vec::new();
        for (history, local) in self.history_dirs()? {
            let index = history.join("index");
            let pending = index.join("pending");
            if !existing_dir(&index).map_err(|_| unknown())?
                || !existing_dir(&pending).map_err(|_| unknown())?
            {
                continue;
            }
            for entry in fs::read_dir(pending).map_err(|_| unknown())? {
                let path = entry.map_err(|_| unknown())?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    let uuid = pending_uuid(&path).map_err(|_| unknown())?;
                    operations.push(PendingOperation { uuid, local });
                }
            }
        }
        operations.sort_unstable_by(|a, b| (a.local, &a.uuid).cmp(&(b.local, &b.uuid)));
        Ok(operations)
    }

    pub fn history_dirs(&self) -> Result<Vec<(PathBuf, bool)>, Fail> {
        let unknown = || invalid("could not inspect the native mise history stores");
        let state = self.state_dir()?;
        let local = state.join("history-local");
        let mut stores = Vec::new();
        let shared = state.join("history");
        if existing_dir(&shared).map_err(|_| unknown())? {
            stores.push((shared, false));
        }
        let local_history = local.join("history");
        if existing_dir(&local).map_err(|_| unknown())?
            && existing_dir(&local_history).map_err(|_| unknown())?
        {
            stores.push((local_history, true));
        }
        Ok(stores)
    }

    pub fn local_origin(&self, store: &Path) -> Result<Option<String>, Fail> {
        let mut cmd = self.git(Some(store))?;
        cmd.args([
            "config",
            "--null",
            "--local",
            "--get-all",
            "remote.origin.url",
        ]);
        optional_config(self.probe(cmd, "could not read the history store origin")?)
    }

    pub fn effective_origin(&self) -> Result<(String, String), Fail> {
        let cmd = self.mise(&["bootstrap", "dotfiles", "origin"])?;
        let text = self
            .text(cmd, "could not read the effective mise history origin")
            .map_err(|_| invalid("could not read the effective mise history origin"))?;
        let (url, branch) = parse_origin(&text)?;
        Ok((url.to_owned(), branch.to_owned()))
    }

    pub fn require_origin(&self, url: &str, branch: &str) -> Result<(), Fail> {
        let (effective_url, effective_branch) = self.effective_origin()?;
        if effective_url != url || effective_branch != branch {
            return Err(invalid(
                "mise history origin and branch must match the validated restore request",
            ));
        }
        Ok(())
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
        let source = self.source_urls(vars.source_repo.as_ref())?;
        let root = self.ctx.root.path();
        let check = |url: &str| {
            if !concrete_url(url)
                || !expected.matches(url)
                || source
                    .iter()
                    .any(|source| same_transport(root, source, url))
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
            Some(self.url_resolver()?)
        } else {
            None
        };
        let resolver = store.or_else(|| temporary.as_ref().map(|temp| temp.path()));
        self.url_config(resolver)?;
        for url in self
            .operand_urls(raw, || self.git(resolver))
            .map_err(|_| invalid_destination())?
        {
            check(&url)?;
        }
        if let Some(store) = store {
            for url in self.origin_urls(Some(store))? {
                check(&url)?;
            }
        }
        Ok(())
    }

    fn source_urls(&self, configured: Option<&RepoSetting>) -> Result<Vec<String>, Fail> {
        let checkout = exists(&self.ctx.root.path().join(".git"))?;
        if !checkout && configured.is_none() {
            return Ok(Vec::new());
        }
        let config = self.url_config(None)?;
        let mut urls = if checkout {
            self.origin_urls(None)?
        } else {
            Vec::new()
        };
        if let Some(repo) = configured {
            let raw = repo.transport_url();
            if !concrete_url(&raw) {
                return Err(invalid(
                    "could not validate the configured source repository",
                ));
            }
            let effective = if checkout {
                self.operand_urls(&raw, || self.git(None))?
            } else {
                // `git remote show -n` needs a repository. Without one, use
                // exactly the URL rules read from the source directory.
                let temp = self.url_resolver()?;
                self.operand_urls(&raw, || {
                    let mut cmd = self.git(Some(temp.path()))?;
                    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
                        .env("GIT_CONFIG_GLOBAL", "/dev/null")
                        .env_remove("GIT_CONFIG_PARAMETERS")
                        .env("GIT_CONFIG_COUNT", config.len().to_string());
                    for (index, (key, value)) in config.iter().enumerate() {
                        cmd.env(format!("GIT_CONFIG_KEY_{index}"), key)
                            .env(format!("GIT_CONFIG_VALUE_{index}"), value);
                    }
                    Ok(cmd)
                })?
            };
            urls.push(raw.into_owned());
            urls.extend(effective);
        }
        if urls
            .iter()
            .any(|url| !concrete_url(url) && local_transport(url).is_none())
        {
            return Err(invalid("could not validate the configured source origin"));
        }
        Ok(urls)
    }

    fn url_resolver(&self) -> Result<tempfile::TempDir, Fail> {
        let diagnostic = "could not stage history URL validation";
        let temp = tempfile::tempdir().map_err(|_| blocked(diagnostic))?;
        let mut cmd = self.git(None)?;
        cmd.args(["init", "--bare", "--template="]).arg(temp.path());
        self.output(cmd, diagnostic)?;
        Ok(temp)
    }

    fn operand_urls(
        &self,
        raw: &str,
        command: impl Fn() -> Result<Command, Fail>,
    ) -> Result<Vec<String>, Fail> {
        let diagnostic = "could not resolve effective repository URLs";
        let mut fetch = command()?;
        fetch.args(["ls-remote", "--get-url", "--", raw]);
        let fetch = self.text(fetch, diagnostic)?;
        let fetch = fetch
            .strip_suffix('\n')
            .ok_or_else(|| invalid(diagnostic))?;

        let mut push = command()?;
        push.args(["remote", "show", "-n", "--", raw]);
        let result = self.text(push, diagnostic)?;
        let pushes = result
            .lines()
            .filter_map(|line| line.strip_prefix("  Push  URL: "));
        let urls: Vec<String> = std::iter::once(fetch)
            .chain(pushes)
            .map(str::to_owned)
            .collect();
        if urls.len() < 2 {
            return Err(invalid(diagnostic));
        }
        Ok(urls)
    }

    fn origin_urls(&self, store: Option<&Path>) -> Result<Vec<String>, Fail> {
        let diagnostic = "could not resolve the repository origin URLs";
        let mut cmd = self.git(store)?;
        cmd.args([
            "config",
            "--null",
            "--get-regexp",
            "^remote\\.origin\\.(url|pushurl)$",
        ]);
        let mut urls = config_records(self.probe(cmd, diagnostic)?)?
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        if !urls.is_empty() {
            for push in [None, Some("--push")] {
                let mut cmd = self.git(store)?;
                cmd.args(["remote", "get-url", "--all"])
                    .args(push)
                    .arg("origin");
                let output = self
                    .text(cmd, diagnostic)
                    .map_err(|_| invalid(diagnostic))?;
                if output.is_empty() {
                    return Err(invalid(diagnostic));
                }
                urls.extend(output.lines().map(str::to_owned));
            }
        }
        Ok(urls)
    }

    fn url_config(&self, store: Option<&Path>) -> Result<Vec<(String, String)>, Fail> {
        // Git's URL queries are line-oriented. A control character in a URL
        // or rewrite can make one actual operand look like several safe ones.
        let mut cmd = self.git(store)?;
        cmd.args([
            "config",
            "--null",
            "--get-regexp",
            r"^(remote\..*\.(url|pushurl)|url\..*\.(insteadof|pushinsteadof))$",
        ]);
        config_records(self.probe(cmd, "could not inspect Git history URL configuration")?)
    }
}

fn existing_dir(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(io::Error::other("expected a native history directory")),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn pending_uuid(path: &Path) -> io::Result<String> {
    #[derive(Deserialize)]
    struct Record {
        #[serde(rename = "id")]
        _id: u64,
        checkpoint: Checkpoint,
        #[serde(rename = "recovery")]
        _recovery: Recovery,
    }

    #[derive(Deserialize)]
    struct Checkpoint {
        uuid: String,
        operation: Operation,
    }

    #[derive(Deserialize)]
    struct Operation {
        id: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Recovery {
        Pending,
        UnfinishedWrites,
        Finished,
    }

    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid native recovery record");
    let file = read_native_file(path)?;
    let record: Record =
        serde_json::from_reader(io::BufReader::new(file)).map_err(|_| invalid())?;
    let Checkpoint { uuid, operation } = record.checkpoint;
    // Native recover accepts UUID prefixes as well as numeric selectors.
    // Only a full UUID matching the filename and operation ID is safe here.
    if uuid.len() != 36
        || !uuid.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
        || path.file_stem().and_then(|stem| stem.to_str()) != Some(uuid.as_str())
        || uuid != operation.id
    {
        return Err(invalid());
    }
    Ok(uuid)
}

pub(super) fn read_native_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("expected a regular native history file"));
    }
    Ok(file)
}

fn concrete_url(url: &str) -> bool {
    // OWNER/NAME is accepted by RepoSetting only. Git treats a literal
    // OWNER/NAME operand as a path in the command's working directory.
    let http = url.starts_with("https://") || url.starts_with("http://");
    url.contains(':')
        && url.trim() == url
        && (!http || !url.contains(['?', '#']))
        && RepoSetting::parse(url).is_ok()
}

fn parse_origin(text: &str) -> Result<(&str, &str), Fail> {
    let fail = || invalid("could not read the effective mise history origin and branch");
    let text = text.strip_suffix('\n').unwrap_or(text);
    let (url, tail) = text.split_once(" (branch ").ok_or_else(fail)?;
    let (branch, _) = tail.split_once(") declared in ").ok_or_else(fail)?;
    // A literal URL or declaration path can contain the origin report's
    // delimiters. Refuse ambiguous reports rather than validating a prefix.
    if !concrete_url(url)
        || branch.is_empty()
        || tail.contains(" (branch ")
        || text.chars().any(char::is_control)
    {
        return Err(fail());
    }
    Ok((url, branch))
}

fn local_transport(url: &str) -> Option<&Path> {
    let path = if url.starts_with("file://localhost/") {
        &url["file://localhost".len()..]
    } else if let Some(path) = url.strip_prefix("file://") {
        if !path.starts_with('/') {
            return None;
        }
        path
    } else if !url.contains(':') {
        url
    } else {
        return None;
    };
    (!path.is_empty() && !path.starts_with('-') && !path.chars().any(char::is_control))
        .then(|| Path::new(path))
}

fn same_transport(cwd: &Path, source: &str, state: &str) -> bool {
    if let (Some(source), Some(state)) = (local_transport(source), local_transport(state)) {
        let source = cwd.join(source);
        let state = cwd.join(state);
        return source == state
            || crate::config::physical(&source)
                .ok()
                .zip(crate::config::physical(&state).ok())
                .is_some_and(|(source, state)| source == state);
    }
    concrete_url(source) && RepoSetting::parse(source).is_ok_and(|repo| repo.matches(state))
}

fn config_records(output: Output) -> Result<Vec<(String, String)>, Fail> {
    let invalid_config = || invalid("could not validate Git repository URL configuration");
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(Vec::new());
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| invalid_config())?;
    let text = text
        .strip_suffix('\0')
        .filter(|_| output.status.success())
        .ok_or_else(invalid_config)?;
    text.split('\0')
        .map(|entry| {
            let (key, value) = entry.split_once('\n').ok_or_else(invalid_config)?;
            if key.chars().any(char::is_control) || value.chars().any(char::is_control) {
                return Err(invalid_config());
            }
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn optional_config(output: Output) -> Result<Option<String>, Fail> {
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(None);
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| invalid("could not read the repository origin"))?;
    match (output.status.success(), text.strip_suffix('\0')) {
        (true, Some(url)) if concrete_url(url) => Ok(Some(url.to_owned())),
        (true, Some(url)) if local_transport(url).is_some() => Ok(None),
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
        "GIT_CONFIG",
        "GIT_CURL_VERBOSE",
    ] {
        cmd.env_remove(name);
    }
    // The launcher starts from env_clear(), so these are the inherited
    // trace settings, including GIT_TRACE_PACKFILE and GIT_TRACE2 targets.
    let traces = cmd
        .get_envs()
        .filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_TRACE"))
        .map(|(name, _)| name.to_owned())
        .collect::<Vec<_>>();
    for name in traces {
        cmd.env_remove(name);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{Env, Flags, config::Root, run::Sink};

    pub fn in_world(test: impl FnOnce(&Native<'_, '_>, &Path)) {
        let temp = tempfile::tempdir().unwrap();
        let world = temp.path().canonicalize().unwrap();
        let home = world.join("home");
        let root = world.join("config");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("config.toml"),
            "[vars]\ndots_profiles = 'fixture'\n",
        )
        .unwrap();
        let path = std::env::var_os("PATH").unwrap();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path))).unwrap();
        let vars = [
            ("PATH", path),
            ("HOME", home.clone().into()),
            ("DOTS_ROOT", root.clone().into()),
            ("CODEX_HOME", home.join("codex").into()),
            ("XDG_CONFIG_HOME", home.join("xdg-config").into()),
            ("XDG_DATA_HOME", home.join("xdg-data").into()),
            ("XDG_STATE_HOME", home.join("xdg-state").into()),
            ("XDG_CACHE_HOME", home.join("xdg-cache").into()),
            ("GIT_CONFIG_GLOBAL", home.join("gitconfig").into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CEILING_DIRECTORIES", world.clone().into()),
            ("GIT_ALLOW_PROTOCOL", "".into()),
            ("GIT_TRACE_PACKFILE", world.join("trace-pack").into()),
            ("GIT_TRACE2_EVENT", world.join("trace-event").into()),
            ("GIT_TRACE_FSMONITOR", world.join("trace-fsmonitor").into()),
            ("GIT_CURL_VERBOSE", "1".into()),
            ("JJ_CONFIG", home.join("jj.toml").into()),
            ("RUSTUP_HOME", home.join("rustup").into()),
            ("CARGO_HOME", home.join("cargo").into()),
            ("MISE_SYSTEM_CONFIG_DIR", home.join("mise-system").into()),
            ("MISE_CONFIG_DIR", world.join("alternate").into()),
            (
                "MISE_GLOBAL_CONFIG_FILE",
                world.join("alternate/base.toml").into(),
            ),
            ("MISE_DATA_DIR", home.join("mise-data").into()),
            ("MISE_CACHE_DIR", home.join("mise-cache").into()),
            ("MISE_TMP_DIR", home.join("mise-tmp").into()),
            ("MISE_INSTALLS_DIR", home.join("mise-installs").into()),
            (
                "MISE_INSTALL_STORE_DIR",
                home.join("mise-install-store").into(),
            ),
            ("MISE_PLUGINS_DIR", home.join("mise-plugins").into()),
            ("MISE_DOWNLOADS_DIR", home.join("mise-downloads").into()),
            ("MISE_STATE_DIR", "~/mise-state".into()),
            ("MISE_CD", world.join("alternate").into()),
            ("MISE_ENV", "ambient".into()),
            ("DOTS_MISE_BIN", std::env::current_exe().unwrap().into()),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect();
        let env = Env { vars, cwd: root };
        let ctx = Ctx {
            root: Root::resolve(&env).unwrap(),
            env,
            flags: Flags::default(),
        };
        let run = Run::begin(&ctx, Sink::Unreported);
        test(&Native::new(&run, &ctx), &world);
    }

    fn git(native: &Native<'_, '_>, store: Option<&Path>, args: &[&str]) {
        let mut cmd = native.git(store).unwrap();
        cmd.args(args);
        native.output(cmd, "fixture Git failed").unwrap();
    }

    #[test]
    fn refuses_every_effective_live_source_destination_even_with_a_configured_source() {
        in_world(|native, world| {
            git(native, None, &["init"]);
            let state = format!("file://{}", world.join("state.git").display());
            let source = format!("file://{}", world.join("source.git").display());
            let declared = format!("file://{}", world.join("declared.git").display());
            let other = format!("file://{}", world.join("other.git").display());
            git(native, None, &["remote", "add", "origin", &source]);
            let expected = RepoSetting::parse(&state).unwrap();
            let mut vars = Vars::read(&native.ctx.root).unwrap();
            vars.source_repo = Some(RepoSetting::parse(&declared).unwrap());
            let check = || native.destinations(None, &state, &expected, &vars);
            check().unwrap();

            let rewrite = format!("url.{state}.insteadOf");
            git(native, None, &["config", &rewrite, &source]);
            assert_eq!(check().unwrap_err().status, 2);
            git(native, None, &["config", "--unset", &rewrite]);

            git(
                native,
                None,
                &["config", "--add", "remote.origin.pushurl", &other],
            );
            check().unwrap();
            git(
                native,
                None,
                &["config", "--add", "remote.origin.pushurl", &state],
            );
            assert_eq!(check().unwrap_err().status, 2);
        });
    }

    #[test]
    fn resolves_configured_source_rewrites_with_and_without_a_live_origin() {
        in_world(|native, world| {
            let state = format!("file://{}", world.join("private.git").display());
            let source = format!("file://{}", world.join("configured.git").display());
            let distinct = format!("file://{}", world.join("distinct.git").display());
            let mut vars = Vars::read(&native.ctx.root).unwrap();
            vars.source_repo = Some(RepoSetting::parse(&source).unwrap());
            let expected = RepoSetting::parse(&state).unwrap();
            let check = || native.destinations(None, &state, &expected, &vars);
            check().unwrap();

            // A configured source still resolves global push rules when its
            // checkout has never been initialized.
            let global = native.home().unwrap().join("gitconfig");
            fs::write(
                &global,
                format!("[url \"{state}\"]\n pushInsteadOf = {source}\n"),
            )
            .unwrap();
            assert_eq!(check().unwrap_err().status, 2);
            fs::remove_file(global).unwrap();

            git(native, None, &["init"]);
            git(native, None, &["remote", "add", "origin", &distinct]);
            check().unwrap();
            for rule in ["insteadOf", "pushInsteadOf"] {
                let key = format!("url.{state}.{rule}");
                git(native, None, &["config", &key, &source]);
                assert_eq!(check().unwrap_err().status, 2);
                git(native, None, &["config", "--unset", &key]);
            }
            check().unwrap();
        });
    }

    #[test]
    fn checks_native_fetch_push_rewrites_and_concrete_operands() {
        in_world(|native, world| {
            let state = format!("file://{}", world.join("private.git").display());
            let other = format!("file://{}", world.join("other.git").display());
            let vars = Vars::read(&native.ctx.root).unwrap();
            let expected = RepoSetting::parse(&state).unwrap();
            let store = world.join("store.git");
            fs::create_dir(&store).unwrap();
            git(native, Some(&store), &["init", "--bare"]);
            let check = || native.destinations(Some(&store), &state, &expected, &vars);
            check().unwrap();
            for rule in ["insteadOf", "pushInsteadOf"] {
                let key = format!("url.{other}.{rule}");
                git(native, Some(&store), &["config", &key, &state]);
                assert_eq!(check().unwrap_err().status, 2);
                git(native, Some(&store), &["config", "--unset", &key]);
            }
            let github = RepoSetting::parse("example/history").unwrap();
            assert_eq!(
                native
                    .destinations(Some(&store), "example/history", &github, &vars)
                    .unwrap_err()
                    .status,
                2
            );
            assert!(!same_transport(
                native.ctx.root.path(),
                "example/history",
                "https://github.com/example/history"
            ));
            assert!(same_transport(
                native.ctx.root.path(),
                "../private.git",
                &state
            ));
        });
    }

    #[test]
    fn uses_managed_mise_root_selection_and_store() {
        in_world(|native, _| {
            let mut native = Native::new(native.run, native.ctx);
            native.select(Selection {
                profile: Some("fixture".to_owned()),
                machine_id: Some("example".to_owned()),
                selectors: vec!["extra".to_owned()],
            });
            let cmd = native.mise(&["bootstrap", "dotfiles", "origin"]).unwrap();
            let env = cmd.get_envs().collect::<std::collections::BTreeMap<_, _>>();
            use std::ffi::OsStr;
            for name in ["MISE_GLOBAL_CONFIG_FILE", "MISE_CD"] {
                assert_eq!(env.get(OsStr::new(name)).copied().flatten(), None);
            }
            assert_eq!(
                env[OsStr::new("MISE_CONFIG_DIR")],
                Some(native.ctx.root.path().as_os_str())
            );
            assert_eq!(
                env[OsStr::new("MISE_STATE_DIR")],
                Some(native.home().unwrap().join("mise-state").as_os_str())
            );
            assert_eq!(
                env[OsStr::new("MISE_ENV")],
                Some(OsStr::new("fixture,host-example,extra"))
            );
            assert_eq!(cmd.get_current_dir(), Some(native.ctx.root.path()));
        });
    }

    #[test]
    fn origin_report_cannot_disguise_the_native_url() {
        let prefix = "file:///private.git (branch host-fixture) declared in ";
        assert_eq!(
            parse_origin(&format!("{prefix}config.local.toml; mode manual\n")).unwrap(),
            ("file:///private.git", "host-fixture")
        );
        assert!(
            parse_origin(&format!(
                "{prefix}different (branch host-other) declared in config.local.toml; mode manual\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn inventories_both_native_scopes_without_creating_or_opening_a_repository() {
        in_world(|native, _| {
            let state = native.state_dir().unwrap();
            native.require_no_pending_operations().unwrap();
            assert!(!state.exists());

            let shared_uuid = "01900000-0000-7000-8000-000000000001";
            let local_uuid = "01900000-0000-7000-8000-000000000002";
            let history = state.join("history");
            fs::create_dir_all(&history).unwrap();
            fs::write(history.join("operation.json"), b"{}").unwrap();
            native.require_no_pending_operations().unwrap();
            for (scope, uuid, recovery) in [
                (state.clone(), shared_uuid, "pending"),
                (state.join("history-local"), local_uuid, "finished"),
            ] {
                let pending = scope.join("history/index/pending");
                fs::create_dir_all(&pending).unwrap();
                fs::write(pending.join("native-write.tmp"), "not a pending record").unwrap();
                let record = serde_json::json!({
                    "id": 2,
                    "recovery": recovery,
                    "checkpoint": {"uuid": uuid, "operation": {"id": uuid}}
                });
                fs::write(pending.join(format!("{uuid}.json")), record.to_string()).unwrap();
            }
            assert_eq!(
                native.pending_operations().unwrap(),
                [
                    PendingOperation {
                        uuid: shared_uuid.into(),
                        local: false
                    },
                    PendingOperation {
                        uuid: local_uuid.into(),
                        local: true
                    },
                ]
            );
            assert_eq!(
                native.require_no_pending_operations().unwrap_err().status,
                1
            );
            assert!(!native.store().unwrap().exists());

            let path = state.join(format!("history/index/pending/{shared_uuid}.json"));
            let wrong_name = path.with_file_name(format!("{local_uuid}.json"));
            fs::rename(&path, &wrong_name).unwrap();
            assert_eq!(native.pending_operations().unwrap_err().status, 2);
            fs::remove_file(wrong_name).unwrap();
            fs::write(path, "malformed private record").unwrap();
            let error = native.pending_operations().unwrap_err();
            assert_eq!(error.status, 2);
            assert!(!error.diagnostic.contains("malformed private record"));
        });
    }

    #[test]
    fn removes_inherited_trace_outputs_for_direct_and_native_git() {
        in_world(|native, _| {
            for cmd in [
                native.git(None).unwrap(),
                native.mise(&["dot", "origin"]).unwrap(),
            ] {
                for (name, value) in cmd.get_envs() {
                    if name.as_encoded_bytes().starts_with(b"GIT_TRACE")
                        || name == "GIT_CURL_VERBOSE"
                    {
                        assert!(value.is_none());
                    }
                }
            }
        });
    }

    #[test]
    fn dots_owned_failure_omits_transport_stderr() {
        in_world(|native, _| {
            let mut cmd = native.command("sh").unwrap();
            cmd.args(["-c", "printf '%s' synthetic-transport-error >&2; exit 1"]);
            let err = native.output(cmd, "history sync failed").unwrap_err();
            assert_eq!(err.diagnostic, "history sync failed");
        });
    }
}
