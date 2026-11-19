use std::collections::BTreeSet;
use std::process::{Command, Output};

use nix::unistd::geteuid;
use serde_json::Value;

use crate::process::Cwd;
use crate::run::Run;
use crate::{Ctx, Fail};

const MANAGERS: &[&str] = &["brew", "brew-cask", "apt", "dnf", "pacman", "apk", "mas"];

pub fn add_request(request: &str) -> Result<Option<(String, Option<String>)>, Fail> {
    let Some((manager, specification)) = request.split_once(':') else {
        return Ok(None);
    };
    if !MANAGERS.contains(&manager) {
        return Ok(None);
    }
    let (name, version) = if matches!(manager, "brew" | "brew-cask" | "mas") {
        (specification, None)
    } else {
        specification
            .split_once('@')
            .map_or((specification, None), |(name, version)| {
                (name, Some(version))
            })
    };
    if name.is_empty()
        || version
            .is_some_and(|version| version.is_empty() || version.contains(char::is_whitespace))
    {
        return Err(Fail::usage(format!("invalid {manager} package request")));
    }
    Ok(Some((
        format!("{manager}:{name}"),
        version.map(str::to_owned),
    )))
}

pub fn split(key: &str) -> Result<(&str, &str), Fail> {
    key.split_once(':')
        .filter(|(manager, _)| MANAGERS.contains(manager))
        .ok_or_else(|| Fail::usage("invalid native package declaration"))
}

pub fn validate_name(manager: &str, name: &str) -> Result<(), Fail> {
    let valid = name
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name.bytes().all(|byte| match manager {
            "mas" => byte.is_ascii_digit(),
            "brew" | "brew-cask" => byte.is_ascii_alphanumeric() || b"_.+@/-".contains(&byte),
            _ => byte.is_ascii_alphanumeric() || b"_.+:-".contains(&byte),
        });
    if !valid
        || name
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(Fail::usage(format!(
            "invalid {manager} package name {name:?}"
        )));
    }
    Ok(())
}

fn command(ctx: &Ctx, argv: &[&str], privileged: bool) -> Result<Command, Fail> {
    let mut arguments = argv.iter();
    let program = if privileged && !geteuid().is_root() {
        "sudo"
    } else {
        arguments.next().expect("program name")
    };
    let mut command = ctx.command(program, Cwd::Root)?;
    command
        .args(arguments)
        .env("LC_ALL", "C")
        .env("HOMEBREW_NO_AUTOREMOVE", "1")
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_COLOR", "1");
    Ok(command)
}

fn probe(ctx: &Ctx, run: &Run<'_>, command_line: &[&str], allowed: &[i32]) -> Result<Output, Fail> {
    let program = command_line[0];
    let result = run
        .probe(command(ctx, command_line, false)?)
        .map_err(|error| {
            Fail::new(
                1,
                format!("{program} could not verify safe removal: {error}"),
            )
        })?;
    if result
        .status
        .code()
        .is_none_or(|status| !allowed.contains(&status))
    {
        return Err(Fail::new(
            1,
            format!(
                "{program} could not verify safe removal ({})",
                result.status
            ),
        ));
    }
    Ok(result)
}

fn query(ctx: &Ctx, run: &Run<'_>, command: &[&str]) -> Result<String, Fail> {
    String::from_utf8(probe(ctx, run, command, &[0])?.stdout).map_err(|_| {
        Fail::new(
            1,
            format!("{} returned unreadable package information", command[0]),
        )
    })
}

