use std::collections::BTreeSet;
use std::ffi::{CString, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use nix::libc;
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};
use toml_edit::{DocumentMut, InlineTable, Item, TableLike, Value, value};

use crate::config::Vars;
use crate::process::Cwd;
use crate::run::Run;
use crate::{Ctx, Fail};

use super::edit::{self, DOTFILES, Edit};
use super::{absolute_request, file_request, io_fail, missing, physical};

#[derive(Debug)]
pub struct Paths {
    home: PathBuf,
    target: PathBuf,
    source: PathBuf,
    key: String,
    source_key: String,
}

pub fn requests<'a>(
    ctx: &Ctx,
    selected: Option<&Path>,
    requests: impl Iterator<Item = &'a OsString>,
) -> Result<Vec<Option<Paths>>, Fail> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut checked_selector = false;
    requests
        .map(|request| {
            if !file_request(request) {
                return Ok(None);
            }
            if !checked_selector && let Some(path) = selected {
                let vars = Vars::read(&ctx.root)?;
                let names = std::iter::once("config.toml".into()).chain(
                    vars.profiles
                        .iter()
                        .map(|profile| format!("config.{profile}.toml")),
                );
                if !names
                    .map(|name| ctx.root.path().join(name))
                    .any(|allowed| allowed == path)
                {
                    return Err(Fail::usage(format!(
                        "a file request needs config.toml or a declared profile file in {}",
                        ctx.root.path().display()
                    )));
                }
                checked_selector = true;
            }
            let paths = paths(ctx, Path::new(request))?;
            if seen.iter().any(|target| same_name(target, &paths.target)) {
                return Err(Fail::usage(format!(
                    "file requested twice: {}",
                    paths.target.display()
                )));
            }
            seen.insert(paths.target.clone());
            Ok(Some(paths))
        })
        .collect()
}

fn paths(ctx: &Ctx, request: &Path) -> Result<Paths, Fail> {
    let raw_home = ctx
        .env
        .home()
        .ok_or_else(|| Fail::usage("file requests need HOME"))?;
    let home = raw_home
        .canonicalize()
        .map_err(|error| refusal(raw_home, error))?;
    let raw_target = absolute_request(ctx, request)?;
    let relative = raw_target
        .strip_prefix(raw_home)
        .or_else(|_| raw_target.strip_prefix(&home))
        .map_err(|_| refusal(&raw_target, "target is outside HOME"))?;
    let mut target = home.clone();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        match component {
            Component::Normal(name) => {
                target.push(name);
                if components.peek().is_some() {
                    verify_directory(&target, false)?;
                }
            }
            Component::CurDir => (),
            Component::ParentDir if target != home => {
                target.pop();
            }
            _ => return Err(refusal(&raw_target, "target is outside HOME")),
        }
    }
    let state = ctx
        .env
        .state_dir()
        .and_then(|path| physical(&path))
        .map_err(|error| refusal(&target, error))?;
    if target == home || target.starts_with(ctx.root.path()) || target.starts_with(state) {
        return Err(refusal(
            &target,
            "target must be below HOME and outside the configuration and dots state",
        ));
    }
    let relative = target.strip_prefix(&home).expect("checked under HOME");
    let relative_text = relative
        .to_str()
        .ok_or_else(|| refusal(&target, "path must be UTF-8 to record it"))?;
    let source_key = format!("dotfiles/{relative_text}");
    let source = ctx.root.path().join(&source_key);
    verify_source_parents(ctx.root.path(), &source)?;
    let key = format!("~/{relative_text}");
    Ok(Paths {
        home,
        target,
        source,
        key,
        source_key,
    })
}

fn verify_directory(path: &Path, allow_missing: bool) -> Result<(), Fail> {
    match missing(fs::symlink_metadata(path)).map_err(|error| refusal(path, error))? {
        Some(metadata) if metadata.is_dir() => Ok(()),
        None if allow_missing => Ok(()),
        _ => Err(refusal(path, "expected a directory with no symlink")),
    }
}

fn verify_target_parent(paths: &Paths) -> Result<(), Fail> {
    let mut dir = paths.home.clone();
    let parent = paths.target.parent().expect("target below HOME");
    for component in parent
        .strip_prefix(&paths.home)
        .expect("target below HOME")
        .components()
    {
        dir.push(component);
        verify_directory(&dir, false).map_err(|error| Fail::new(1, error.diagnostic))?;
    }
    Ok(())
}

