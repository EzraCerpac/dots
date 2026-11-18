use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::config::{RepoSetting, Vars, passive};
use crate::process::Cwd;
use crate::run::{Outcome, Run};
use crate::{Ctx, Fail};

pub(super) fn clear_git_checkout_override(command: &mut Command) {
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
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

// P-06 owns RepoSetting::transport_url. Keep the conversion local until that
// accessor is in the base branch.
pub(super) fn transport_url(value: &str) -> String {
    if value.contains(':') {
        value.to_owned()
    } else {
        format!("https://github.com/{value}")
    }
}

pub(super) fn configured_transport(ctx: &Ctx, key: &str) -> Result<String, Fail> {
    let doc = passive::toml(&ctx.root.path().join("config.toml"))?;
    let raw = doc
        .get("vars")
        .and_then(|table| table.get(key))
        .and_then(|value| value.as_str())
        .ok_or_else(|| Fail::usage(format!("config.toml is missing [vars] {key}")))?;
    RepoSetting::parse(raw)?;
    Ok(transport_url(raw))
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

pub(super) fn validate_native_source(
    ctx: &Ctx,
    run: &Run<'_>,
    value: &str,
    vars: &Vars,
) -> Result<String, Fail> {
    let (repo, reference) = value
        .split_once("?ref=")
        .map_or((value, None), |(repo, reference)| (repo, Some(reference)));
    let setting = RepoSetting::parse(repo)?;
    let url = transport_url(repo);
    let expected = vars.source_repo.as_ref().unwrap_or(&setting);
    if !expected.matches(&url) {
        return Err(Fail::usage(
            "the requested bootstrap source does not match dots_source_repo",
        ));
    }
    validate_destination(ctx, run, &url, expected, false)?;
    Ok(match reference {
        Some(reference) => format!("{url}?ref={reference}"),
        None => url,
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

pub(super) fn validate_update(
    ctx: &Ctx,
    run: &Run<'_>,
    vars: &Vars,
    source: Option<&(&str, String)>,
    from_dir: Option<&OsString>,
) -> Result<(), Fail> {
    let root = ctx.root.path();
    let checkout = match source {
        Some(("from", _)) => {
            let path = if let Some(path) = from_dir {
                PathBuf::from(path)
            } else if let Some(data) = ctx.env.get("MISE_DATA_DIR").filter(|dir| !dir.is_empty()) {
                PathBuf::from(data).join("bootstrap-repo")
            } else {
                let data = ctx
                    .env
                    .get("XDG_DATA_HOME")
                    .filter(|dir| !dir.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| ctx.env.home().map(|home| home.join(".local/share")))
                    .ok_or_else(|| {
                        Fail::usage(
                            "set HOME or MISE_DATA_DIR before updating a bootstrap checkout",
                        )
                    })?;
                data.join("mise/bootstrap-repo")
            };
            root.join(path)
        }
        _ => root.to_owned(),
    };
    if !checkout.join(".git").exists() {
        return Ok(());
    }
    let query = |args: &[&str]| {
        let mut command = git(ctx)?;
        command.arg("-C").arg(&checkout).args(args);
        probe(run, command)
    };
    if source.is_none()
        && !query(&["rev-parse", "--symbolic-full-name", "@{upstream}"])?
            .status
            .success()
    {
        // Unadorned mise bootstrap skips the global checkout when it has no upstream.
        return Ok(());
    }

    let selected = if let Some((_, url)) = source {
        Some(RepoSetting::parse(
            url.split_once("?ref=").map_or(url, |(repo, _)| repo),
        )?)
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
    if source.is_some_and(|(_, url)| url.contains("?ref=")) {
        return validate_remote(ctx, run, &checkout, "origin", expected);
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
