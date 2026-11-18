mod version;

pub use version::version;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::config::{Root, selection};
use crate::process::{Cwd, Mise, is_executable, status_code};
use crate::{Args, Ctx, Fail, cli};

pub enum Invocation<'a> {
    Root,
    Profile(&'a Args),
    Mise(&'a [OsString]),
    External { name: &'a str, args: &'a [OsString] },
}

pub fn dispatch(ctx: &Ctx, invocation: Invocation<'_>) -> Result<u8, Fail> {
    match invocation {
        Invocation::Root => {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(ctx.root.path().as_os_str().as_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .map_err(|error| {
                    Fail::new(1, format!("could not write configuration root: {error}"))
                })?;
            Ok(0)
        }
        Invocation::Profile(args) => profile(ctx, args),
        Invocation::Mise(args) => {
            let status = Mise::new(ctx)?
                .command(args)?
                .status()
                .map_err(|error| Fail::new(1, format!("could not run mise: {error}")))?;
            Ok(status_code(status))
        }
        Invocation::External { name, args } => external(ctx, name, args),
    }
}

fn profile(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    if let (Some(profile), Some(machine_id)) = (
        args.get_one::<String>("profile"),
        args.get_one::<String>("machine-id"),
    ) {
        let origin = args.get_one::<String>("origin").map(String::as_str);
        selection::persist(
            &ctx.root,
            &ctx.env,
            selection::Persist {
                profile,
                machine_id,
                origin,
            },
        )?;
        println!("Saved local setup profile {profile} for host {machine_id}.");
    } else {
        let selected = selection::read(&ctx.root, &ctx.env)?;
        let profile = selected.profile.ok_or_else(|| {
            Fail::usage("no profile selected; run dots profile --profile PROFILE --machine-id ID")
        })?;
        println!("{profile}");
    }
    Ok(0)
}

fn external(ctx: &Ctx, name: &str, args: &[OsString]) -> Result<u8, Fail> {
    if !valid_name(name) {
        return Err(Fail::usage(format!(
            "invalid command name '{name}'; use dots help"
        )));
    }
    if name == "internal" {
        return Err(Fail::usage(format!(
            "unknown command '{name}'; use dots help"
        )));
    }
    let path = directory(&ctx.root).join(name);
    if !is_executable(&path) {
        let reason = if path.symlink_metadata().is_ok() {
            format!("; {} is not an executable file", path.display())
        } else {
            String::new()
        };
        return Err(Fail::usage(format!(
            "unknown command '{name}'{reason}; use dots help"
        )));
    }
    let persisted;
    let profile = if let Some(profile) = ctx.env.get("DOTS_PROFILE") {
        profile
    } else {
        persisted = selection::configured_profile(&ctx.root)?;
        persisted.as_deref().unwrap_or_default().as_ref()
    };
    let status = ctx
        .command_with_profile(&path, Cwd::Caller, profile)?
        .args(args)
        .status()
        .map_err(|error| Fail::new(1, format!("could not run {}: {error}", path.display())))?;
    Ok(status_code(status))
}

pub fn list(root: &Root) -> Vec<String> {
    if !root.path().join("config.toml").is_file() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(directory(root)) else {
        return Vec::new();
    };
    let tree = cli::tree();
    let mut names: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            (valid_name(&name) && !is_builtin(&tree, &name) && is_executable(&entry.path()))
                .then_some(name)
        })
        .collect();
    names.sort_unstable();
    names
}

pub(crate) fn is_external(root: &Root, name: &str) -> bool {
    valid_name(name)
        && !is_builtin(&cli::tree(), name)
        && is_executable(&directory(root).join(name))
}

fn is_builtin(tree: &clap::Command, name: &str) -> bool {
    name == "help"
        || name == "internal"
        || tree.get_subcommands().any(|command| {
            command.get_name() == name || command.get_all_aliases().any(|alias| alias == name)
        })
}

fn directory(root: &Root) -> PathBuf {
    root.path().join(Path::new("dots/commands"))
}

pub(crate) fn valid_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
