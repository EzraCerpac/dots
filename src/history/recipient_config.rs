use std::collections::BTreeSet;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use crate::Fail;

use super::{invalid, native::Native};

const KEY: &str = "history.encryption.recipients";
const MISSING_KEY: &[u8] = b"mise ERROR Key not found: history.encryption.recipients in ";

pub(super) fn read(native: &Native<'_, '_>) -> Result<BTreeSet<String>, Fail> {
    let root = native.ctx.root.path();
    let home = native.home()?;
    let env = |key| native.ctx.env.get(key).filter(|value| !value.is_empty());
    let path_env = |name| {
        let path = Path::new(env(name)?);
        Some(match path.strip_prefix("~/") {
            Ok(suffix) => home.join(suffix),
            Err(_) => root.join(path),
        })
    };
    let envs = native.selection()?.active_environments();
    let enabled = ["1", "y", "yes", "true", "on"];
    let scoped = env("MISE_ENV_CONF_D")
        .is_some_and(|value| enabled.iter().any(|flag| value.eq_ignore_ascii_case(flag)));
    let system = if let Some(file) = path_env("MISE_SYSTEM_CONFIG_FILE") {
        vec![file]
    } else {
        let dir = path_env("MISE_SYSTEM_CONFIG_DIR")
            .or_else(|| path_env("MISE_SYSTEM_DIR"))
            .unwrap_or_else(|| PathBuf::from("/etc/mise"));
        files(&dir, &envs, scoped)?
    };
    let mut nonempty = false;
    let mut last = BTreeSet::new();
    for file in system.into_iter().chain(files(root, &envs, scoped)?) {
        if file.extension().is_none_or(|extension| extension != "toml")
            || !optional(fs::metadata(&file), &file)?.is_some_and(|meta| meta.is_file())
        {
            continue;
        }
        let fail = |reason: &str| invalid(format!("{}: {reason}", file.display()));
        let mut cmd = native.mise(&["--no-config", "config", "get", "--file"])?;
        cmd.arg(&file).arg(KEY).envs([
            ("GIT_ALLOW_PROTOCOL", ""),
            ("MISE_FRIENDLY_ERROR", "1"),
            ("NO_COLOR", "1"),
        ]);
        let output = native
            .probe(cmd, "could not run mise config get")
            .map_err(|_| fail("could not run mise config get"))?;
        if !output.status.success() {
            if output.status.code() == Some(1)
                && output.stdout.is_empty()
                && output.stderr.starts_with(MISSING_KEY)
            {
                continue;
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
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
    // This list only informs refresh timing. Restore writes its local recipients.
    Ok(last)
}

fn files(dir: &Path, envs: &str, scoped: bool) -> Result<Vec<PathBuf>, Fail> {
    let conf = dir.join("conf.d");
    let (mut flat, mut folders) = (BTreeSet::new(), BTreeSet::new());
    for entry in optional(fs::read_dir(&conf), &conf)?.into_iter().flatten() {
        let entry = entry.map_err(|err| fs_error(&conf, err))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            folders.insert(path);
        } else if path.extension().is_some_and(|ext| ext == "toml") {
            flat.insert(path);
        }
    }
    let mut files: Vec<_> = flat
        .iter()
        .filter(|path| !scoped || fragment_env(path).is_none())
        .cloned()
        .collect();
    let envs = envs
        .split(',')
        .filter(|name| !name.is_empty() && !name.contains(['/', '\\']));
    for local in [false, true] {
        for env in std::iter::once(None).chain(envs.clone().map(Some)) {
            if let Some(name) = env.filter(|_| scoped) {
                files.extend(
                    flat.iter()
                        .filter(|path| fragment_env(path) == Some((name, local)))
                        .cloned(),
                );
            }
            let mut suffix = env.map_or_else(String::new, |name| format!(".{name}"));
            if local {
                suffix.push_str(".local");
            }
            let mise = format!("mise{suffix}.toml");
            files.extend(folders.iter().map(|folder| folder.join(&mise)));
            files.extend([dir.join(format!("config{suffix}.toml")), dir.join(mise)]);
        }
    }
    let mut seen = BTreeSet::new();
    files.retain(|file| seen.insert(file.clone()));
    Ok(files)
}

fn fragment_env(path: &Path) -> Option<(&str, bool)> {
    let stem = path.file_stem()?.to_str()?;
    let (stem, local) = stem
        .strip_suffix(".local")
        .map_or((stem, false), |stem| (stem, true));
    let (name, env) = stem.split_once('.')?;
    (!name.is_empty() && !env.is_empty()).then_some((env, local))
}

fn optional<T>(result: io::Result<T>, path: &Path) -> Result<Option<T>, Fail> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(fs_error(path, err)),
    }
}

fn fs_error(path: &Path, err: io::Error) -> Fail {
    invalid(format!("{}: {}", path.display(), err.kind()))
}