pub fn plan(ctx: &Ctx, run: &Run<'_>, key: &str) -> Result<Option<Command>, Fail> {
    let (manager, name) = split(key)?;
    validate_name(manager, name)?;
    if matches!(manager, "brew" | "brew-cask") {
        return brew_plan(ctx, run, manager, name);
    }
    if manager == "apt" {
        return apt_plan(ctx, run, name);
    }
    let inventory = match manager {
        "dnf" => query(ctx, run, &["rpm", "-qa", "--qf", "%{NAME}\\n"])?,
        "pacman" => query(ctx, run, &["pacman", "-Qq"])?,
        "apk" => query(ctx, run, &["apk", "info"])?,
        "mas" => query(ctx, run, &["mas", "list"])?,
        _ => unreachable!("supported manager"),
    };
    if !inventory
        .lines()
        .any(|line| line.split_whitespace().next() == Some(name))
    {
        return Ok(None);
    }
    let command = match manager {
        "dnf" => dnf_plan(ctx, run, name)?,
        "pacman" => {
            let preview = query(ctx, run, &["pacman", "-R", "--print-format", "%n", name])?;
            only_target(preview.lines(), name)?;
            command(ctx, &["pacman", "-R", "--noconfirm", name], true)?
        }
        "apk" => {
            let preview = query(ctx, run, &["apk", "del", "--simulate", name])?;
            let mut removals = Vec::new();
            for line in preview.lines().filter(|line| line.starts_with('(')) {
                let (_, detail) = line.split_once(')').ok_or_else(|| unsafe_removal(name))?;
                let mut columns = detail.split_whitespace();
                if columns.next() != Some("Purging") {
                    return Err(unsafe_removal(name));
                }
                removals.push(columns.next().ok_or_else(|| unsafe_removal(name))?);
            }
            only_target(removals.into_iter(), name)?;
            command(ctx, &["apk", "del", name], true)?
        }
        "mas" => command(ctx, &["mas", "uninstall", name], false)?,
        _ => unreachable!("supported manager"),
    };
    Ok(Some(command))
}

fn unsafe_removal(name: &str) -> Fail {
    Fail::new(
        1,
        format!("cannot verify a removal of only {name}; check installed dependents"),
    )
}

fn only_target<'a>(mut names: impl Iterator<Item = &'a str>, name: &str) -> Result<(), Fail> {
    if names.next() != Some(name) || names.next().is_some() {
        return Err(unsafe_removal(name));
    }
    Ok(())
}

fn dnf_plan(ctx: &Ctx, run: &Run<'_>, name: &str) -> Result<Command, Fail> {
    let options = "--setopt=clean_requirements_on_remove=False";
    probe(ctx, run, &["rpm", "-e", "--test", name], &[0])?;
    let preview = probe(
        ctx,
        run,
        &["dnf", options, "--assumeno", "remove", name],
        &[0, 1],
    )?;
    let stdout = String::from_utf8_lossy(&preview.stdout);
    let stderr = String::from_utf8_lossy(&preview.stderr);
    let mut removals = Vec::new();
    let mut summaries = Vec::new();
    for line in stdout.lines().chain(stderr.lines()).map(str::trim) {
        if [
            "Installing",
            "Upgrading",
            "Downgrading",
            "Reinstalling",
            "Removing dependent",
            "Removing unused",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            return Err(unsafe_removal(name));
        }
        let columns: Vec<_> = line.split_whitespace().collect();
        match columns.as_slice() {
            [
                "Remove" | "Removing:",
                count,
                "Package" | "Packages" | "package" | "packages",
            ] => summaries.push(*count),
            [package, arch, _, _, _, ..]
                if matches!(
                    *arch,
                    "noarch" | "x86_64" | "aarch64" | "i386" | "i486" | "i586" | "i686"
                ) || ["arm", "ppc", "s390"]
                    .iter()
                    .any(|prefix| arch.starts_with(prefix)) =>
            {
                removals.push(*package);
            }
            _ => (),
        }
    }
    only_target(removals.into_iter(), name)?;
    if summaries != ["1"] {
        return Err(unsafe_removal(name));
    }
    command(ctx, &["dnf", options, "--assumeyes", "remove", name], true)
}

fn apt_names(ctx: &Ctx, run: &Run<'_>) -> Result<BTreeSet<String>, Fail> {
    let output = query(
        ctx,
        run,
        &[
            "dpkg-query",
            "-W",
            "-f=${Package}:${Architecture} ${db:Status-Status}\\n",
        ],
    )?;
    let mut names = BTreeSet::new();
    for line in output.lines() {
        let Some((name, status)) = line.split_once(' ') else {
            return Err(Fail::new(
                1,
                "dpkg-query returned invalid package information",
            ));
        };
        if status == "installed" {
            if !name.contains(':') {
                return Err(Fail::new(
                    1,
                    "dpkg-query returned an unqualified package identity",
                ));
            }
            names.insert(name.to_owned());
        }
    }
    Ok(names)
}

fn apt_base(name: &str) -> &str {
    name.split_once(':').map_or(name, |(name, _)| name)
}

fn apt_selector(name: &str) -> bool {
    !name.contains(':') || name.ends_with(":any") || name.ends_with(":native")
}

