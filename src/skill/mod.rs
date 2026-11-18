use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

use crate::config::physical;
use crate::{Ctx, Env, Fail};

const SKILL: &[u8] = include_bytes!("../../skills/dots/SKILL.md");

pub fn refresh(ctx: &Ctx) -> Result<(), Fail> {
    let env = &ctx.env;
    if env.get("DOTS_SKILL_LINKS") == Some(OsStr::new("0")) {
        return Ok(());
    }
    if env.home().is_none() && env.get("XDG_STATE_HOME").is_none_or(OsStr::is_empty) {
        return Ok(());
    }
    let state = env
        .state_dir()
        .map_err(|error| Fail::new(1, error.to_string()))?;
    refresh_in(env, &state).map_err(|error| {
        Fail::new(
            1,
            format!(
                "could not update the dots agent skill at {}: {error}",
                state.display()
            ),
        )
    })
}

fn refresh_in(env: &Env, state: &Path) -> io::Result<()> {
    let claude = agent_root(env, "CLAUDE_CONFIG_DIR", ".claude");
    let codex = agent_root(env, "CODEX_HOME", ".codex");
    let protected: Vec<_> = [&claude, &codex]
        .into_iter()
        .flatten()
        .filter(|path| !path.exists())
        .filter_map(|path| physical(path).ok())
        .collect();
    if would_create_agent_root(state, &protected) {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state)?;
    let state = state.canonicalize()?;
    // Serialize updates to the existing ownership and warn-once records.
    let guard = File::open(&state)?;
    guard.lock()?;
    let skills = contained_dir(&state, "skills")?;
    let source = contained_dir(&skills, "dots")?;
    let skill_file = source.join("SKILL.md");
    replace_file(&skill_file, SKILL)?;

    let owned_path = state.join("skill-links");
    let notices_path = state.join("skill-notices");
    let mut ownership = read_records(&owned_path)?;
    let mut notices: BTreeSet<_> = read_optional(&notices_path)?
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    for base in agent_roots(env, claude, codex, &protected)? {
        let link = base.join("skills/dots");
        if would_create_agent_root(&link, &protected) {
            continue;
        }
        let key = tsv_path(&link)?;
        let parent = match base
            .canonicalize()
            .and_then(|base| contained_dir(&base, "skills"))
        {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                warn_once(
                    &mut notices,
                    &notices_path,
                    &key,
                    format!(
                        "left {} alone; its directory leads outside {}, so the dots skill is not linked there",
                        link.display(),
                        base.display()
                    ),
                )?;
                continue;
            }
            Err(error) => return Err(error),
        };

        let path = parent.join("dots");
        let target = fs::read_link(&path).ok();
        let ours = target.as_deref().is_some_and(|target| {
            ownership
                .get(&key)
                .is_some_and(|saved| saved == target.as_os_str().as_bytes())
        });
        let exists = match fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if exists && !ours {
            warn_once(
                &mut notices,
                &notices_path,
                &key,
                format!(
                    "left {} alone; it is not a link dots made, so the dots skill is not linked there",
                    link.display()
                ),
            )?;
            continue;
        }
        if target.as_deref() == Some(source.as_path()) {
            continue;
        }

        let new_target = tsv_path(&source)?;
        if let Some(target) = target {
            let staged = tempfile::tempdir_in(&parent)?;
            let staged_link = staged.path().join("dots");
            symlink(&source, &staged_link)?;
            if fs::read_link(&path).ok().as_ref() != Some(&target) {
                continue;
            }
            fs::rename(staged_link, &path)?;
        } else {
            match symlink(&source, &path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        ownership.insert(key, new_target);
        if let Err(error) = write_records(&owned_path, &ownership) {
            // Leave a concurrent user replacement untouched.
            if fs::read_link(&path).ok().as_deref() == Some(source.as_path()) {
                let _ = fs::remove_file(&path);
            }
            return Err(error);
        }
    }
    Ok(())
}

fn agent_root(env: &Env, variable: &str, default: &str) -> Option<PathBuf> {
    env.get(variable)
        .filter(|value| !value.is_empty())
        .map(|value| env.cwd().join(value))
        .or_else(|| env.home().map(|home| env.cwd().join(home).join(default)))
}

fn would_create_agent_root(path: &Path, protected: &[PathBuf]) -> bool {
    physical(path).is_ok_and(|path| protected.iter().any(|root| path.starts_with(root)))
}

fn agent_roots(
    env: &Env,
    claude: Option<PathBuf>,
    codex: Option<PathBuf>,
    protected: &[PathBuf],
) -> io::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    if let Some(claude) = claude.filter(|path| path.is_dir()) {
        roots.push(claude);
    }
    if let Some(agents) = env.home().map(|home| env.cwd().join(home).join(".agents"))
        && !would_create_agent_root(&agents, protected)
    {
        // The shared Agent Skills directory also serves an existing Codex.
        if !agents.is_dir() && codex.is_some_and(|path| path.is_dir()) {
            match fs::create_dir(&agents) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        if agents.is_dir() {
            roots.push(agents);
        }
    }
    Ok(roots)
}

fn contained_dir(base: &Path, name: &str) -> io::Result<PathBuf> {
    let path = base.join(name);
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let resolved = path.canonicalize()?;
    if !resolved.starts_with(base) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory leads outside its base",
        ));
    }
    if !resolved.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "expected a directory",
        ));
    }
    Ok(resolved)
}

fn tsv_path(path: &Path) -> io::Result<Vec<u8>> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.iter().any(|byte| matches!(byte, b'\n' | b'\t')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill path contains a tab or newline",
        ));
    }
    Ok(bytes.to_vec())
}

fn read_records(path: &Path) -> io::Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    Ok(read_optional(path)?
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let tab = line.iter().position(|byte| *byte == b'\t')?;
            Some((line[..tab].to_vec(), line[tab + 1..].to_vec()))
        })
        .collect())
}

fn write_records(path: &Path, records: &BTreeMap<Vec<u8>, Vec<u8>>) -> io::Result<()> {
    let mut bytes = Vec::new();
    for (link, target) in records {
        bytes.extend_from_slice(link);
        bytes.push(b'\t');
        bytes.extend_from_slice(target);
        bytes.push(b'\n');
    }
    replace_file(path, &bytes)
}

fn warn_once(
    notices: &mut BTreeSet<Vec<u8>>,
    path: &Path,
    key: &[u8],
    diagnostic: String,
) -> io::Result<()> {
    if notices.insert(key.to_vec()) {
        eprintln!("dots: {}", Fail::new(0, diagnostic));
        let mut bytes = Vec::new();
        for key in notices.iter() {
            bytes.extend_from_slice(key);
            bytes.push(b'\n');
        }
        replace_file(path, &bytes)?;
    }
    Ok(())
}

fn read_optional(path: &Path) -> io::Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if read_optional(path)? == bytes {
        return Ok(());
    }
    let mut file = tempfile::NamedTempFile::new_in(path.parent().expect("file path has a parent"))?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}
