use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{Exceptions, RepoSetting, Vars, selection};
use crate::config_commands;
use crate::process::{Cwd, Mise, is_executable, mise_binary};
use crate::report::Command;
use crate::run::{Issue, Outcome, Run, Sink};
use crate::{Args, Ctx, Env, Fail, skill};

use super::{packages, read_json, stage, startup_error};

struct Program {
    name: &'static str,
    version_arg: &'static str,
    used_by: &'static str,
}

const PROGRAMS: &[Program] = &[
    Program {
        name: "mise",
        version_arg: "--version",
        used_by: "commands that read the configuration through mise",
    },
    Program {
        name: "git",
        version_arg: "--version",
        used_by: "init, sync, up (sync), backup, restore, bootstrap, publish, add FILE",
    },
    Program {
        name: "gitleaks",
        version_arg: "version",
        used_by: "sync, up (sharing), publish, sync --last (patches)",
    },
    Program {
        name: "age",
        version_arg: "--version",
        used_by: "bundle, bootstrap (bundle), enroll-history, backup, restore",
    },
    Program {
        name: "age-keygen",
        version_arg: "--version",
        used_by: "bundle, bootstrap (bundle), enroll-history, backup, restore",
    },
    Program {
        name: "ssh",
        version_arg: "-V",
        used_by: "remote",
    },
    Program {
        name: "gh",
        version_arg: "--version",
        used_by: "publish, init (remote offer)",
    },
    Program {
        name: "jj",
        version_arg: "--version",
        used_by: "publish",
    },
    Program {
        name: "brew",
        version_arg: "--version",
        used_by: "brew and brew:/brew-cask: declarations (macOS)",
    },
    Program {
        name: "apt",
        version_arg: "--version",
        used_by: "apt: declarations",
    },
    Program {
        name: "dnf",
        version_arg: "--version",
        used_by: "dnf: declarations",
    },
    Program {
        name: "pacman",
        version_arg: "--version",
        used_by: "pacman: declarations",
    },
    Program {
        name: "apk",
        version_arg: "--version",
        used_by: "apk: declarations",
    },
    Program {
        name: "mas",
        version_arg: "version",
        used_by: "mas: declarations",
    },
];

pub fn status(ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    let run = Run::begin(ctx, Sink::Report(Command::Status));
    let mise_version_readable = show_programs(ctx, &run);

    if !ctx.root.path().join("config.toml").is_file() {
        let diagnostic = format!(
            "no configuration at {} (config.toml missing)",
            ctx.root.path().display()
        );
        return Ok(startup_error(
            run,
            Fail::usage(diagnostic),
            "Create a configuration with dots init or set DOTS_ROOT, then rerun dots status.",
        ));
    }
    println!("{}", one_line(&config_commands::version(&ctx.env)?));
    println!("Agent skill links");
    match skill::status(ctx) {
        skill::Status::Disabled => println!("  disabled"),
        skill::Status::Links(links) if links.is_empty() => {
            println!("  no eligible agent directories")
        }
        skill::Status::Links(links) => {
            for (link, status) in links {
                let link = link.display().to_string();
                match status {
                    skill::LinkStatus::Owned(target) => {
                        row(&link, "linked", &target.display().to_string(), None)
                    }
                    skill::LinkStatus::UserProvided => {
                        row(&link, "user-provided", "existing entry", None)
                    }
                    skill::LinkStatus::Missing => row(&link, "missing", "no link", None),
                }
            }
        }
    }

    if !mise_version_readable {
        let error = mise_binary(&ctx.env)
            .err()
            .unwrap_or_else(|| Fail::usage("could not run mise --version"));
        return Ok(startup_error(
            run,
            error,
            "Install a working mise or set DOTS_MISE_BIN, then rerun dots status.",
        ));
    }
    let (vars, selected) = match Vars::read(&ctx.root).and_then(|vars| {
        selection::read(&ctx.root, &ctx.env)
            .map_err(|error| {
                Fail::new(error.status, format!("{error}; check [vars] dots_profiles"))
            })
            .map(|selected| (vars, selected))
    }) {
        Ok(selection) => selection,
        Err(error) => {
            return Ok(startup_error(
                run,
                error,
                "Check dots_profiles and run dots profile --profile PROFILE --machine-id ID, then rerun dots status.",
            ));
        }
    };
    let mise = match Mise::new(ctx) {
        Ok(mise) => mise,
        Err(error) => {
            return Ok(startup_error(
                run,
                error,
                "Check mise and the selected profile, then rerun dots status.",
            ));
        }
    };
    if run.cancelled() {
        return Ok(run.end(130));
    }
    let profile = selected.profile.as_deref().unwrap_or("not selected");
    let machine = selected.machine_id.as_deref().unwrap_or("not set");
    let host = probe_line(ctx, &run, Cwd::Caller, Path::new("hostname"), &[])
        .unwrap_or_else(|| machine.to_owned());
    println!("Host: {}  Profile: {}", one_line(&host), one_line(profile));
    println!("Machine ID: {}", one_line(machine));
    show_source(ctx, &run, vars.source_repo.as_ref());

    let packages = stage(&run, "Packages", "dots status", |_| {
        show_packages(&run, &mise)?;
        Ok(0)
    });
    let tools = stage(&run, "Tools", "dots status", |_| {
        show_tools(&run, &mise)?;
        Ok(0)
    });
    let files = stage(&run, "Dotfiles and history", "dots status", |_| {
        show_files(&run, &mise)?;
        Ok(0)
    });
    let recipes = stage(&run, "Installer exceptions", "dots status", |_| {
        show_exceptions(ctx, selected.profile.as_deref().unwrap_or_default())?;
        Ok(0)
    });
    let result = if run.cancelled() {
        130
    } else if recipes == 2 {
        2
    } else {
        u8::from(
            [packages, tools, files, recipes]
                .iter()
                .any(|status| *status != 0),
        )
    };
    Ok(run.end(result))
}