fn verify_source_parents(root: &Path, source: &Path) -> Result<(), Fail> {
    let mut dir = root.to_owned();
    let parent = source.parent().expect("source below root");
    for component in parent
        .strip_prefix(root)
        .expect("source below root")
        .components()
    {
        dir.push(component);
        verify_directory(&dir, true)?;
    }
    Ok(())
}

fn refusal(path: &Path, reason: impl std::fmt::Display) -> Fail {
    Fail::usage(format!("{}: {reason}", path.display()))
}

fn config_name(name: &str) -> bool {
    ["config", "mise", ".mise"].iter().any(|base| {
        name.strip_prefix(base)
            .is_some_and(|tail| tail.starts_with('.') && tail.ends_with(".toml"))
    })
}

fn references(ctx: &Ctx, paths: &Paths) -> Result<Vec<(PathBuf, bool)>, Fail> {
    let root = ctx.root.path();
    let dir = fs::read_dir(root).map_err(|error| refusal(root, error))?;
    let mut configs = BTreeSet::new();
    for entry in dir {
        let entry = entry.map_err(|error| refusal(root, error))?;
        if entry.file_name().to_str().is_some_and(config_name) {
            let path = physical(&entry.path()).map_err(|error| refusal(&entry.path(), error))?;
            configs.insert(path);
        }
    }
    let mut found = Vec::new();
    for path in configs {
        let text = fs::read_to_string(&path).map_err(|error| refusal(&path, error))?;
        let doc: DocumentMut = text
            .parse()
            .map_err(|error: toml_edit::TomlError| refusal(&path, error.message()))?;
        if let Some(table) = edit::table(&doc, DOTFILES)? {
            for (key, item) in table.iter() {
                let target = declared(&paths.home, &paths.home, key, &path)?;
                if same_name(&target, &paths.target) || uses_paths(item, root, paths, &path)? {
                    let exact = key == paths.key
                        && item.as_table_like().is_some_and(|record| {
                            record.len() == 2
                                && record.get("source").and_then(Item::as_str)
                                    == Some(&paths.source_key)
                                && record.get("mode").and_then(Item::as_str) == Some("symlink")
                        });
                    found.push((path.clone(), exact));
                }
            }
        }
    }
    Ok(found)
}

fn declared(base: &Path, home: &Path, value: &str, config: &Path) -> Result<PathBuf, Fail> {
    if value.contains("{{") || value.contains("{%") {
        return Err(refusal(
            config,
            "cannot prove uniqueness with a templated dotfile path",
        ));
    }
    let path = value
        .strip_prefix("~/")
        .map_or_else(|| base.join(value), |relative| home.join(relative));
    let Some(parent) = path.parent() else {
        return Ok(path);
    };
    let filename = path
        .file_name()
        .ok_or_else(|| refusal(config, "invalid dotfile path"))?;
    physical(parent)
        .map(|parent| parent.join(filename))
        .map_err(|error| refusal(config, error))
}

fn same_name(first: &Path, second: &Path) -> bool {
    if first == second {
        return true;
    }
    if first.parent() == second.parent()
        && let Ok(first_entry) = fs::symlink_metadata(first)
        && let Ok(second_entry) = fs::symlink_metadata(second)
        && (first_entry.dev(), first_entry.ino()) == (second_entry.dev(), second_entry.ino())
    {
        // Realpath reflects the spelling stored by the filesystem. Unlike
        // comparing inodes alone, it distinguishes regular-file hardlinks.
        if first_entry.nlink() == 1
            || !first_entry.is_symlink()
                && first
                    .canonicalize()
                    .is_ok_and(|a| second.canonicalize().is_ok_and(|b| a == b))
        {
            return true;
        }
    }
    #[cfg(target_os = "macos")]
    if first
        .as_os_str()
        .as_bytes()
        .eq_ignore_ascii_case(second.as_os_str().as_bytes())
        && let Some(parent) = second.ancestors().find(|parent| parent.is_dir())
        && let Ok(parent) = CString::new(parent.as_os_str().as_bytes())
    {
        // The nearest directory identifies the volume even before the leaf exists.
        // pathconf reads the path synchronously.
        return unsafe { libc::pathconf(parent.as_ptr(), libc::_PC_CASE_SENSITIVE) } == 0;
    }
    false
}

