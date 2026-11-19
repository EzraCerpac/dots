use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use toml_edit::{DocumentMut, Item};

use crate::{Fail, config::passive};

use super::native::Native;
use super::{invalid, keys};

const DIAGNOSTIC: &str = "no verified history recipient configuration; configure [history.encryption].recipients and retry dots restore";

#[derive(Deserialize)]
struct Document {
    history: Option<History>,
}

#[derive(Deserialize)]
struct History {
    encryption: Option<Encryption>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Encryption {
    recipients: Vec<String>,
}

#[derive(Default, Deserialize)]
struct Early {
    env: Option<Vec<String>>,
    #[serde(default)]
    env_sets: BTreeMap<String, Vec<String>>,
    env_conf_d: Option<bool>,
    auto_env: Option<bool>,
    ignored_config_paths: Option<Vec<String>>,
}

// Native history reads system, then global config directly, without project
// configs or the separate global override tier. "config get" reads one file.
pub(super) fn read(native: &Native<'_, '_>) -> Result<BTreeSet<String>, Fail> {
    let cmd = native.mise(&[])?;
    let home = native.home()?;
    let root = native.ctx.root.path();
    let var = |name| {
        cmd.get_envs()
            .find_map(|(key, value)| (key == name).then_some(value).flatten())
    };
    let system = var("MISE_SYSTEM_CONFIG_DIR")
        .filter(|path| !path.is_empty())
        .or_else(|| var("MISE_SYSTEM_DIR").filter(|path| !path.is_empty()))
        .map_or_else(
            || PathBuf::from("/etc/mise"),
            |path| resolve(home, root, path),
        );
    let no_config = setting(var("MISE_NO_CONFIG")) == Some(true);
    let mut early = Early::default();
    let mut ignored = Vec::new();
    for path in [
        system.join("miserc.toml"),
        root.join("miserc.toml"),
        root.join("miserc.local.toml"),
    ]
    .into_iter()
    .filter(|_| !no_config)
    {
        let layer = read_early(&path)?;
        early.env_conf_d = layer.env_conf_d.or(early.env_conf_d);
        early.auto_env = layer.auto_env.or(early.auto_env);
        early.env = layer.env.or(early.env);
        early.env_sets.extend(layer.env_sets);
        if let Some(paths) = layer.ignored_config_paths {
            let dir = path.parent().expect("miserc directory");
            ignored = paths
                .iter()
                .map(|path| resolve(home, dir, path.as_ref()))
                .collect();
        }
    }
    if let Some(paths) = var("MISE_IGNORED_CONFIG_PATHS") {
        ignored = env::split_paths(paths)
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| resolve(home, root, path.as_os_str()))
            .collect();
    }
    let env_conf_d = setting(var("MISE_ENV_CONF_D"))
        .or(early.env_conf_d)
        .unwrap_or(false);
    if !no_config {
        merge_project_selectors(&mut early, root, home, var("MISE_CEILING_PATHS"))?;
    }
    let environments = environments(
        var("MISE_ENV").and_then(OsStr::to_str).unwrap_or_default(),
        &early,
        setting(var("MISE_AUTO_ENV"))
            .or(early.auto_env)
            .unwrap_or(false),
    );
    let system_files = var("MISE_SYSTEM_CONFIG_FILE")
        .filter(|path| !path.is_empty())
        .map_or_else(
            || layers(&system, &environments, env_conf_d),
            |path| vec![resolve(home, root, path)],
        );
    let files = system_files
        .into_iter()
        .chain(layers(root, &environments, env_conf_d));
    let global_root = var("MISE_GLOBAL_CONFIG_ROOT")
        .filter(|path| !path.is_empty())
        .map_or_else(|| home.to_owned(), |path| resolve(home, root, path));
    let ignored = IgnorePaths::new(ignored);
    let mut recipients = BTreeSet::new();
    for path in files.filter(|path| path.extension() == Some(OsStr::new("toml")) && path.is_file())
    {
        let doc: Document = read_toml(&path)?;
        if let Some(encryption) = doc.history.and_then(|history| history.encryption) {
            let trust_root = match path.parent() {
                Some(dir) if is_folder_fragment(&path, dir) => dir,
                _ => &global_root,
            };
            if encryption.recipients.is_empty() || ignored.matches(&path) {
                return Err(invalid(DIAGNOSTIC));
            }
            if ignored.matches(trust_root) && !paranoid_config(native)? {
                return Err(invalid(DIAGNOSTIC));
            }
            check_persisted_ignore(native, &path)?;
            // Each declaring layer must be trusted and nonempty. The last
            // declaration replaces the entire list.
            recipients = encryption.recipients.into_iter().collect();
        }
    }
    if recipients.is_empty() || recipients.iter().any(|key| !keys::recipient_shape(key)) {
        return Err(invalid(DIAGNOSTIC));
    }
    Ok(recipients)
}