fn show_programs(ctx: &Ctx, run: &Run<'_>) -> bool {
    println!("Programs");
    let stage = run.start("Programs");
    let mut mise_version_readable = false;
    for program in PROGRAMS {
        if run.cancelled() {
            break;
        }
        let path = if program.name == "mise" {
            mise_binary(&ctx.env).ok()
        } else {
            find_program(&ctx.env, program.name)
        };
        let summary = if let Some(path) = path {
            let version = probe_line(ctx, run, Cwd::Caller, &path, &[program.version_arg]);
            if program.name == "mise" {
                mise_version_readable = version.is_some();
            }
            format!(
                "found, {}",
                version.as_deref().unwrap_or("version unavailable")
            )
        } else {
            "missing".into()
        };
        row(
            program.name,
            &summary,
            &format!("needed by {}", program.used_by),
            None,
        );
    }
    run.finish(stage, Outcome::Completed);
    mise_version_readable
}

fn find_program(env: &Env, program: &str) -> Option<PathBuf> {
    let search = env
        .get("PATH")
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|path| env.cwd().join(path).join(program));
    let fallback = (cfg!(target_os = "macos") && program == "brew").then_some([
        PathBuf::from("/opt/homebrew/bin/brew"),
        PathBuf::from("/usr/local/bin/brew"),
    ]);
    search
        .chain(fallback.into_iter().flatten())
        .find(|path| is_executable(path))
}

fn probe_line(
    ctx: &Ctx,
    run: &Run<'_>,
    cwd: Cwd<'_>,
    program: &Path,
    args: &[&str],
) -> Option<String> {
    let mut command = ctx.command_with_profile(program, cwd, "".as_ref()).ok()?;
    command.args(args).env("HOMEBREW_NO_AUTO_UPDATE", "1");
    let output = run.probe(command).ok()?;
    if !output.status.success() {
        return None;
    }
    [output.stdout, output.stderr].iter().find_map(|bytes| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(one_line)
            .find(|line| !line.is_empty())
    })
}

fn show_source(ctx: &Ctx, run: &Run<'_>, expected: Option<&RepoSetting>) {
    if run.cancelled() {
        return;
    }
    println!("Source");
    let stage = run.start("Source");
    let git = find_program(&ctx.env, "git");
    let git_query = |args: &[&str]| {
        git.as_deref()
            .and_then(|git| probe_line(ctx, run, Cwd::Root, git, args))
    };
    let has_repository = ctx.root.path().join(".git").exists();
    let origin = has_repository
        .then(|| {
            git_query(&[
                "config",
                "--local",
                "--includes",
                "--get",
                "remote.origin.url",
            ])
        })
        .flatten();
    let diagnostic = match (git.is_some(), expected, origin.as_deref()) {
        (false, _, _) => Some(("Git is missing", "Install Git, then rerun dots status.")),
        (_, Some(_), None) => Some((
            "the configured source remote is missing",
            "Check dots_source_repo and git remote, then rerun dots status.",
        )),
        (_, Some(expected), Some(origin)) if !expected.matches(origin) => Some((
            "origin differs from dots_source_repo",
            "Set origin to the configured repository, then rerun dots status.",
        )),
        _ => None,
    };
    if let Some((diagnostic, next)) = diagnostic {
        row("repository", "attention", diagnostic, Some(next));
        run.issue(stage, Issue::new(diagnostic, next));
        run.finish(stage, Outcome::Deferred);
        return;
    }
    let Some(origin) = origin else {
        row("repository", "ok", "local-only", None);
        run.finish(stage, Outcome::Completed);
        return;
    };
    let remote = display_origin(&origin);
    let branch = git_query(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_else(|| "detached HEAD".into());
    let remote_dev = git_query(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "refs/remotes/origin/dev",
    ]);
    let unpushed = if remote_dev.is_some() {
        git_query(&["rev-list", "--count", "refs/remotes/origin/dev..HEAD"])
    } else {
        git_query(&["rev-list", "--count", "HEAD"])
    }
    .and_then(|count| count.parse::<u64>().ok());
    let summary = format!(
        "shared through {remote}; {branch}{}",
        unpushed.map_or_else(String::new, |count| format!(
            "; {count} commits not on origin/dev as of the last fetch"
        ))
    );
    row(
        "repository",
        "ok",
        &summary,
        unpushed
            .is_some_and(|count| count > 0)
            .then_some("Run dots sync."),
    );
    run.finish(stage, Outcome::Completed);
}