fn uses_paths(item: &Item, root: &Path, paths: &Paths, config: &Path) -> Result<bool, Fail> {
    fn matches(value: &str, root: &Path, paths: &Paths, config: &Path) -> Result<bool, Fail> {
        let declared = declared(root, &paths.home, value, config)?;
        if same_name(&declared, &paths.source) {
            return Ok(true);
        }
        let metadata =
            |path: &Path| missing(fs::metadata(path)).map_err(|error| refusal(config, error));
        let Some((reference, source)) = metadata(&declared)?.zip(metadata(&paths.source)?) else {
            return Ok(false);
        };
        Ok((reference.dev(), reference.ino()) == (source.dev(), source.ino()))
    }

    fn record(
        table: &dyn TableLike,
        root: &Path,
        paths: &Paths,
        config: &Path,
    ) -> Result<bool, Fail> {
        if let Some(target) = table.get("target").and_then(Item::as_str)
            && same_name(
                &declared(&paths.home, &paths.home, target, config)?,
                &paths.target,
            )
        {
            return Ok(true);
        }
        if let Some(source) = table.get("source").and_then(Item::as_str)
            && matches(source, root, paths, config)?
        {
            return Ok(true);
        }
        if let Some(variants) = table.get("variants") {
            if let Some(variants) = variants.as_array() {
                for variant in variants.iter().filter_map(Value::as_inline_table) {
                    if record(variant, root, paths, config)? {
                        return Ok(true);
                    }
                }
            } else if let Some(variants) = variants.as_array_of_tables() {
                for variant in variants.iter() {
                    if record(variant, root, paths, config)? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
    if let Some(table) = item.as_table_like() {
        record(table, root, paths, config)
    } else if let Some(path) = item.as_str() {
        matches(path, root, paths, config)
    } else {
        Ok(false)
    }
}

fn ignored(ctx: &Ctx, run: &Run<'_>, paths: &Paths) -> Result<bool, Fail> {
    let root = ctx.root.path();
    let git = || ctx.command("git", Cwd::Root);
    let probe = |cmd| {
        run.probe(cmd).map_err(|error| {
            Fail::new(
                1,
                format!("git could not check ignore rules for add FILE: {error}"),
            )
        })
    };
    let mut repo = git()?;
    repo.args(["rev-parse", "--show-toplevel"]);
    let result = probe(repo)?;
    let isolated = if result.status.success() {
        None
    } else if String::from_utf8_lossy(&result.stderr).contains("not a git repository") {
        let dir = tempfile::tempdir()
            .map_err(|error| io_fail("could not stage Git metadata for", root, error))?;
        let mut init = git()?;
        init.args(["init", "--bare", "--template=", "--quiet"])
            .arg(dir.path());
        let outcome = probe(init)?;
        if !outcome.status.success() {
            return Err(Fail::new(
                1,
                "git could not prepare an ignore query for add FILE",
            ));
        }
        Some(dir)
    } else {
        return Err(Fail::new(
            1,
            "git could not determine the configuration repository for add FILE",
        ));
    };
    let mut check = git()?;
    if let Some(dir) = &isolated {
        check
            .arg("--git-dir")
            .arg(dir.path())
            .arg("--work-tree")
            .arg(root);
    }
    check.args([
        "check-ignore",
        "--no-index",
        "--quiet",
        "--",
        &paths.source_key,
    ]);
    match probe(check)?.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Fail::new(
            1,
            "git could not check ignore rules for add FILE",
        )),
    }
}

fn preflight_add(
    ctx: &Ctx,
    run: &Run<'_>,
    paths: &Paths,
    config: &Path,
) -> Result<Option<Entry>, Fail> {
    let existing = snapshot(&paths.target)
        .map_err(|error| refusal(&paths.target, error))?
        .ok_or_else(|| refusal(&paths.target, "target file is missing"))?;
    let source = snapshot(&paths.source).map_err(|error| refusal(&paths.source, error))?;
    let records = references(ctx, paths)?;
    if is_link(paths, &existing)
        && source.as_ref().is_some_and(Entry::is_regular)
        && records == [(config.to_owned(), true)]
    {
        return Ok(None);
    }
    if !existing.is_regular() {
        return Err(refusal(
            &paths.target,
            format!(
                "expected a regular file or its exact adoption in {}",
                config.display()
            ),
        ));
    }
    if source.is_some() {
        return Err(reconcile(
            paths,
            &records,
            "source already exists; reconcile the adoption before retrying",
        ));
    }
    if let Some((file, _)) = records.first() {
        return Err(refusal(
            file,
            format!(
                "already declares {} or uses {}",
                paths.key, paths.source_key
            ),
        ));
    }
    if ignored(ctx, run, paths)? {
        return Err(refusal(
            &paths.source,
            "the configuration's Git rules ignore this source",
        ));
    }
    Ok(Some(existing))
}

pub struct Adoption {
    paths: Paths,
    config: PathBuf,
    original: Option<Entry>,
}

pub fn prepare_add(
    ctx: &Ctx,
    run: &Run<'_>,
    paths: Paths,
    config: &Path,
) -> Result<Adoption, Fail> {
    let original = preflight_add(ctx, run, &paths, config)?;
    Ok(Adoption {
        paths,
        config: config.into(),
        original,
    })
}

impl Adoption {
    pub fn execute(self, ctx: &Ctx) -> Result<(), Fail> {
        let Self {
            paths,
            config,
            original,
        } = self;
        let Some(original) = original else {
            println!("Already adopted {}", paths.target.display());
            return Ok(());
        };
        verify_target_parent(&paths)?;
        if !references(ctx, &paths)?.is_empty() {
            return Err(Fail::new(
                1,
                format!("a declaration appeared for {}", paths.key),
            ));
        }
        let mut edit = Edit::open(&config)?;
        let mut record = InlineTable::new();
        record.insert("source", Value::from(&paths.source_key));
        record.insert("mode", Value::from("symlink"));
        edit::put(&mut edit.doc.document, DOTFILES, &paths.key, value(record))?;

        let mut parents = Vec::new();
        let result = (|| {
            create_parents(ctx.root.path(), &paths.source, &mut parents)?;
            let mut link = Replacement::new(&paths.target, original.clone())?;
            symlink(&paths.source, &link.parked)
                .map_err(|error| io_fail("could not stage link for", &paths.target, error))?;
            let source = create_source(&paths.source, &original)?;
            match edit.install() {
                Ok(Some(written)) => {
                    #[cfg(test)]
                    tests::before_link_install();
                    if let Err(error) = link.install() {
                        return Err(edit::rollback(
                            error,
                            [written.restore(), retract_source(ctx, &paths, &source)],
                        ));
                    }
                }
                Ok(None) => {
                    return Err(edit::rollback(
                        Fail::new(1, "adoption declaration was not added"),
                        [retract_source(ctx, &paths, &source)],
                    ));
                }
                Err(error) => {
                    return Err(edit::rollback(
                        error,
                        [retract_source(ctx, &paths, &source)],
                    ));
                }
            }
            Ok(())
        })();
        if result.is_err() {
            for directory in parents.iter().rev() {
                let _ = fs::remove_dir(directory);
            }
        }
        result?;
        println!("Adopted {}", paths.target.display());
        Ok(())
    }
}

fn create_parents(root: &Path, source: &Path, created: &mut Vec<PathBuf>) -> Result<(), Fail> {
    let mut dir = root.to_owned();
    for component in source
        .parent()
        .expect("source parent")
        .strip_prefix(root)
        .expect("source below root")
        .components()
    {
        dir.push(component);
        match fs::create_dir(&dir) {
            Ok(()) => created.push(dir.clone()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                verify_directory(&dir, false).map_err(|error| Fail::new(1, error.diagnostic))?;
            }
            Err(error) => return Err(io_fail("could not create source directory", &dir, error)),
        }
    }
    Ok(())
}

pub struct Removal {
    paths: Paths,
    config: PathBuf,
    target: Entry,
    source: Entry,
}

pub fn prepare_remove(ctx: &Ctx, paths: Paths, selected: Option<&Path>) -> Result<Removal, Fail> {
    let records = references(ctx, &paths)?;
    let [(config, true)] = records.as_slice() else {
        return Err(reconcile(
            &paths,
            &records,
            "cannot find a unique adoption declaration",
        ));
    };
    if selected.is_some_and(|selected| selected != config) {
        return Err(refusal(
            &paths.target,
            format!("its adoption declaration is in {}", config.display()),
        ));
    }
    let target = snapshot(&paths.target)
        .map_err(|error| refusal(&paths.target, error))?
        .filter(|entry| is_link(&paths, entry))
        .ok_or_else(|| {
            reconcile(
                &paths,
                &records,
                "target is no longer the expected adoption link",
            )
        })?;
    let source = snapshot(&paths.source)
        .map_err(|error| refusal(&paths.source, error))?
        .filter(Entry::is_regular)
        .ok_or_else(|| {
            reconcile(
                &paths,
                &records,
                "adoption source is missing or no longer regular",
            )
        })?;
    Ok(Removal {
        paths,
        config: config.clone(),
        target,
        source,
    })
}

impl Removal {
    pub fn describe(&self) {
        println!(
            "Restore {} from {}",
            self.paths.target.display(),
            self.paths.source.display()
        );
    }

    pub fn execute(self, ctx: &Ctx) -> Result<(), Fail> {
        let Self {
            paths,
            config,
            target,
            source,
        } = self;
        verify_target_parent(&paths)?;
        verify_source_parents(ctx.root.path(), &paths.source)
            .map_err(|error| Fail::new(1, error.diagnostic))?;
        let mut edit = Edit::open(&config).map_err(|error| Fail::new(1, error.diagnostic))?;
        if references(ctx, &paths).map_err(|error| Fail::new(1, error.diagnostic))?
            != [(config.clone(), true)]
        {
            return Err(Fail::new(
                1,
                format!("adoption declaration changed for {}", paths.key),
            ));
        }
        edit::delete(&mut edit.doc.document, DOTFILES, &paths.key)?;

        let mut replacement = Replacement::new(&paths.target, target)?;
        write_regular(&replacement.parked, &source)?;
        replacement.install()?;
        let parked = match Stashed::new(&paths.source, &source) {
            Ok(parked) => parked,
            Err(error) => return Err(edit::rollback(error, [replacement.restore()])),
        };
        let written = match edit.install() {
            Ok(written) => written,
            Err(error) => {
                return Err(edit::rollback(
                    error,
                    [parked.restore(), replacement.restore()],
                ));
            }
        };
        if !snapshot(&parked.parked).is_ok_and(|entry| entry.as_ref() == Some(&source)) {
            return Err(edit::rollback(
                Fail::new(
                    1,
                    format!(
                        "adoption source changed during removal: {}",
                        paths.source.display()
                    ),
                ),
                [
                    written.map_or(Ok(()), edit::Written::restore),
                    parked.restore(),
                    replacement.restore(),
                ],
            ));
        }
        drop(parked);
        println!("Restored {}", paths.target.display());
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Entry {
    stamp: (u64, u64, u32, i64, i64),
    data: Data,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Data {
    File(Vec<u8>),
    Link(PathBuf),
    Other,
}

impl Entry {
    fn is_regular(&self) -> bool {
        matches!(self.data, Data::File(_))
    }
}

pub(super) fn snapshot(path: &Path) -> io::Result<Option<Entry>> {
    fn stamp(metadata: &fs::Metadata) -> (u64, u64, u32, i64, i64) {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    }
    let Some(metadata) = missing(fs::symlink_metadata(path))? else {
        return Ok(None);
    };
    let data = if metadata.is_file() {
        Data::File(fs::read(path)?)
    } else if metadata.is_symlink() {
        Data::Link(fs::read_link(path)?)
    } else {
        Data::Other
    };
    let before = stamp(&metadata);
    if stamp(&fs::symlink_metadata(path)?) != before {
        return Err(io::Error::other("file changed while reading it"));
    }
    Ok(Some(Entry {
        stamp: before,
        data,
    }))
}

fn is_link(paths: &Paths, entry: &Entry) -> bool {
    let Data::Link(link) = &entry.data else {
        return false;
    };
    let parent = paths.target.parent().expect("target parent");
    let destination = parent.join(link);
    destination
        .parent()
        .zip(destination.file_name())
        .and_then(|(dir, leaf)| physical(dir).ok().map(|dir| dir.join(leaf)))
        .is_some_and(|path| path == paths.source)
}

pub(super) fn needed(path: &Path) -> Result<Entry, Fail> {
    snapshot(path)
        .map_err(|error| io_fail("could not verify", path, error))?
        .ok_or_else(|| Fail::new(1, format!("{} disappeared", path.display())))
}

fn write_contents(file: &mut NamedTempFile, data: &Entry) -> io::Result<()> {
    let Data::File(bytes) = &data.data else {
        return Err(io::Error::other("expected regular file"));
    };
    file.write_all(bytes)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(data.stamp.2))?;
    file.as_file().sync_all()
}

fn write_regular(path: &Path, data: &Entry) -> Result<(), Fail> {
    let result = (|| {
        let mut file = NamedTempFile::new_in(path.parent().expect("staged file parent"))?;
        write_contents(&mut file, data)?;
        file.persist_noclobber(path)?;
        Ok(())
    })();
    result.map_err(|error| io_fail("could not write", path, error))
}

fn create_source(path: &Path, data: &Entry) -> Result<Entry, Fail> {
    let result = (|| {
        let mut file = NamedTempFile::new_in(path.parent().expect("source parent"))?;
        write_contents(&mut file, data)?;
        let entry =
            snapshot(file.path())?.ok_or_else(|| io::Error::other("staged source disappeared"))?;
        file.persist_noclobber(path)?;
        Ok(entry)
    })();
    result.map_err(|error| io_fail("could not create source", path, error))
}

fn remove_owned(path: &Path, expected: &Entry) -> Result<(), Fail> {
    Stashed::new(path, expected).map(drop)
}

fn retract_source(ctx: &Ctx, paths: &Paths, expected: &Entry) -> Result<(), Fail> {
    let records = references(ctx, paths)?;
    let target = snapshot(&paths.target)
        .map_err(|error| io_fail("could not inspect", &paths.target, error))?;
    if !records.is_empty() || target.as_ref().is_some_and(|target| is_link(paths, target)) {
        return Err(reconcile(
            paths,
            &records,
            "kept the source because it is still referenced",
        ));
    }
    remove_owned(&paths.source, expected)
}

fn staging_prefix(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_bytes());
    let prefix = u64::from_be_bytes(digest[..8].try_into().expect("eight SHA-256 bytes"));
    format!(".dots-file-{prefix:016x}-")
}

fn parked_paths(path: &Path) -> Vec<PathBuf> {
    let prefix = staging_prefix(path);
    let Some(entries) = path.parent().and_then(|parent| fs::read_dir(parent).ok()) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().as_bytes().starts_with(prefix.as_bytes())
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path().join("original"))
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .collect()
}

fn reconcile(paths: &Paths, records: &[(PathBuf, bool)], reason: &str) -> Fail {
    let declarations = records
        .iter()
        .map(|(path, _)| path.display().to_string())
        .collect::<Vec<_>>();
    let parked = parked_paths(&paths.target)
        .into_iter()
        .chain(parked_paths(&paths.source))
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    let declaration = if declarations.is_empty() {
        "no declaration was found".into()
    } else {
        format!("configuration: {}", declarations.join(", "))
    };
    let mut message = format!(
        "{reason} for {} and {} ({declaration})",
        paths.key,
        paths.source.display()
    );
    if !parked.is_empty() {
        message.push_str(&format!("; parked entries: {}", parked.join(", ")));
    }
    refusal(&paths.target, message)
}

fn staging(path: &Path) -> Result<TempDir, Fail> {
    tempfile::Builder::new()
        .prefix(&staging_prefix(path))
        .tempdir_in(path.parent().expect("file parent"))
        .map_err(|error| io_fail("could not stage", path, error))
}

pub(super) struct Stashed {
    path: PathBuf,
    parked: PathBuf,
    work: TempDir,
}

impl Stashed {
    pub(super) fn new(path: &Path, expected: &Entry) -> Result<Self, Fail> {
        let work = staging(path)?;
        let parked = work.path().join("original");
        if needed(path)? != *expected {
            return Err(Fail::new(
                1,
                format!("{} changed before removal", path.display()),
            ));
        }
        fs::rename(path, &parked)
            .map_err(|error| io_fail("could not stage source", path, error))?;
        let stash = Self {
            path: path.into(),
            parked,
            work,
        };
        if !snapshot(&stash.parked).is_ok_and(|entry| entry.as_ref() == Some(expected)) {
            return Err(edit::rollback(
                Fail::new(1, format!("{} changed during removal", path.display())),
                [stash.restore()],
            ));
        }
        Ok(stash)
    }

    pub(super) fn restore(self) -> Result<(), Fail> {
        if let Err(error) = rename_noclobber(&self.parked, &self.path) {
            return Err(retained(self.work, &self.path, error));
        }
        Ok(())
    }
}

pub(super) struct Replacement {
    path: PathBuf,
    pub(super) parked: PathBuf,
    work: Option<TempDir>,
    original: Entry,
    installed: Option<Entry>,
}

impl Replacement {
    pub(super) fn new(path: &Path, original: Entry) -> Result<Self, Fail> {
        let work = staging(path)?;
        let parked = work.path().join("original");
        Ok(Self {
            path: path.into(),
            parked,
            work: Some(work),
            original,
            installed: None,
        })
    }

    pub(super) fn install(&mut self) -> Result<(), Fail> {
        if needed(&self.path)? != self.original {
            return Err(Fail::new(
                1,
                format!("{} changed before replacement", self.path.display()),
            ));
        }
        let installed = needed(&self.parked)?;
        #[cfg(test)]
        super::edit::tests::before_config_replace();
        exchange(&self.path, &self.parked)
            .map_err(|error| io_fail("could not replace", &self.path, error))?;
        self.installed = Some(installed);
        if !snapshot(&self.parked).is_ok_and(|entry| entry.as_ref() == Some(&self.original))
            || !snapshot(&self.path).is_ok_and(|entry| entry == self.installed)
        {
            let error = Fail::new(
                1,
                format!("{} changed during replacement", self.path.display()),
            );
            return Err(edit::rollback(error, [self.undo()]));
        }
        Ok(())
    }

    fn restore(mut self) -> Result<(), Fail> {
        self.undo()
    }

    fn undo(&mut self) -> Result<(), Fail> {
        let result = (|| {
            if snapshot(&self.path)? != self.installed {
                return Err(io::Error::other("the target has a later edit"));
            }
            let original = snapshot(&self.parked)?;
            exchange(&self.path, &self.parked)?;
            if snapshot(&self.parked)? != self.installed {
                if snapshot(&self.path)? == original {
                    exchange(&self.path, &self.parked)?;
                }
                return Err(io::Error::other("the target changed during rollback"));
            }
            Ok(())
        })();
        result.map_err(|error| {
            retained(
                self.work.take().expect("staging is active"),
                &self.path,
                error,
            )
        })
    }
}

fn retained(work: TempDir, path: &Path, error: io::Error) -> Fail {
    let backup = work.keep().join("original");
    Fail::new(
        1,
        format!(
            "could not restore {}: {error}; saved entry at {}",
            path.display(),
            backup.display()
        ),
    )
}

// Trade two directory entries atomically so we can inspect the displaced file,
// even when a writer changes the target just before the system call.
pub(super) fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    rename(a, b, true).map_err(exchange_error)
}

fn exchange_error(error: io::Error) -> io::Error {
    if error
        .raw_os_error()
        .is_some_and(|errno| [libc::ENOSYS, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&errno))
    {
        io::Error::new(
            error.kind(),
            format!("{error}. This filesystem or system does not support atomic exchange."),
        )
    } else {
        error
    }
}

pub(super) fn rename_noclobber(a: &Path, b: &Path) -> io::Result<()> {
    rename(a, b, false)
}

fn rename(a: &Path, b: &Path, exchange: bool) -> io::Result<()> {
    let a = CString::new(a.as_os_str().as_bytes())?;
    let b = CString::new(b.as_os_str().as_bytes())?;
    // The C strings stay live for the atomic exchange and are never retained.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renamex_np(
            a.as_ptr(),
            b.as_ptr(),
            if exchange {
                libc::RENAME_SWAP
            } else {
                libc::RENAME_EXCL
            },
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            if exchange {
                libc::RENAME_EXCHANGE
            } else {
                libc::RENAME_NOREPLACE
            },
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::{Env, Flags, config::Root, packages::github::test_env};

    thread_local! {
        static BEFORE_LINK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(super) fn before_link_install() {
        if let Some(before_link) = BEFORE_LINK.take() {
            before_link();
        }
    }

    fn fixture() -> (TempDir, Ctx) {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let base = directory.path().canonicalize().unwrap();
        let home = base.join("home");
        let root = base.join("config");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&root).unwrap();
        let env = Env {
            vars: test_env::private_environment(&base, &home, &root)
                .into_iter()
                .collect(),
            cwd: root.clone(),
        };
        let ctx = Ctx {
            root: Root::resolve(&env).unwrap(),
            env,
            flags: Flags::default(),
        };
        (directory, ctx)
    }

    #[test]
    fn adoption_keeps_the_source_if_a_later_edit_retains_its_declaration() {
        let (_directory, ctx) = fixture();
        let config = ctx.root.path().join("config.toml");
        fs::write(&config, "").unwrap();
        let target = ctx.env.home().unwrap().join("settings");
        fs::write(&target, "original").unwrap();
        let paths = paths(&ctx, &target).unwrap();
        let source = paths.source.clone();
        let original = snapshot(&target).unwrap();
        let config_to_edit = config.clone();
        let target_to_edit = target.clone();
        BEFORE_LINK.set(Some(Box::new(move || {
            fs::write(&target_to_edit, "replacement").unwrap();
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&config_to_edit)
                .unwrap();
            writeln!(file, "# keep my edit").unwrap();
        })));
        let error = Adoption {
            paths,
            config: config.clone(),
            original,
        }
        .execute(&ctx)
        .unwrap_err();
        assert!(error.diagnostic.contains("kept a later edit"), "{error}");
        assert_eq!(fs::read_to_string(target).unwrap(), "replacement");
        let text = fs::read_to_string(config).unwrap();
        assert!(
            text.contains("# keep my edit") && text.contains("dotfiles/settings"),
            "{text}"
        );
        assert_eq!(fs::read_to_string(source).unwrap(), "original");
    }

    #[test]
    fn removal_restores_a_parked_source_edited_during_config_install() {
        let (_directory, ctx) = fixture();
        let target = ctx.env.home().unwrap().join("settings");
        let paths = paths(&ctx, &target).unwrap();
        let source = paths.source.clone();
        let config = ctx.root.path().join("config.toml");
        let declaration =
            "[dotfiles]\n\"~/settings\" = { source = \"dotfiles/settings\", mode = \"symlink\" }\n";
        fs::write(&config, declaration).unwrap();
        fs::create_dir(source.parent().unwrap()).unwrap();
        fs::write(&source, b"original\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&source, &target).unwrap();
        let original = fs::metadata(&source).unwrap();
        let mut writer = fs::OpenOptions::new().append(true).open(&source).unwrap();
        let removal = prepare_remove(&ctx, paths, None).unwrap();
        let source_path = source.clone();
        edit::tests::on_next_exchange(move || {
            // The first exchange replaces the symlink. The second edits the
            // configuration while the open source inode is parked.
            edit::tests::on_next_exchange(move || {
                assert!(!source_path.exists());
                writer.write_all(b"late edit\n").unwrap();
                writer.sync_all().unwrap();
            });
        });

        let error = removal.execute(&ctx).unwrap_err();
        assert_eq!(error.status, 1, "{error}");
        assert!(error.diagnostic.contains("source changed"), "{error}");
        assert_eq!(fs::read_link(&target).unwrap(), source);
        let restored = fs::metadata(&source).unwrap();
        assert_eq!(restored.ino(), original.ino());
        assert_eq!(restored.mode(), original.mode());
        assert_eq!(fs::read(&target).unwrap(), b"original\nlate edit\n");
        assert_eq!(fs::read_to_string(config).unwrap(), declaration);
    }

    #[test]
    fn exchange_explains_only_unambiguous_unsupported_errors() {
        for errno in [libc::ENOSYS, libc::ENOTSUP, libc::EOPNOTSUPP] {
            let error = exchange_error(io::Error::from_raw_os_error(errno));
            assert!(
                error
                    .to_string()
                    .contains("does not support atomic exchange")
            );
        }
        for errno in [libc::EINVAL, libc::ENOENT, libc::EACCES, libc::EXDEV] {
            let error = exchange_error(io::Error::from_raw_os_error(errno));
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }
}
