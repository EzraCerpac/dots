use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use semver::Version;
use ureq::http::header::LOCATION;

use crate::Ctx;
use crate::packages::rename_noclobber;
use crate::process::{Cwd, Mise};
use crate::run::{Change, Outcome, Run};

use super::tools::{self, Tool};
use super::{json, mise_command, step};

const TOOL: &str = "github:EzraCerpac/dots";
const RELEASES: &str = "https://github.com/EzraCerpac/dots/releases";
const TAG_PATH: &str = "/EzraCerpac/dots/releases/tag/";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(60);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Direct {
    pub tool: String,
    install_filter: (&'static str, String),
}

impl Direct {
    pub(super) fn exclude_from_install(&self, command: &mut Command) {
        command.env(self.install_filter.0, &self.install_filter.1);
    }
}

pub(super) fn direct(run: &Run<'_>, mise: &Mise<'_>, files: &[u8]) -> Option<Direct> {
    #[derive(serde::Deserialize)]
    struct ConfigFile {
        #[serde(default)]
        tools: Vec<String>,
    }
    let files: Vec<ConfigFile> = serde_json::from_slice(files).ok()?;
    let names: std::collections::BTreeSet<_> = files
        .iter()
        .flat_map(|file| file.tools.iter().map(String::as_str))
        .filter(|name| crate::config::manager_tool(name))
        .collect();
    let mut requests = names.into_iter().filter_map(|name| {
        // This field comes from mise's effective requests, even when no
        // version can be resolved. A policy probe must not fetch versions.
        let cmd =
            mise_command(run, mise, ["tool", name, "--requested", "--json"]).map(|mut cmd| {
                cmd.env("MISE_OFFLINE", "1");
                cmd
            });
        let requested: Option<Vec<String>> = json(run, cmd).ok()?;
        Some((name, requested?))
    });
    let (tool, requested) = requests.next()?;
    if !tool.eq_ignore_ascii_case(TOOL) || requested != ["latest"] || requests.next().is_some() {
        return None;
    }

    let setting = |name| tools::text(run, mise_command(run, mise, ["settings", "get", name]));
    if setting("locked").as_deref() == Some("true") {
        return None;
    }
    if setting("lockfile").as_deref() != Some("false") {
        // An applicable lock returns a row, including when its version is missing.
        let locked: Vec<Tool> = json(
            run,
            mise_command(run, mise, ["--locked", "ls", "--current", "--json", tool]),
        )
        .ok()?;
        if !locked.is_empty() {
            return None;
        }
    }

    // mise's install command has no --exclude. Preserve the effective filter:
    // an allowlist takes precedence over the blocklist, even if it is empty.
    let install_filter = if let Some(value) = setting("enable_tools") {
        let mut allowed: Vec<String> = serde_json::from_str(&value).ok()?;
        allowed.retain(|name| !name.eq_ignore_ascii_case(tool));
        ("MISE_ENABLE_TOOLS", allowed.join(","))
    } else {
        let mut disabled: Vec<String> = serde_json::from_str(&setting("disable_tools")?).ok()?;
        disabled.push(tool.to_owned());
        ("MISE_DISABLE_TOOLS", disabled.join(","))
    };
    Some(Direct {
        tool: tool.to_owned(),
        install_filter,
    })
}

pub(super) fn update(ctx: &Ctx, run: &Run<'_>, before: Direct) {
    step(run, "dots manager", |id| {
        let changed = (|| {
            // Hooks and standalone mise may have changed the effective selection.
            let (_, mise, files) = super::Config::reload(ctx, run).ok()?;
            let direct = direct(run, &mise, &files)?;
            if before.tool != direct.tool
                || tools::text(
                    run,
                    mise_command(run, &mise, ["settings", "get", "offline"]),
                )
                .as_deref()
                    != Some("false")
            {
                return None;
            }
            let running = env!("CARGO_PKG_VERSION").parse::<Version>().ok()?;
            let installed: Vec<Installed> = json(
                run,
                mise_command(run, &mise, ["ls", "--installed", "--json", &direct.tool]),
            )
            .ok()?;
            let release = latest_release()?;
            if release.cmp_precedence(&running).is_le()
                || installed
                    .iter()
                    .filter_map(|entry| entry.version.parse::<Version>().ok())
                    .any(|version| release.cmp_precedence(&version).is_le())
            {
                return None;
            }
            install(ctx, run, &mise, &direct, &release)?;
            Some(Change {
                item: "dots".to_owned(),
                before: running.to_string(),
                after: format!("{release}, used from the next run"),
            })
        })();
        if let Some(change) = changed {
            run.change(id, change);
            Ok(Outcome::Completed)
        } else {
            Ok(Outcome::Skipped)
        }
    });
}

#[derive(serde::Deserialize)]
struct Installed {
    version: String,
}

fn latest_release() -> Option<Version> {
    // GitHub normally returns the release page in one redirect. Never follow
    // the redirect or send credentials to the web endpoint.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(LOOKUP_TIMEOUT))
        .user_agent("dots")
        .build()
        .into();
    let response = agent.head(format!("{RELEASES}/latest")).call().ok()?;
    if !response.status().is_redirection() {
        return None;
    }
    release_version(response.headers().get(LOCATION)?.to_str().ok()?)
}

fn release_version(location: &str) -> Option<Version> {
    let path = location
        .strip_prefix("https://github.com")
        .unwrap_or(location);
    let tag = path.strip_prefix(TAG_PATH)?;
    tag.strip_prefix('v').unwrap_or(tag).parse().ok()
}

fn install(
    ctx: &Ctx,
    run: &Run<'_>,
    mise: &Mise<'_>,
    direct: &Direct,
    release: &Version,
) -> Option<()> {
    let tools = tools::list(run, mise).ok()?;
    let [request] = tools.get(&direct.tool)?.as_slice() else {
        return None;
    };
    let parent = request.install_path.parent()?;
    let installs = parent.parent()?;
    let data = installs.parent()?;
    let user_installs = crate::config_commands::mise_installs(&ctx.env)?;
    if !data.is_absolute()
        || (installs != user_installs
            && installs.canonicalize().ok()? != user_installs.canonicalize().ok()?)
    {
        return None;
    }
    let stage = tempfile::Builder::new()
        .prefix(".dots-manager-")
        .tempdir_in(data)
        .ok()?;
    let isolated = |args: &[&str]| {
        let mut command = mise_command(run, mise, args.iter().copied()).ok()?;
        command
            .current_dir(stage.path())
            .env_remove("MISE_CD")
            .env("MISE_CONFIG_DIR", stage.path())
            .env("MISE_GLOBAL_CONFIG_FILE", "")
            .env("MISE_CONFIG_FILE", "")
            .env("MISE_SYSTEM_CONFIG_FILE", "")
            .env("MISE_SYSTEM_CONFIG_DIR", stage.path())
            .env("MISE_CEILING_PATHS", stage.path())
            .env("MISE_DATA_DIR", stage.path())
            .env("MISE_INSTALLS_DIR", stage.path().join("installs"))
            .env("MISE_STATE_DIR", stage.path().join("state"))
            .env("MISE_SHIMS_DIR", stage.path().join("shims"));
        Some(command)
    };
    let install_request = format!("{}@{release}", direct.tool);
    let output = run
        .probe_detached(isolated(&["install", &install_request])?, INSTALL_TIMEOUT)
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let version = release.to_string();
    let staged = stage
        .path()
        .join("installs")
        .join(parent.file_name()?)
        .join(&version);
    let reported = bounded_text(
        run,
        isolated(&["which", "--tool", &install_request, "dots"])?,
    )?;
    let executable = Path::new(&reported).canonicalize().ok()?;
    let staged_path = staged.canonicalize().ok()?;
    if !executable.starts_with(&staged_path)
        || !reports_version(ctx, run, &executable, &version)
        || run.cancelled()
    {
        return None;
    }
    // Another installer can win the race. Keep its installation untouched.
    let identity = staged.metadata().ok()?;
    fs::create_dir_all(parent).ok()?;
    let target = parent.join(&version);
    rename_noclobber(&staged, &target).ok()?;

    let selected = mise_command(run, mise, ["which", "dots"])
        .ok()
        .and_then(|command| bounded_text(run, command))
        .and_then(|path| Path::new(&path).canonicalize().ok());
    if let (Some(selected), Ok(target)) = (selected, target.canonicalize())
        && selected.starts_with(target)
        && reports_version(ctx, run, &selected, &version)
    {
        return Some(());
    }
    // Withdraw only the candidate we published, never an existing install.
    if target
        .metadata()
        .is_ok_and(|current| (current.dev(), current.ino()) == (identity.dev(), identity.ino()))
    {
        let _ = rename_noclobber(&target, &staged);
    }
    None
}

fn bounded_text(run: &Run<'_>, command: Command) -> Option<String> {
    let output = run.probe_detached(command, VERIFY_TIMEOUT).ok()?;
    output.status.success().then_some(())?;
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn reports_version(ctx: &Ctx, run: &Run<'_>, executable: &Path, version: &str) -> bool {
    let Ok(mut command) = ctx.command(executable, Cwd::Root) else {
        return false;
    };
    command.env("DOTS_SKILL_LINKS", "0").arg("--version");
    bounded_text(run, command).is_some_and(|output| {
        let mut words = output.split_whitespace();
        words.next() == Some("dots") && words.next() == Some(version)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_redirect_must_name_a_concrete_official_release() {
        assert_eq!(
            release_version(&format!("{RELEASES}/tag/v0.5.2"))
                .unwrap()
                .to_string(),
            "0.5.2"
        );
        assert!(release_version("/EzraCerpac/dots/releases/tag/0.5.2-rc.1").is_some());
        for location in [
            "https://github.com/other/dots/releases/tag/v0.5.2",
            "http://github.com/EzraCerpac/dots/releases/tag/v0.5.2",
            "/EzraCerpac/dots/releases/tag/vlatest",
            "/EzraCerpac/dots/releases/tag/v0.5.2?query",
        ] {
            assert!(release_version(location).is_none(), "{location}");
        }
    }
}
