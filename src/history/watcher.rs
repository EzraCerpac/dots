use std::fs::{self, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use plist::Value;

use crate::Fail;

use super::native::{Native, read_native_file};
use super::{blocked, invalid};

// Native bootstrap installs its built-in history service as `mise dot watch`.
// Standalone mise also accepts the long forms and the bootstrap alias.
const WATCH_PROCESS: &str = r"^([^[:space:]]*/)?mise([.-][^[:space:]/]+)?[[:space:]]+([^[:space:]]+[[:space:]]+)*((bootstrap|bs)[[:space:]]+)?(dot|dotfiles)([[:space:]]+-[^[:space:]]+)*[[:space:]]+watch([[:space:]]|$)";

pub(super) fn require_stopped(native: &Native<'_, '_>) -> Result<(), Fail> {
    let unknown = || invalid("could not confirm that the mise history watcher is stopped");
    let running = || {
        blocked(
            "mise history watcher is running; stop it through its service owner before restoring",
        )
    };
    // The store lock identifies watchers even when mise was renamed. Mise
    // takes an exclusive flock, so a shared try-lock suffices to observe it.
    for (history, _) in native.history_dirs().map_err(|_| unknown())? {
        let file = match read_native_file(&history.join("watch.lock")) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(unknown()),
        };
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(running()),
            Err(TryLockError::Error(_)) => return Err(unknown()),
        }
    }

    let uid = nix::unistd::geteuid().to_string();
    let mut processes = native.command("pgrep")?;
    processes.args(["-u", &uid, "-f", WATCH_PROCESS]);
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
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(extension) {
                continue;
            }
            // Only regular definitions can describe a service. A dangling
            // or unreadable mise definition leaves its status unknown.
            let managed = name.starts_with("dev.mise.");
            if !fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                if managed {
                    return Err(unknown());
                }
                continue;
            }
            if extension == ".plist" {
                let (label, watches) = match plist_service(&path) {
                    Some(service) => service,
                    None if managed => return Err(unknown()),
                    None => continue,
                };
                if !watches && !(managed && name.contains("history")) {
                    continue;
                }
                let mut cmd = native.command("launchctl")?;
                cmd.args(["list", &label]);
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
                let definition = fs::read_to_string(&path).map_err(|_| unknown())?;
                let watches = match unit_watches(&definition) {
                    Some(watches) => watches,
                    None if managed => return Err(unknown()),
                    None => continue,
                };
                if !watches && !(managed && name.contains("history")) {
                    continue;
                }
                let mut cmd = native.command("systemctl")?;
                cmd.args(["--user", "is-active", "--", name]);
                let result = native
                    .probe(cmd, &unknown().diagnostic)
                    .map_err(|_| unknown())?;
                match (
                    result.status.code(),
                    String::from_utf8_lossy(&result.stdout).trim(),
                ) {
                    (Some(3), "inactive") => {}
                    (_, "active" | "activating" | "deactivating" | "reloading" | "maintenance") => {
                        return Err(running());
                    }
                    _ => return Err(unknown()),
                }
            }
        }
    }
    Ok(())
}

fn watch_words<'a>(words: impl IntoIterator<Item = &'a str>) -> bool {
    let mut seen_mise = false;
    let mut dotfiles = false;
    for word in words {
        let word = word.trim_matches(['\'', '"', ';']);
        if dotfiles {
            if word == "watch" {
                return true;
            }
            if word.starts_with('-') {
                continue;
            }
        }
        dotfiles = seen_mise && matches!(word, "dot" | "dotfiles");
        let executable = word.rsplit('/').next().unwrap_or_default();
        seen_mise |= executable == "mise"
            || executable.starts_with("mise-")
            || executable.starts_with("mise.");
    }
    false
}

fn plist_service(path: &Path) -> Option<(String, bool)> {
    let plist = Value::from_file(path).ok()?;
    let service = plist.as_dictionary()?;
    let label = service.get("Label")?.as_string()?;
    if label.is_empty() {
        return None;
    }
    let mut words = Vec::new();
    if let Some(program) = service.get("Program") {
        words.push(program.as_string()?);
    }
    if let Some(arguments) = service.get("ProgramArguments") {
        for argument in arguments.as_array()? {
            words.push(argument.as_string()?);
        }
    }
    if words.is_empty() {
        return None;
    }
    // Direct launchd argv may contain spaces in the executable path. Scan
    // individual words first, then check command strings used by a wrapper.
    let watches = watch_words(words.iter().copied())
        || watch_words(words.iter().flat_map(|word| word.split_ascii_whitespace()));
    Some((label.to_owned(), watches))
}

