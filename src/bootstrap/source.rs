use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::config::{RepoSetting, Vars};
use crate::process::Cwd;
use crate::run::{Outcome, Run};
use crate::{Ctx, Fail};

pub(super) fn clear_git_checkout_override(command: &mut Command) {
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_CONFIG",
    ] {
        command.env_remove(key);
    }
}

fn git(ctx: &Ctx) -> Result<Command, Fail> {
    let mut command = ctx.command("git", Cwd::Root)?;
    clear_git_checkout_override(&mut command);
    Ok(command)
}

fn probe(run: &Run<'_>, command: Command) -> Result<Output, Fail> {
    run.probe(command).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Fail::new(1, "git is required for bootstrap repository checks")
        } else {
            Fail::new(1, format!("could not run git for bootstrap: {error}"))
        }
    })
}

fn git_text<const N: usize>(ctx: &Ctx, run: &Run<'_>, args: [&str; N]) -> Result<Output, Fail> {
    let mut command = git(ctx)?;
    command.args(args);
    probe(run, command)
}

pub(super) fn origin(ctx: &Ctx, run: &Run<'_>) -> Result<Option<String>, Fail> {
    if !ctx.root.path().join(".git").exists() {
        return Ok(None);
    }
    let output = git_text(ctx, run, ["config", "--get", "remote.origin.url"])?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Ok(Some(text(output, "could not read Git origin")?))
}

fn text(output: Output, diagnostic: &str) -> Result<String, Fail> {
    if !output.status.success() {
        return Err(Fail::new(1, diagnostic));
    }
    let result = String::from_utf8(output.stdout).map_err(|_| Fail::new(1, diagnostic))?;
    Ok(result.strip_suffix('\n').unwrap_or(&result).to_owned())
}

pub(super) fn validate_destination(
    ctx: &Ctx,
    run: &Run<'_>,
    url: &str,
    expected: &RepoSetting,
    from_origin: bool,
) -> Result<(), Fail> {
    if from_origin {
        validate_remote(ctx, run, ctx.root.path(), "origin", expected)?;
    } else {
        let output = git_text(ctx, run, ["ls-remote", "--get-url", "--", url])?;
        let fetch = text(output, "could not resolve the bootstrap fetch URL")?;
        check_url(&fetch, expected)?;
        let rules = git_text(
            ctx,
            run,
            [
                "config",
                "--null",
                "--get-regexp",
                r"^url\..*\.pushinsteadof$",
            ],
        )?;
        if rules.status.code() != Some(1) {
            let rules = text(rules, "could not read bootstrap Git push URL rewrites")?;
            if let Some(push) = push_url(url, &rules)? {
                check_url(&push, expected)?;
            }
        }
    }
    Ok(())
}

fn check_url(url: &str, expected: &RepoSetting) -> Result<(), Fail> {
    if expected.matches(url) {
        Ok(())
    } else {
        Err(Fail::usage(
            "a Git URL rewrite or push URL points outside the expected bootstrap repository",
        ))
    }
}

fn push_url(url: &str, rules: &str) -> Result<Option<String>, Fail> {
    // For a literal remote URL, Git uses the longest pushInsteadOf match.
    // With no match, the effective push URL is the fetch URL we already checked.
    let mut rewrites: Vec<(&str, Vec<&str>)> = Vec::new();
    for rule in rules.split_terminator('\0') {
        let (key, prefix) = rule
            .split_once('\n')
            .ok_or_else(|| Fail::new(1, "could not parse bootstrap Git push URL rewrites"))?;
        let base = key
            .strip_prefix("url.")
            .and_then(|key| key.strip_suffix(".pushinsteadof"))
            .ok_or_else(|| Fail::new(1, "could not parse bootstrap Git push URL rewrites"))?;
        if let Some((_, prefixes)) = rewrites.iter_mut().find(|(seen, _)| *seen == base) {
            prefixes.push(prefix);
        } else {
            rewrites.push((base, vec![prefix]));
        }
    }
    let mut longest = None;
    for (base, prefixes) in rewrites {
        for prefix in prefixes {
            if let Some(suffix) = url.strip_prefix(prefix)
                && longest.is_none_or(|(length, _, _)| prefix.len() > length)
            {
                longest = Some((prefix.len(), base, suffix));
            }
        }
    }
    Ok(longest.map(|(_, base, suffix)| format!("{base}{suffix}")))
}

