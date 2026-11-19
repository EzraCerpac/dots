use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::Fail;

use super::{invalid, native::Native};

const KEY: &str = "history.encryption.recipients";

pub(super) fn read(native: &Native<'_, '_>) -> Result<BTreeSet<String>, Fail> {
    let root = native.ctx.root.path();
    let home = native.home()?;
    let env = |key| native.ctx.env.get(key).filter(|value| !value.is_empty());
    let resolve = |path: &Path| match path.strip_prefix("~/") {
        Ok(suffix) => home.join(suffix),
        Err(_) => root.join(path),
    };
    let args = ["--no-config", "config", "get", "--file"];
    let command = native.mise(&args)?;
    let environments = command
        .get_envs()
        .find_map(|(key, value)| (key == "MISE_ENV").then_some(value?.to_str()?))
        .unwrap_or_default();
    let system = match env("MISE_SYSTEM_CONFIG_FILE") {
        Some(path) => vec![resolve(Path::new(path))],
        None => {
            let dir = env("MISE_SYSTEM_CONFIG_DIR")
                .or_else(|| env("MISE_SYSTEM_DIR"))
                .map_or_else(|| PathBuf::from("/etc/mise"), |dir| resolve(Path::new(dir)));
            files(&dir, environments)?
        }
    };
    let mut nonempty = false;
    let mut last = BTreeSet::new();
    for path in system.into_iter().chain(files(root, environments)?) {
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let fail = |reason: &str| {
            invalid(format!(
                "could not read history recipients in {} ({reason})",
                path.display()
            ))
        };
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(fail(&error.kind().to_string())),
        }
        let mut cmd = native.mise(&args)?;
        cmd.arg(&path)
            .arg(KEY)
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("NO_COLOR", "1");
        let output = native
            .probe(cmd, "could not run mise config get")
            .map_err(|_| fail("could not run mise config get"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.code() == Some(1)
                && output.stdout.is_empty()
                && stderr.lines().next().is_some_and(|line| {
                    line.starts_with("mise ERROR Key not found: history.encryption.recipients in ")
                })
            {
                continue;
            }
            let reason = if stderr.contains("TOML parse error") {
                "mise reported a TOML parse error".to_owned()
            } else if stderr.contains("Permission denied") {
                "mise reported permission denied".to_owned()
            } else {
                format!("mise config get failed with {}", output.status)
            };
            return Err(fail(&reason));
        }
        last = serde_json::from_slice(&output.stdout)
            .map_err(|_| fail("mise did not return an array of recipient strings"))?;
        nonempty |= !last.is_empty();
    }
    if !nonempty {
        return Err(invalid(
            "no verified history recipient configuration; configure [history.encryption].recipients and retry dots restore",
        ));
    }
    // Only the last declaration informs refresh timing. Restore writes its own
    // identity and existing local recipients, and mise validates capture policy.
    Ok(last)
}

fn files(dir: &Path, environments: &str) -> Result<Vec<PathBuf>, Fail> {
    let conf_d = dir.join("conf.d");
    let error = |err: std::io::Error| {
        invalid(format!(
            "could not list history config files in {} ({})",
            conf_d.display(),
            err.kind()
        ))
    };
    let mut paths = match fs::read_dir(&conf_d) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()).map_err(error))
            .collect::<Result<Vec<_>, _>>()?,
        Err(err) if err.kind() == ErrorKind::NotFound => vec![],
        Err(err) => return Err(error(err)),
    };
    paths.retain(|path| {
        path.file_name()
            .is_some_and(|name| !name.to_string_lossy().starts_with('.'))
    });
    paths.sort();
    let profiles = environments
        .split(',')
        .filter(|name| !name.is_empty() && !name.contains(['/', '\\']));
    for local in ["", ".local"] {
        let suffixes =
            std::iter::once(String::new()).chain(profiles.clone().map(|name| format!(".{name}")));
        for profile in suffixes {
            paths.extend(
                ["config", "mise"].map(|base| dir.join(format!("{base}{profile}{local}.toml"))),
            );
        }
    }
    Ok(paths)
}