fn unit_watches(unit: &str) -> Option<bool> {
    let unit = unit.replace("\\\n", " ");
    let mut commands = unit
        .lines()
        .filter_map(|line| line.trim().split_once('='))
        .filter(|(key, command)| {
            matches!(
                key.trim_end(),
                "ExecStart" | "ExecStartPre" | "ExecStartPost"
            ) && !command.trim().is_empty()
        })
        .peekable();
    commands.peek()?;
    Some(commands.any(|(_, command)| watch_words(command.split_ascii_whitespace())))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::history::native::tests::in_world;

    fn script(home: &Path, name: &str, body: &str) {
        let path = home.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn matches_mise_watch_aliases_in_process_argv() {
        in_world(|native, _| {
            // Test the POSIX ERE used by pgrep without starting a watcher.
            let mut grep = native.command("grep").unwrap();
            grep.args(["-E", WATCH_PROCESS]);
            let samples = native.home().unwrap().join("commands");
            fs::write(
                &samples,
                "/bin/mise dot watch\n/bin/mise -E host-fixture dotfiles watch --once\n/bin/mise bs dotfiles watch\n/bin/mise bootstrap dotfiles watch\n/bin/mise dot --local watch\n/bin/mise dot --local status\n",
            ).unwrap();
            grep.arg(samples);
            let matches = native.text(grep, "could not match fixture argv").unwrap();
            assert_eq!(matches.lines().count(), 5);
            assert!(!matches.contains("status"));
        });
    }

    #[test]
    fn checks_custom_service_by_its_command_and_fails_closed() {
        in_world(|native, _| {
            let home = native.home().unwrap();
            script(home, "pgrep", "exit 1");
            let (manager, unavailable, inactive) = match std::env::consts::OS {
                "macos" => {
                    let dir = home.join("Library/LaunchAgents");
                    fs::create_dir_all(&dir).unwrap();
                    let mut definition = plist::Dictionary::new();
                    definition.insert("Label".into(), "dev.mise.custom-agent".into());
                    definition.insert(
                        "ProgramArguments".into(),
                        Value::Array(vec![
                            "/tmp/a space/mise".into(),
                            "dot".into(),
                            "--local".into(),
                            "watch".into(),
                        ]),
                    );
                    Value::Dictionary(definition)
                        .to_file_xml(dir.join("dev.mise.custom-agent.plist"))
                        .unwrap();
                    (
                        "launchctl",
                        "exit 99",
                        "printf 'Could not find service\n' >&2; exit 113",
                    )
                }
                "linux" => {
                    let dir = home.join(".config/systemd/user");
                    fs::create_dir_all(&dir).unwrap();
                    fs::write(
                        dir.join("dev.mise.custom-agent.service"),
                        "[Service]\nExecStart=\"/tmp/a space/mise\" dot --local watch\n",
                    )
                    .unwrap();
                    ("systemctl", "exit 99", "printf 'inactive\n'; exit 3")
                }
                _ => unreachable!(),
            };
            script(home, manager, "printf 'active\n'; exit 0");
            assert!(
                require_stopped(native)
                    .unwrap_err()
                    .diagnostic
                    .contains("is running")
            );
            script(home, manager, unavailable);
            assert!(
                require_stopped(native)
                    .unwrap_err()
                    .diagnostic
                    .contains("could not confirm")
            );
            script(home, manager, inactive);
            require_stopped(native).unwrap();
        });
    }

    #[test]
    fn observes_held_native_watch_locks_without_creating_files_or_inspecting_processes_first() {
        in_world(|native, _| {
            let home = native.home().unwrap();
            script(home, "pgrep", "exit 1");
            let shared = native.store().unwrap().parent().unwrap().to_path_buf();
            let local = shared.with_file_name("history-local").join("history");
            require_stopped(native).unwrap();
            assert!(!shared.exists());
            assert!(!local.exists());

            for dir in [shared, local] {
                fs::create_dir_all(&dir).unwrap();
                let path = dir.join("watch.lock");
                let holder = fs::File::create(&path).unwrap();
                require_stopped(native).unwrap();
                holder.try_lock().unwrap();
                script(home, "pgrep", "exit 99");
                assert!(
                    require_stopped(native)
                        .unwrap_err()
                        .diagnostic
                        .contains("is running")
                );
                drop(holder);
                script(home, "pgrep", "exit 1");
                require_stopped(native).unwrap();
                assert!(path.is_file());

                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
                assert!(
                    require_stopped(native)
                        .unwrap_err()
                        .diagnostic
                        .contains("could not confirm")
                );
                fs::remove_dir(&path).unwrap();
            }
        });
    }
}