fn apt_preview(ctx: &Ctx, run: &Run<'_>, name: &str) -> Result<String, Fail> {
    query(
        ctx,
        run,
        &["apt-get", "--simulate", "--no-auto-remove", "remove", name],
    )
}

fn apt_qualified(
    ctx: &Ctx,
    run: &Run<'_>,
    name: &str,
    names: &BTreeSet<String>,
) -> Result<String, Fail> {
    if name.contains(':') {
        return Ok(name.to_owned());
    }
    let mut installed = names.iter().filter(|installed| apt_base(installed) == name);
    if let Some(candidate) = installed.next()
        && installed.next().is_none()
    {
        return Ok(candidate.clone());
    }
    let architecture = query(ctx, run, &["dpkg", "--print-architecture"])?;
    let architecture = architecture.trim();
    if architecture.is_empty()
        || !architecture
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(Fail::new(1, "cannot verify native dpkg architecture"));
    }
    Ok(format!("{name}:{architecture}"))
}

fn apt_removals(preview: &str) -> impl Iterator<Item = &str> {
    preview
        .lines()
        .filter_map(|line| line.strip_prefix("Remv "))
        .filter_map(|line| line.split_whitespace().next())
}

fn apt_plan(ctx: &Ctx, run: &Run<'_>, name: &str) -> Result<Option<Command>, Fail> {
    let installed = apt_names(ctx, run)?;
    if !installed
        .iter()
        .any(|item| item == name || (apt_selector(name) && apt_base(item) == apt_base(name)))
    {
        return Ok(None);
    }
    let preview = apt_preview(ctx, run, name)?;
    if preview
        .lines()
        .any(|line| line.starts_with("Inst ") || line.starts_with("Conf "))
    {
        return Err(unsafe_removal(name));
    }
    let mut removals = apt_removals(&preview);
    let Some(removed) = removals.next() else {
        return if apt_selector(name) && name.contains(':') {
            Ok(None)
        } else {
            Err(unsafe_removal(name))
        };
    };
    if apt_base(removed) != apt_base(name) || removals.next().is_some() {
        return Err(unsafe_removal(name));
    }
    let chosen = apt_qualified(ctx, run, removed, &installed)?;
    if !installed.contains(&chosen) || (!apt_selector(name) && chosen != name) {
        return Err(unsafe_removal(name));
    }
    // APT's solver may choose different removals after the preview. dpkg
    // checks dependencies while processing only the named package.
    let command = command(
        ctx,
        &[
            "dpkg",
            "--no-force-depends",
            "--no-force-depends-version",
            "--remove",
            &chosen,
        ],
        true,
    )?;
    Ok(Some(command))
}

fn brew_flag(manager: &str) -> &str {
    if manager == "brew" {
        "--formula"
    } else {
        "--cask"
    }
}

fn brew_inventory(ctx: &Ctx, run: &Run<'_>, manager: &str) -> Result<String, Fail> {
    query(
        ctx,
        run,
        &["brew", "list", brew_flag(manager), "--full-name", "-1"],
    )
}

fn short(name: &str) -> &str {
    name.rsplit('/').next().expect("one or more components")
}

fn brew_identity<'a>(
    manager: &str,
    name: &str,
    listings: &'a str,
) -> Result<Option<&'a str>, Fail> {
    let prefix = if manager == "brew" {
        "homebrew/core/"
    } else {
        "homebrew/cask/"
    };
    let mut matches = listings.lines().filter(|item| {
        *item == name
            || (!name.contains('/') && short(item) == name)
            || item.strip_prefix(prefix).unwrap_or(item)
                == name.strip_prefix(prefix).unwrap_or(name)
    });
    let found = matches.next();
    if matches.next().is_some() {
        return Err(Fail::new(
            1,
            format!("ambiguous installed Homebrew package {name}"),
        ));
    }
    Ok(found)
}