pub(super) struct NativeSource {
    url: String,
    reference: Option<String>,
}

impl NativeSource {
    pub(super) fn argument(&self) -> String {
        match &self.reference {
            Some(reference) => {
                let separator = if self.url.contains('?') { '&' } else { '?' };
                format!("{}{separator}ref={reference}", self.url)
            }
            None => self.url.clone(),
        }
    }
}

fn credential_query_key(key: &str) -> bool {
    let mut pieces = key.split('%');
    let mut decoded = pieces.next().unwrap_or_default().to_owned();
    for piece in pieces {
        if let Some(hex) = piece.get(..2)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(char::from(byte));
            decoded.push_str(&piece[2..]);
        } else {
            decoded.push('%');
            decoded.push_str(piece);
        }
    }
    decoded.make_ascii_lowercase();
    decoded.contains("token") || decoded.contains("password") || decoded == "key"
}

pub(super) fn validate_native_source(
    ctx: &Ctx,
    run: &Run<'_>,
    value: &str,
    vars: &Vars,
) -> Result<NativeSource, Fail> {
    let invalid = || Fail::usage("invalid bootstrap source repository or Git ref");
    if value.chars().any(char::is_control) {
        return Err(invalid());
    }
    // Mise keeps every query pair except ref=, and uses the last ref value.
    let mut reference = None;
    let mut repo = value.to_owned();
    if let Some((base, query)) = value.split_once('?') {
        let mut rest = Vec::new();
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("ref=") {
                reference = Some(value);
            } else {
                let key = pair.split_once('=').map_or(pair, |(key, _)| key);
                if credential_query_key(key) {
                    return Err(Fail::usage(
                        "refusing a URL that carries a password or token",
                    ));
                }
                rest.push(pair);
            }
        }
        if reference.is_some_and(|value| {
            value.is_empty() || value.starts_with('-') || value == "HEAD" || value.contains('?')
        }) {
            return Err(invalid());
        }
        repo.truncate(base.len());
        if !rest.is_empty() {
            repo.push('?');
            repo.push_str(&rest.join("&"));
        }
    }
    let setting = RepoSetting::parse(&repo)?;
    let url = setting.transport_url().into_owned();
    let expected = vars.source_repo.as_ref().unwrap_or(&setting);
    if !expected.matches(&url) {
        return Err(Fail::usage(
            "the requested bootstrap source does not match dots_source_repo",
        ));
    }
    validate_destination(ctx, run, &url, expected, false)?;
    Ok(NativeSource {
        url,
        reference: reference.map(str::to_owned),
    })
}

fn validate_remote(
    ctx: &Ctx,
    run: &Run<'_>,
    checkout: &Path,
    remote: &str,
    expected: &RepoSetting,
) -> Result<(), Fail> {
    for mode in [None, Some("--push")] {
        let mut command = git(ctx)?;
        command
            .arg("-C")
            .arg(checkout)
            .args(["remote", "get-url", "--all"]);
        command.args(mode).arg("--").arg(remote);
        let urls = text(
            probe(run, command)?,
            "could not resolve bootstrap repository URLs",
        )?;
        for effective in urls.split('\n') {
            check_url(effective, expected)?;
        }
    }
    Ok(())
}

fn mise_path(ctx: &Ctx, name: &str) -> Result<Option<PathBuf>, Fail> {
    let Some(value) = ctx.env.get(name).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = Path::new(value);
    if let Ok(tail) = path.strip_prefix("~/") {
        let home = ctx
            .env
            .home()
            .ok_or_else(|| Fail::usage("set HOME to expand a mise bootstrap directory"))?;
        return Ok(Some(home.join(tail)));
    }
    Ok(Some(path.to_owned()))
}