fn display_origin(origin: &str) -> String {
    if RepoSetting::parse(origin).is_err() {
        return "origin".into();
    }
    let address = origin.split(['?', '#']).next().unwrap_or_default();
    if let Some((scheme, location)) = address.split_once("://") {
        let (authority, path) = location.split_once('/').unwrap_or((location, ""));
        let host = authority.rsplit('@').next().unwrap_or_default();
        return format!("{scheme}://{host}/{path}");
    }
    address.rsplit('@').next().unwrap_or_default().into()
}

fn show_packages(run: &Run<'_>, mise: &Mise<'_>) -> Result<(), Fail> {
    println!("Packages");
    let scope = packages::Scope::read(run, mise)?;
    let state = scope.status(run, mise, false)?;
    if state.is_empty() {
        println!("  none declared");
    }
    for (manager, records) in state {
        let mut items = BTreeMap::new();
        for package in &records.packages {
            items.insert(&package.package, package);
        }
        for (name, package) in items {
            let label = format!("{manager}:{name}");
            let state = item_state(&package.state);
            let summary = (!package.installed_version.is_empty())
                .then_some(package.installed_version.as_str())
                .or(package.reason.as_deref())
                .or(records.reason.as_deref())
                .unwrap_or(&package.state);
            row(
                &label,
                state,
                summary,
                action_for(state, "Review package status, then run dots status."),
            );
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Tool {
    #[serde(default = "active")]
    active: bool,
    installed: Option<bool>,
    requested_version: Option<String>,
    version: Option<String>,
}

fn active() -> bool {
    true
}

fn show_tools(run: &Run<'_>, mise: &Mise<'_>) -> Result<(), Fail> {
    println!("Tools");
    let command = mise.command_active(["ls", "--current", "--json"])?;
    let tools: BTreeMap<String, Vec<Tool>> = read_json(run, command, "inspect declared tools")?;
    let mut active = BTreeMap::new();
    for (name, records) in tools {
        for tool in records
            .into_iter()
            .filter(|tool| tool.active || tool.installed == Some(false))
        {
            let version = tool
                .requested_version
                .as_deref()
                .or(tool.version.as_deref())
                .unwrap_or("unknown");
            active.insert(format!("{name}@{version}"), tool);
        }
    }
    if active.is_empty() {
        println!("  none declared");
    }
    for (name, tool) in active {
        let state = match tool.installed {
            Some(true) => "ok",
            Some(false) => "missing",
            None => "unknown",
        };
        row(
            &name,
            state,
            tool.version.as_deref().unwrap_or("version unavailable"),
            action_for(state, "Install the selected tools with dots up."),
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct FileState {
    target: Option<String>,
    path: Option<String>,
    state: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Files {
    files: Vec<FileState>,
    edits: Vec<FileState>,
    orphaned: Vec<FileState>,
    history: Option<History>,
}

#[derive(Deserialize)]
struct History {
    enabled: Option<bool>,
    watcher: Option<String>,
    unavailable: Option<String>,
    sync: Option<HistorySync>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct HistorySync {
    conflicts: Vec<serde::de::IgnoredAny>,
    pending_applications: Vec<serde::de::IgnoredAny>,
    last_error: Option<serde::de::IgnoredAny>,
    application_failure: Option<serde::de::IgnoredAny>,
    validation_error: Option<serde::de::IgnoredAny>,
}

fn show_files(run: &Run<'_>, mise: &Mise<'_>) -> Result<(), Fail> {
    println!("Dotfiles");
    let command = mise.command_active(["bootstrap", "dotfiles", "status", "--json"])?;
    let data: Files = read_json(run, command, "inspect dotfiles and history")?;
    let mut files = BTreeMap::new();
    for file in data
        .files
        .into_iter()
        .chain(data.edits)
        .chain(data.orphaned)
    {
        if let Some(path) = file.target.or(file.path) {
            files
                .entry(path)
                .or_insert_with(|| file.state.unwrap_or_else(|| "unknown".into()));
        }
    }
    if files.is_empty() {
        println!("  none declared");
    }
    for (name, summary) in files {
        let state = item_state(&summary);
        row(
            &name,
            state,
            &summary,
            action_for(state, "Review the declaration and run dots status again."),
        );
    }

    println!("History");
    let Some(history) = data.history else {
        println!("  unavailable");
        return Ok(());
    };
    let (state, summary) = if history.unavailable.is_some() {
        ("attention", "unavailable")
    } else {
        match history.enabled {
            Some(true) => ("ok", "enabled"),
            Some(false) => ("skipped", "disabled"),
            None => ("unknown", "unavailable"),
        }
    };
    row(
        "settings history",
        state,
        summary,
        action_for(state, "Review history status before running dots backup."),
    );
    if let Some(watcher) = history.watcher {
        let state = match watcher.as_str() {
            "watching" | "service-watching" | "running" => "ok",
            "not-watching" | "service-not-watching" | "degraded" => "attention",
            "not-declared" => "skipped",
            _ => "unknown",
        };
        row(
            "watcher",
            state,
            &watcher,
            action_for(
                state,
                "Review the watcher before relying on it to save changes.",
            ),
        );
    }
    if let Some(sync) = history.sync {
        let problems = sync.last_error.is_some()
            || sync.application_failure.is_some()
            || sync.validation_error.is_some()
            || !sync.conflicts.is_empty()
            || !sync.pending_applications.is_empty();
        let state = if problems { "attention" } else { "ok" };
        let counts = format!(
            "{} conflicts, {} pending applications",
            sync.conflicts.len(),
            sync.pending_applications.len()
        );
        row(
            "synchronization",
            state,
            &counts,
            action_for(
                state,
                "Review history status before the next synchronization.",
            ),
        );
    }
    Ok(())
}

fn show_exceptions(ctx: &Ctx, profile: &str) -> Result<(), Fail> {
    println!("Installer exceptions");
    let manifest = Exceptions::read(&ctx.root)?;
    let mut rows = BTreeMap::new();
    for recipe in manifest.selected(profile)? {
        rows.entry(recipe.name.as_str())
            .and_modify(|install_only| *install_only |= recipe.install_only)
            .or_insert(recipe.install_only);
    }
    if rows.is_empty() {
        println!("  none declared");
    }
    for (name, install_only) in rows {
        row(
            name,
            "ok",
            if install_only {
                "ready, install-only"
            } else {
                "ready"
            },
            None,
        );
    }
    Ok(())
}

fn item_state(value: &str) -> &'static str {
    match value {
        "applied" | "tracked" | "present" | "installed" | "ok" | "ready" | "satisfied" | "set"
        | "current" | "absent" => "ok",
        "missing" | "source_missing" | "source-missing" | "not_installed" | "not-installed" => {
            "missing"
        }
        "differs" | "drift" | "orphaned" | "orphaned_changed" | "would_remove" => "drift",
        "error" | "failed" => "failed",
        "skipped" => "skipped",
        "attention" => "attention",
        _ => "unknown",
    }
}

fn action_for<'a>(state: &str, action: &'a str) -> Option<&'a str> {
    matches!(state, "missing" | "drift" | "attention" | "failed").then_some(action)
}

fn row(name: &str, status: &str, summary: &str, action: Option<&str>) {
    println!(
        "  {} — {} — {}",
        one_line(name),
        one_line(status),
        one_line(summary)
    );
    if let Some(action) = action {
        println!("    {}", one_line(action));
    }
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_display_never_includes_url_credentials_or_query() {
        assert_eq!(
            display_origin("https://token@example.invalid/a.git"),
            "origin"
        );
        assert_eq!(
            display_origin("ssh://name@example.invalid/a.git?view=1"),
            "ssh://example.invalid/a.git"
        );
        assert_eq!(
            display_origin("git@example.invalid:a.git"),
            "example.invalid:a.git"
        );
    }

    #[test]
    fn status_fields_fit_on_one_line() {
        assert_eq!(
            one_line(" tool\titem\u{1b}\nversion "),
            "tool item  version"
        );
    }
}