fn layers(dir: &Path, environments: &[&str], env_conf_d: bool) -> Vec<PathBuf> {
    let entries: BTreeSet<_> = fs::read_dir(dir.join("conf.d"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    let folders: Vec<_> = entries.iter().filter(|path| path.is_dir()).collect();
    let mut paths: Vec<_> = entries
        .iter()
        .filter(|path| {
            path.extension() == Some(OsStr::new("toml"))
                && (!env_conf_d || fragment_environment(path).is_none())
        })
        .cloned()
        .collect();
    for local in [false, true] {
        let suffix = if local { ".local" } else { "" };
        for environment in std::iter::once(None).chain(environments.iter().copied().map(Some)) {
            let env_suffix = environment.map_or_else(String::new, |name| format!(".{name}"));
            if let Some(name) = environment.filter(|_| env_conf_d) {
                paths.extend(
                    entries
                        .iter()
                        .filter(|path| fragment_environment(path) == Some((name, local)))
                        .cloned(),
                );
            }
            let mise = format!("mise{env_suffix}{suffix}.toml");
            paths.extend(folders.iter().map(|folder| folder.join(&mise)));
            paths.extend([
                dir.join(format!("config{env_suffix}{suffix}.toml")),
                dir.join(mise),
            ]);
        }
    }
    let mut seen = BTreeSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    paths
}

fn fragment_environment(path: &Path) -> Option<(&str, bool)> {
    let stem = path.file_stem()?.to_str()?;
    if path.extension()? != "toml" {
        return None;
    }
    let (stem, local) = stem
        .strip_suffix(".local")
        .map_or((stem, false), |stem| (stem, true));
    let (name, environment) = stem.split_once('.')?;
    (!name.is_empty() && !environment.is_empty()).then_some((environment, local))
}

fn is_folder_fragment(path: &Path, folder: &Path) -> bool {
    folder.parent().and_then(Path::file_name) == Some(OsStr::new("conf.d"))
        && !folder
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        && path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("mise."))
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Fail> {
    passive::toml(path)
        .and_then(|doc| toml_edit::de::from_document(doc).map_err(|_| invalid(DIAGNOSTIC)))
        .map_err(|_| invalid(DIAGNOSTIC))
}

fn read_early(path: &Path) -> Result<Early, Fail> {
    if !path.is_file() {
        return Ok(Early::default());
    }
    let early: Early = read_toml(path)?;
    if early
        .env
        .iter()
        .flatten()
        .chain(early.env_sets.keys())
        .chain(early.env_sets.values().flatten())
        .chain(early.ignored_config_paths.iter().flatten())
        .any(|value| {
            ["{{", "{%", "{#"]
                .iter()
                .any(|marker| value.contains(marker))
        })
    {
        return Err(invalid(DIAGNOSTIC));
    }
    Ok(early)
}

// Project miserc can select native environments, but only system/global
// miserc can control global fragment naming or ignore a history config file.
fn merge_project_selectors(
    early: &mut Early,
    root: &Path,
    home: &Path,
    ceiling: Option<&OsStr>,
) -> Result<(), Fail> {
    let ceiling: BTreeSet<_> = ceiling
        .into_iter()
        .flat_map(env::split_paths)
        .map(|path| expand_home(home, &path))
        .collect();
    let protected: BTreeSet<_> = early
        .env
        .iter()
        .flatten()
        .chain(early.env_sets.keys())
        .cloned()
        .collect();
    let mut ancestors: Vec<_> = root
        .ancestors()
        .take_while(|dir| !ceiling.contains(*dir))
        .collect();
    if let Some(index) = ancestors.iter().position(|dir| *dir == home) {
        ancestors.truncate(index + 1);
    }
    for dir in ancestors.into_iter().rev() {
        for name in [".config/miserc.toml", ".miserc.toml", ".miserc.local.toml"] {
            if name == ".config/miserc.toml" && (dir == home || dir.parent().is_none()) {
                continue;
            }
            let layer = read_early(&dir.join(name))?;
            early.auto_env = layer.auto_env.or(early.auto_env);
            early.env_sets.extend(
                layer
                    .env_sets
                    .into_iter()
                    .filter(|(name, _)| !protected.contains(name)),
            );
        }
    }
    Ok(())
}

fn environments<'a>(selected: &'a str, early: &'a Early, auto: bool) -> Vec<&'a str> {
    let mut names = Vec::new();
    for name in selected.split(',').filter(|name| !name.is_empty()) {
        let set = early.env_sets.get(name);
        for name in set
            .into_iter()
            .flatten()
            .map(String::as_str)
            .chain(set.is_none().then_some(name))
        {
            if !name.contains(['/', '\\']) && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    if auto {
        let arch = match env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            arch => arch,
        };
        // The platform name is constant for this process.
        let platform = PLATFORM.get_or_init(|| format!("{}-{arch}", env::consts::OS));
        for name in [platform.as_str(), env::consts::OS, env::consts::FAMILY] {
            if !names.contains(&name) {
                names.insert(0, name);
            }
        }
    }
    names
}

static PLATFORM: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn setting(value: Option<&OsStr>) -> Option<bool> {
    match value?.to_str()?.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn expand_home(home: &Path, path: &Path) -> PathBuf {
    path.strip_prefix("~/")
        .map_or_else(|_| path.to_owned(), |suffix| home.join(suffix))
}

fn resolve(home: &Path, dir: &Path, path: &OsStr) -> PathBuf {
    let mut result = PathBuf::new();
    for part in dir.join(expand_home(home, Path::new(path))).components() {
        if part == Component::ParentDir {
            result.pop();
        } else {
            result.push(part.as_os_str());
        }
    }
    result
}

#[derive(Default)]
struct IgnorePaths {
    literals: Vec<PathBuf>,
    patterns: Vec<globset::GlobMatcher>,
}

impl IgnorePaths {
    fn new(paths: Vec<PathBuf>) -> Self {
        let mut ignored = Self::default();
        for path in paths {
            let pattern = path.to_string_lossy();
            if pattern.contains(['*', '?', '[', '{'])
                && let Ok(glob) = globset::GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .build()
            {
                ignored.patterns.push(glob.compile_matcher());
            } else {
                ignored.literals.push(path);
            }
        }
        ignored
    }

    fn matches(&self, path: &Path) -> bool {
        self.literals.iter().any(|prefix| path.starts_with(prefix))
            || self.patterns.iter().any(|glob| glob.is_match(path))
    }
}

fn mise_setting(native: &Native<'_, '_>, setting: &str) -> Result<Item, Fail> {
    let mut cmd = native.mise(&["settings", "get", setting])?;
    cmd.env("GIT_ALLOW_PROTOCOL", "");
    let output = native
        .text(cmd, DIAGNOSTIC)
        .map_err(|_| invalid(DIAGNOSTIC))?;
    let mut doc: DocumentMut = format!("value = {output}")
        .parse()
        .map_err(|_| invalid(DIAGNOSTIC))?;
    doc.remove("value").ok_or_else(|| invalid(DIAGNOSTIC))
}

fn paranoid_config(native: &Native<'_, '_>) -> Result<bool, Fail> {
    let value = mise_setting(native, "paranoid")?;
    Ok(value.as_bool().unwrap_or_else(|| {
        value.as_array().is_some_and(|categories| {
            categories
                .iter()
                .any(|category| category.as_str() == Some("config"))
        })
    }))
}

fn check_persisted_ignore(native: &Native<'_, '_>, path: &Path) -> Result<(), Fail> {
    let canonical = path.canonicalize().map_err(|_| invalid(DIAGNOSTIC))?;
    let ignored = fs::read_dir(native.state_dir()?.join("ignored-configs"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            let entry = entry.path();
            if entry.is_symlink() {
                fs::read_link(entry).ok().as_ref() == Some(&canonical)
            } else {
                entry.is_file()
                    && fs::read_to_string(entry).is_ok_and(|record| Path::new(&record) == canonical)
            }
        });
    if !ignored {
        return Ok(());
    }
    // Explicit trust overrides a persisted dismissal but not ignored_config_paths.
    let paths = mise_setting(native, "trusted_config_paths")?;
    let home = native.home()?;
    let root = native.ctx.root.path();
    if paths
        .as_array()
        .ok_or_else(|| invalid(DIAGNOSTIC))?
        .iter()
        .filter_map(|path| path.as_str().filter(|path| !path.is_empty()))
        .map(|path| expand_home(home, Path::new(path)))
        .any(|path| {
            canonical.starts_with(&path)
                || root
                    .join(path)
                    .canonicalize()
                    .is_ok_and(|path| canonical.starts_with(path))
        })
    {
        Ok(())
    } else {
        Err(invalid(DIAGNOSTIC))
    }
}