fn update_directory(ctx: &Ctx, from: bool, from_dir: Option<&Path>) -> Result<PathBuf, Fail> {
    let root = ctx.root.path();
    if from {
        let checkout = if let Some(path) = from_dir {
            PathBuf::from(path)
        } else {
            let data = match mise_path(ctx, "MISE_DATA_DIR")? {
                Some(data) => data,
                None => mise_path(ctx, "XDG_DATA_HOME")?
                    .or_else(|| ctx.env.home().map(|home| home.join(".local/share")))
                    .ok_or_else(|| {
                        Fail::usage(
                            "set HOME or MISE_DATA_DIR before updating a bootstrap checkout",
                        )
                    })?
                    .join("mise"),
            };
            data.join("bootstrap-repo")
        };
        return Ok(root.join(checkout));
    }
    // Mise uses the explicit global file's parent, including for --adopt.
    let global_file = match mise_path(ctx, "MISE_GLOBAL_CONFIG_FILE")? {
        Some(file) => Some(file),
        None => mise_path(ctx, "MISE_CONFIG_FILE")?,
    };
    Ok(global_file
        .and_then(|path| path.parent().map(|parent| root.join(parent)))
        .unwrap_or_else(|| root.to_owned()))
}

pub(super) fn validate_update(
    ctx: &Ctx,
    run: &Run<'_>,
    vars: &Vars,
    source: Option<&(&str, NativeSource)>,
    from_dir: Option<&Path>,
) -> Result<(), Fail> {
    let dir = update_directory(
        ctx,
        source.is_some_and(|(flag, _)| *flag == "from"),
        from_dir,
    )?;
    let Ok(resolved) = dir.canonicalize() else {
        return Ok(());
    };
    if source.is_some()
        && resolved.is_dir()
        && resolved
            .read_dir()
            .map_err(|_| Fail::new(1, "could not inspect the bootstrap checkout"))?
            .next()
            .is_none()
    {
        return Ok(());
    }
    let query_in = |path: &Path, args: &[&str]| {
        let mut command = git(ctx)?;
        command.arg("-C").arg(path).args(args);
        probe(run, command)
    };
    let top = query_in(&resolved, &["rev-parse", "--show-toplevel"])?;
    if !top.status.success() {
        return Ok(());
    }
    let path = top.stdout.strip_suffix(b"\n").unwrap_or(&top.stdout);
    let checkout = Path::new(OsStr::from_bytes(path))
        .canonicalize()
        .map_err(|_| Fail::new(1, "could not resolve the bootstrap checkout"))?;
    if source.is_none() && checkout != resolved && !dir.is_symlink() {
        // A global directory nested in an unrelated repository is skipped.
        return Ok(());
    }
    let query = |args: &[&str]| query_in(&checkout, args);
    if source.is_none()
        && !query(&["rev-parse", "--symbolic-full-name", "@{upstream}"])?
            .status
            .success()
    {
        // Unadorned mise bootstrap skips the global checkout when it has no upstream.
        return Ok(());
    }

    let selected = if let Some((_, source)) = source {
        Some(RepoSetting::parse(&source.url)?)
    } else if vars.source_repo.is_none() {
        Some(RepoSetting::parse(&origin(ctx, run)?.ok_or_else(
            || {
                Fail::usage(
                    "set dots_source_repo or configure the source origin before bootstrap --update",
                )
            },
        )?)?)
    } else {
        None
    };
    let expected = vars
        .source_repo
        .as_ref()
        .or(selected.as_ref())
        .expect("configured source");
    if let Some((_, source)) = source {
        let raw = query(&["config", "--get", "remote.origin.url"])?;
        let raw = text(raw, "could not read the bootstrap checkout origin")?;
        check_url(&raw, expected)?;
        if source.reference.is_some() {
            return validate_remote(ctx, run, &checkout, "origin", expected);
        }
    }

    // A reused checkout without a requested ref uses git pull, just like
    // mise's global-checkout update. Git chooses the branch remote, else origin.
    let branch = query(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let remote = if branch.status.success() {
        let branch = text(branch, "could not identify the bootstrap update branch")?;
        let setting = query(&["config", "--get", &format!("branch.{branch}.remote")])?;
        if setting.status.code() == Some(1) {
            "origin".to_owned()
        } else {
            text(setting, "could not resolve the bootstrap update remote")?
        }
    } else {
        "origin".to_owned()
    };
    validate_remote(ctx, run, &checkout, &remote, expected)
}

fn reference(ctx: &Ctx, run: &Run<'_>, name: &str) -> Result<Option<String>, Fail> {
    let output = git_text(ctx, run, ["rev-parse", "--verify", "--quiet", name])?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Ok(Some(text(
        output,
        "could not inspect bootstrap source refs",
    )?))
}

pub(super) fn prepare(ctx: &Ctx, run: &Run<'_>, vars: &Vars) -> Result<Outcome, Fail> {
    if !ctx.root.path().join(".git").exists() {
        return Ok(Outcome::Skipped);
    }
    let inside = git_text(ctx, run, ["rev-parse", "--show-toplevel"])?;
    if !inside.status.success()
        || inside.stdout.strip_suffix(b"\n") != Some(ctx.root.path().as_os_str().as_bytes())
    {
        return Err(Fail::new(
            1,
            "configuration root is not the Git worktree root",
        ));
    }
    let branch = git_text(ctx, run, ["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if !branch.status.success() || branch.stdout != b"main\n" {
        return Ok(Outcome::Skipped);
    }
    let Some(url) = origin(ctx, run)? else {
        return Ok(Outcome::Skipped);
    };
    let origin_setting = RepoSetting::parse(&url)?;
    if vars
        .source_repo
        .as_ref()
        .is_some_and(|expected| !expected.matches(&url))
    {
        return Err(Fail::usage(
            "the configuration's Git origin does not match dots_source_repo",
        ));
    }
    // Only a clean clone at its fetched main tip is eligible. No fetch is needed.
    let Some(head) = reference(ctx, run, "HEAD")? else {
        return Ok(Outcome::Skipped);
    };
    if reference(ctx, run, "refs/remotes/origin/main")?.as_deref() != Some(head.as_str())
        || !text(
            git_text(ctx, run, ["status", "--porcelain", "--untracked-files=all"])?,
            "could not check the bootstrap source for local edits",
        )?
        .is_empty()
    {
        return Ok(Outcome::Skipped);
    }
    // Switching can run Git's normal checkout hook. Ensure the configured
    // destination still agrees with the source before invoking it.
    validate_destination(
        ctx,
        run,
        &url,
        vars.source_repo.as_ref().unwrap_or(&origin_setting),
        true,
    )?;
    let mut command = git(ctx)?;
    command.args(["switch", "--quiet", "--no-guess", "--no-overwrite-ignore"]);
    match (
        reference(ctx, run, "refs/heads/dev")?,
        reference(ctx, run, "refs/remotes/origin/dev")?,
    ) {
        (Some(local), Some(remote)) if local == remote => {
            command.arg("dev");
        }
        (Some(local), None) if local == head => {
            command.arg("dev");
        }
        (Some(_), _) => return Ok(Outcome::Skipped),
        (None, Some(_)) => {
            command.args(["--track", "-c", "dev", "refs/remotes/origin/dev"]);
        }
        (None, None) => {
            command.args(["-c", "dev"]);
        }
    }
    text(
        run.probe(command)
            .map_err(|_| Fail::new(1, "could not run git to select dev"))?,
        "could not switch the clean bootstrap source checkout to dev",
    )?;
    Ok(Outcome::Completed)
}
