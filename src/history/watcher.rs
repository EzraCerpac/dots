use std::fs;
use std::io;
use std::path::PathBuf;

use crate::Fail;

use super::native::Native;
use super::{blocked, invalid};

pub(super) fn require_stopped(native: &Native<'_, '_>) -> Result<(), Fail> {
    let unknown = || invalid("could not confirm that the mise history watcher is stopped");
    let running = || {
        blocked(
            "mise history watcher is running; stop it through its service owner before restoring",
        )
    };
    let uid = nix::unistd::geteuid().to_string();
    let mut processes = native.command("pgrep")?;
    processes.args([
        "-u",
        &uid,
        "-f",
        "(^|[ /])mise[[:space:]].*bootstrap[[:space:]]+dotfiles[[:space:]]+watch([[:space:]]|$)",
    ]);
    match native
        .probe(processes, &unknown().diagnostic)
        .map_err(|_| unknown())?
        .status
        .code()
    {
        Some(1) => {}
        Some(0) => return Err(running()),
        _ => return Err(unknown()),
    }

    let home = native.home()?;
    let (dirs, extension) = match std::env::consts::OS {
        "macos" => (vec![home.join("Library/LaunchAgents")], ".plist"),
        "linux" => {
            let mut dirs = vec![home.join(".config/systemd/user")];
            if let Some(config) = native.ctx.env.get("XDG_CONFIG_HOME") {
                let dir = PathBuf::from(config).join("systemd/user");
                if dir != dirs[0] {
                    dirs.push(dir);
                }
            }
            (dirs, ".service")
        }
        _ => return Err(unknown()),
    };
    for dir in dirs {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(unknown()),
        };
        for entry in entries {
            let entry = entry.map_err(|_| unknown())?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !(name.starts_with("dev.mise.")
                && name.contains("history")
                && name.ends_with(extension))
            {
                continue;
            }
            if extension == ".plist" {
                let mut cmd = native.command("launchctl")?;
                cmd.args(["list", name.trim_end_matches(extension)]);
                let result = native
                    .probe(cmd, &unknown().diagnostic)
                    .map_err(|_| unknown())?;
                if result.status.success() {
                    return Err(running());
                }
                if !String::from_utf8_lossy(&result.stderr).contains("Could not find service") {
                    return Err(unknown());
                }
            } else {
                let mut cmd = native.command("systemctl")?;
                cmd.args(["--user", "is-active", name]);
                let result = native
                    .probe(cmd, &unknown().diagnostic)
                    .map_err(|_| unknown())?;
                match String::from_utf8_lossy(&result.stdout).trim() {
                    "inactive" => {}
                    "active" | "activating" | "deactivating" | "reloading" | "maintenance" => {
                        return Err(running());
                    }
                    _ => return Err(unknown()),
                }
            }
        }
    }
    Ok(())
}
