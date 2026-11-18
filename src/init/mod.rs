mod checkout;
mod files;
mod git;
mod remote;

pub use files::TEMPLATES;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::config::{Root, Vars, selection};
use crate::{Args, Ctx, Env, Fail};

use git::{Git, Shape};

pub fn init(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let root = match args.get_one::<OsString>("DIR") {
        Some(dir) => Root::from_dir(&ctx.env, Path::new(dir))?,
        None => Root::resolve(&ctx.env)?,
    };
    let mut env = Env {
        vars: ctx.env.vars.clone(),
        cwd: ctx.env.cwd.clone(),
    };
    env.vars.insert("DOTS_ROOT".into(), root.path().into());
    let target = Ctx {
        root,
        env,
        flags: ctx.flags,
    };
    let root = target.root.path();
    let ancestor = existing_directory(root)?;
    let probe = Git {
        ctx: &target,
        root,
        profile: "",
    };
    let shape = probe.shape(ancestor)?;

    if ancestor != root {
        fs::create_dir_all(root).map_err(|err| file_error(root, err))?;
        println!("created configuration directory {}", root.display());
    } else {
        println!("kept configuration directory {}", root.display());
    }
    if root != ctx.root.path() {
        println!("This directory is outside the active configuration root.");
    }
    files::scaffold(root)?;

    let vars = Vars::read(&target.root)?;
    let result = selection::read_for_init(&target.root);
    let selected = match result {
        Ok(selected) => selected,
        Err(error) if error.status == 2 => {
            repair_commands(root)?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let profile = selected.profile.as_deref().unwrap_or_else(|| {
        vars.profiles
            .iter()
            .find(|p| p.as_str() == "default")
            .unwrap_or(&vars.profiles[0])
    });
    let fallback;
    let machine = match selected.machine_id.as_deref() {
        Some(machine) => machine,
        None => {
            fallback = hostname().as_deref().and_then(machine_name);
            let Some(machine) = fallback.as_deref() else {
                repair_commands(root)?;
                return Err(Fail::usage(
                    "could not derive a machine ID from the hostname",
                ));
            };
            machine
        }
    };

    selection::persist(
        &target.root,
        &target.env,
        selection::Persist {
            profile,
            machine_id: machine,
            origin: None,
        },
    )?;
    println!(
        "{} profile {profile}",
        if selected.profile.is_some() {
            "kept"
        } else {
            "created"
        }
    );
    println!(
        "{} machine ID {machine}",
        if selected.machine_id.is_some() {
            "kept"
        } else {
            "created"
        }
    );

    let git = Git { profile, ..probe };
    let created_seed = !matches!(shape, Shape::Existing);
    if created_seed {
        git.seed(shape)?;
    } else {
        println!("kept existing Git repository");
        checkout::advise(&git)?;
    }
    remote::configure(&git, vars.source_repo.as_ref(), created_seed)?;
    Ok(0)
}

fn existing_directory(path: &Path) -> Result<&Path, Fail> {
    for ancestor in path.ancestors() {
        match ancestor.metadata() {
            Ok(meta) if meta.is_dir() => return Ok(ancestor),
            Ok(_) => {
                return Err(Fail::usage(format!(
                    "{} must be a directory",
                    ancestor.display()
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(file_error(ancestor, err)),
        }
    }
    Err(Fail::usage(
        "configuration directory has no existing parent",
    ))
}

fn hostname() -> Option<Vec<u8>> {
    let mut buffer = [0_u8; 256];
    // The pointer is writable for exactly the supplied size.
    let result = unsafe { nix::libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer.iter().position(|b| *b == 0)?;
    Some(buffer[..end].to_vec())
}

fn machine_name(hostname: &[u8]) -> Option<String> {
    let mut name = String::new();
    let mut separator = false;
    for byte in hostname.iter().copied().take_while(|b| *b != b'.') {
        if byte.is_ascii_alphanumeric() {
            if separator && !name.is_empty() {
                name.push('-');
            }
            name.push(char::from(byte.to_ascii_lowercase()));
            separator = false;
        } else {
            separator = true;
        }
    }
    (!name.is_empty()).then_some(name)
}

fn file_error(path: &Path, err: io::Error) -> Fail {
    Fail::new(1, format!("{}: {err}", path.display()))
}

fn repair_commands(root: &Path) -> Result<(), Fail> {
    println!();
    command_line(
        "DOTS_ROOT=",
        root,
        " dots profile --profile PROFILE --machine-id ID",
    )?;
    command_line("dots init ", root, "")
}

fn command_line(before: &str, root: &Path, after: &str) -> Result<(), Fail> {
    let mut stdout = io::stdout().lock();
    let mut write = || -> io::Result<()> {
        stdout.write_all(before.as_bytes())?;
        stdout.write_all(b"'")?;
        for (index, chunk) in root
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'\'')
            .enumerate()
        {
            if index > 0 {
                stdout.write_all(b"'\\''")?;
            }
            stdout.write_all(chunk)?;
        }
        stdout.write_all(b"'")?;
        writeln!(stdout, "{after}")
    };
    write().map_err(|err| Fail::new(1, format!("could not print init guidance: {err}")))
}

#[cfg(test)]
mod tests {
    use super::machine_name;

    #[test]
    fn normalizes_only_the_first_hostname_label() {
        for (hostname, id) in [
            (&b"-My__Workstation-.example"[..], Some("my-workstation")),
            (&b"localhost."[..], Some("localhost")),
            (&b"._invalid"[..], None),
            (&b"___-_.example"[..], None),
            (&b"\xffa\xff\xffZ"[..], Some("a-z")),
        ] {
            assert_eq!(machine_name(hostname).as_deref(), id);
        }
    }
}