fn brew_plan(ctx: &Ctx, run: &Run<'_>, manager: &str, name: &str) -> Result<Option<Command>, Fail> {
    let listings = brew_inventory(ctx, run, manager)?;
    let Some(installed) = brew_identity(manager, name, &listings)? else {
        if manager == "brew-cask" {
            verify_absent_cask(ctx, run, name)?;
        }
        return Ok(None);
    };
    if manager == "brew" {
        let dependents = query(
            ctx,
            run,
            &["brew", "uses", "--installed", "--recursive", installed],
        )?;
        if !dependents.trim().is_empty() {
            return Err(Fail::new(
                1,
                format!(
                    "cannot remove {installed}; installed dependents: {}",
                    dependents.lines().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
    }
    command(
        ctx,
        &["brew", "uninstall", brew_flag(manager), installed],
        false,
    )
    .map(Some)
}

fn verify_absent_cask(ctx: &Ctx, run: &Run<'_>, name: &str) -> Result<(), Fail> {
    let error = || {
        Fail::new(
            1,
            format!("cannot verify unregistered cask {name} is absent; check its uninstall recipe"),
        )
    };
    let raw = query(ctx, run, &["brew", "info", "--json=v2", "--cask", name])?;
    let parsed: Value = serde_json::from_str(&raw).map_err(|_| error())?;
    let casks = parsed["casks"].as_array().ok_or_else(error)?;
    let [cask] = casks.as_slice() else {
        return Err(error());
    };
    let apps = cask["artifacts"]
        .as_array()
        .filter(|apps| !apps.is_empty())
        .ok_or_else(error)?;
    for artifact in apps {
        let record = artifact.as_object().ok_or_else(error)?;
        if record.keys().any(|key| key != "app" && key != "target") {
            return Err(error());
        }
        let spec = artifact["app"].as_array().ok_or_else(error)?;
        let source = spec.first().and_then(Value::as_str).ok_or_else(error)?;
        let leaf = spec
            .iter()
            .find_map(|option| option.get("target").and_then(Value::as_str))
            .or_else(|| std::path::Path::new(source).file_name()?.to_str())
            .ok_or_else(error)?;
        let paths = [
            std::path::Path::new("/Applications").join(leaf),
            ctx.env
                .home()
                .ok_or_else(error)?
                .join("Applications")
                .join(leaf),
        ];
        let target = artifact
            .get("target")
            .and_then(Value::as_str)
            .map(|path| super::absolute_request(ctx, path.as_ref()))
            .transpose()?;
        for path in paths.into_iter().chain(target) {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                _ => return Err(error()),
            }
        }
    }
    Ok(())
}

pub fn shares(ctx: &Ctx, run: &Run<'_>, key: &str, other: &str) -> Result<bool, Fail> {
    let (manager, name) = split(key)?;
    let (other_manager, other) = split(other)?;
    if manager != other_manager {
        return Ok(false);
    }
    if name == other {
        return Ok(true);
    }
    if matches!(manager, "brew" | "brew-cask")
        && short(name) == short(other)
        && (!name.contains('/') || !other.contains('/'))
    {
        let listings = brew_inventory(ctx, run, manager)?;
        let left = brew_identity(manager, name, &listings)?;
        return Ok(left.is_some() && left == brew_identity(manager, other, &listings)?);
    }
    if manager == "apt"
        && apt_base(name) == apt_base(other)
        && (apt_selector(name) || apt_selector(other))
    {
        let installed = apt_names(ctx, run)?;
        let identity = |name: &str| -> Result<Option<String>, Fail> {
            if !installed
                .iter()
                .any(|installed| apt_base(installed) == apt_base(name))
            {
                return Ok(None);
            }

            if !apt_selector(name) {
                return Ok(installed.contains(name).then(|| name.to_owned()));
            }
            let preview = apt_preview(ctx, run, name)?;
            let mut matches =
                apt_removals(&preview).filter(|selected| apt_base(selected) == apt_base(name));
            if let Some(selected) = matches.next()
                && matches.next().is_none()
            {
                let qualified = apt_qualified(ctx, run, selected, &installed)?;
                if installed.contains(&qualified) {
                    return Ok(Some(qualified));
                }
            }
            Err(unsafe_removal(name))
        };
        let left = identity(name)?;
        return Ok(left.is_some() && left == identity(other)?);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_version_separator_depends_on_the_manager() {
        for manager in ["brew", "brew-cask", "mas"] {
            let request = format!("{manager}:test@7");
            assert_eq!(add_request(&request).unwrap(), Some((request, None)));
        }
        for manager in ["apt", "dnf", "pacman", "apk"] {
            let key = format!("{manager}:test");
            assert_eq!(
                add_request(&format!("{key}@7")).unwrap(),
                Some((key, Some("7".into())))
            );
        }
        assert_eq!(add_request("npm:@scope/tool@7").unwrap(), None);
    }
}
