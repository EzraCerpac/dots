use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Seek, Write};
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use serde::Deserialize;
use toml_edit::{Document, DocumentMut, Item, Key, Table};

use crate::{Env, config, process::Mise, run::Run};

use super::dotfiles::ApplyPlan;
use super::git::{Git, Tree, checked, invalid, local};
use super::guard::{Dir, Image, private_dir, write_private};
use super::snapshot::Scratch;

#[derive(Debug)]
pub(super) struct ConfigLocation {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for ConfigLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?}: line {}, column {}",
            self.path, self.line, self.column
        )
    }
}
impl std::error::Error for ConfigLocation {}

pub(super) struct Validated {
    _preview: tempfile::TempDir,
    revision: String,
    pub apply: ApplyPlan,
    pub retired: Vec<Retirement>,
    local: BTreeMap<PathBuf, Image>,
    configs: BTreeMap<PathBuf, Image>,
}

impl Validated {
    pub(super) fn revision(&self) -> &str {
        &self.revision
    }

    pub fn still_current(&self, root: &Path) -> io::Result<bool> {
        Ok(self.local == local_files(root)?)
    }

    pub fn ready_to_apply(&self, git: &Git<'_>) -> io::Result<()> {
        self.check_live_inputs(git)?;
        self.check_overlay_locals()?;
        let preview = Dir::open(self._preview.path())?;
        for (path, expected) in &self.configs {
            if !preview.read_path(path)?.git_eq(expected) {
                return Err(invalid("validated configuration changed"));
            }
        }
        Ok(())
    }

    pub fn check_live_inputs(&self, git: &Git<'_>) -> io::Result<()> {
        super::verify_dev(git, &self.revision)?;
        let root = git.root();
        self.check_local_inputs(root)?;
        let live = Dir::open(root)?;
        for (path, expected) in &self.configs {
            if !live.read_path(path)?.git_eq(expected) {
                return Err(invalid("configuration inputs changed after validation"));
            }
        }
        Ok(())
    }

    pub fn check_overlay_locals(&self) -> io::Result<()> {
        self.check_local_inputs(self._preview.path())
    }

    fn check_local_inputs(&self, root: &Path) -> io::Result<()> {
        let mut local = local_files(root)?;
        for (path, original) in &self.local {
            let expected = self
                .retired
                .iter()
                .find(|row| &row.path == path)
                .map_or(original, |row| &row.after);
            if local.remove(path).as_ref() != Some(expected) {
                return Err(invalid("machine-local inputs changed after validation"));
            }
        }
        if !local.is_empty() {
            return Err(invalid("machine-local inputs changed after validation"));
        }
        Ok(())
    }
}

pub(super) struct Retirement {
    pub path: PathBuf,
    pub before: Image,
    pub after: Image,
    pub sources: BTreeSet<PathBuf>,
}

pub(super) fn validate(
    run: &Run<'_>,
    scratch: &Scratch<'_>,
    revision: &str,
    tree: &Tree,
    current: &Tree,
) -> io::Result<Validated> {
    let preview = private_dir(scratch.dir.path(), "validate-")?;
    let root = preview.path().canonicalize()?;
    let mut documents = materialize(scratch, revision, tree, &root)?;
    let local = local_files(scratch.live.root())?;
    let mut ignored = vec![
        PathBuf::from("config.local.toml"),
        PathBuf::from("miserc.toml"),
        PathBuf::from(format!("config.host-{}.toml", scratch.nonce()?)),
        PathBuf::from("locks/dots-sync"),
        PathBuf::from(".setup-state/dots-sync"),
    ];
    for (path, image) in &local {
        if is_mise_config(path) {
            let Image::Regular { bytes, .. } = image else {
                unreachable!()
            };
            documents.insert(path.clone(), parse_at(path, bytes)?);
        }
        write_private(&root.join(path), image)?;
        ignored.push(path.clone());
    }
    protect_future_hosts(&root)?;
    check_ignored(&scratch.live, &root, &ignored)?;
    let retired = retire(&root, &documents, &local, |source| {
        Ok(!current.contains_key(source) && source_exists(scratch.live.root(), source)?)
    })?;
    for row in &retired {
        let path = root.join(&row.path);
        fs::remove_file(&path)?;
        write_private(&path, &row.after)?;
    }
    if !matches!(fs::symlink_metadata(root.join("config.toml")), Ok(md) if md.is_file()) {
        return Err(invalid("merged configuration has no config.toml"));
    }

    let mut vars = run.ctx().env.vars.clone();
    vars.insert("DOTS_ROOT".into(), root.as_os_str().to_owned());
    let env = Env {
        vars,
        cwd: root.clone(),
    };
    let selected_root = config::Root::resolve(&env).map_err(io::Error::other)?;
    config::Vars::read(&selected_root).map_err(io::Error::other)?;
    let selection = config::selection::read(&selected_root, &env).map_err(io::Error::other)?;
    let config = probe(run, &root, MiseProbe::ConfigList, true)?;
    #[derive(Deserialize)]
    struct Entry {
        path: PathBuf,
    }
    let files: Vec<Entry> = serde_json::from_slice(&config).map_err(io::Error::other)?;
    if !files
        .iter()
        .any(|entry| entry.path == root.join("config.toml"))
        || files.iter().any(|entry| {
            !entry
                .path
                .canonicalize()
                .is_ok_and(|path| path.starts_with(&root))
        })
    {
        return Err(invalid("mise did not load the isolated configuration"));
    }
    // Native directory manifests resolve their names from the verified index.
    let preview_git = scratch.live.at(&root);
    let format = scratch.git.text(["rev-parse", "--show-object-format"])?;
    preview_git.get([
        "init",
        "--quiet",
        "--template=",
        &format!("--object-format={format}"),
    ])?;
    let objects = scratch
        .live
        .file_path([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])?
        .canonicalize()?;
    let mut alternate = objects.as_os_str().as_encoded_bytes().to_vec();
    if alternate.contains(&b'\n') {
        return Err(invalid("unsupported Git object directory"));
    }
    alternate.push(b'\n');
    fs::write(root.join(".git/objects/info/alternates"), alternate)?;
    preview_git.get(["read-tree", revision])?;
    let apply = ApplyPlan::new(
        run,
        root.clone(),
        scratch.live.root(),
        selection.active_environments(),
    )?;
    materialize_sources(scratch, revision, tree, &root, &apply)?;
    probe(run, &root, MiseProbe::DotfilePreview, true)?;
    let private = Dir::open(&root)?;
    let mut configs = BTreeMap::new();
    for file in files {
        let path = file.path.strip_prefix(&root).map_err(io::Error::other)?;
        if !local.contains_key(path) {
            configs.insert(path.to_owned(), private.read_path(path)?);
        }
    }
    Ok(Validated {
        _preview: preview,
        revision: revision.into(),
        apply,
        retired,
        local,
        configs,
    })
}

fn local_files(root: &Path) -> io::Result<BTreeMap<PathBuf, Image>> {
    let anchored = Dir::open(root)?;
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(root)? {
        let path = PathBuf::from(entry?.file_name());
        let name = path.as_os_str().as_bytes();
        if local(&path)
            && !name.eq_ignore_ascii_case(b"locks")
            && !name.eq_ignore_ascii_case(b".setup-state")
        {
            let image = anchored.read_path(&path)?;
            if !matches!(image, Image::Regular { .. }) {
                return Err(invalid("machine-local config is not a regular file"));
            }
            files.insert(path, image);
        }
    }
    Ok(files)
}

fn materialize(
    scratch: &Scratch<'_>,
    revision: &str,
    tree: &Tree,
    root: &Path,
) -> io::Result<BTreeMap<PathBuf, DocumentMut>> {
    let mut documents = BTreeMap::new();
    for (path, entry) in tree {
        if local(path) {
            return Err(invalid("configuration tracks a machine-local file"));
        }
        let image = if checkout_config(path) {
            Image::checkout(&scratch.git, revision, path, Some(entry))?
        } else {
            Image::from_git(&scratch.git, Some(entry))?
        };
        let destination = root.join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        write_private(&destination, &image)?;
        if is_mise_config(path) {
            let Image::Regular { bytes, .. } = image else {
                return Err(invalid("configuration file is not a regular file"));
            };
            documents.insert(path.clone(), parse_at(path, &bytes)?);
        }
    }
    Ok(documents)
}

fn checkout_config(path: &Path) -> bool {
    is_mise_config(path)
        || (path.starts_with("conf.d") && path.extension().is_some_and(|ext| ext == "toml"))
}

fn materialize_sources(
    scratch: &Scratch<'_>,
    revision: &str,
    tree: &Tree,
    root: &Path,
    apply: &ApplyPlan,
) -> io::Result<()> {
    let sources: BTreeSet<_> = apply
        .sources()
        .filter_map(|source| source.strip_prefix(scratch.live.root()).ok())
        .collect();
    // Apply checkout filters to the selected source files and directory
    // leaves. Other incoming files are still prepared during checkout.
    for (path, entry) in tree {
        if checkout_config(path)
            || !matches!(entry.mode.as_str(), "100644" | "100755")
            || !sources.iter().any(|source| path.starts_with(source))
        {
            continue;
        }
        let image = Image::checkout(&scratch.git, revision, path, Some(entry))?;
        let destination = root.join(path);
        fs::remove_file(&destination)?;
        write_private(&destination, &image)?;
    }
    Ok(())
}

pub(super) fn overlay_environments(run: &Run<'_>, root: &Path) -> io::Result<String> {
    let mut vars = run.ctx().env.vars.clone();
    vars.insert("DOTS_ROOT".into(), root.as_os_str().to_owned());
    let env = Env {
        vars,
        cwd: root.to_owned(),
    };
    let private = config::Root::resolve(&env).map_err(io::Error::other)?;
    let selection = config::selection::read(&private, &env).map_err(io::Error::other)?;
    Ok(selection.active_environments())
}

pub(super) fn command(
    run: &Run<'_>,
    root: &Path,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    preview: bool,
) -> io::Result<Command> {
    let mise = Mise::new(run.ctx()).map_err(io::Error::other)?;
    let mut argv = vec![OsString::from("-C"), root.as_os_str().to_owned()];
    argv.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    let mut cmd = mise.command(argv).map_err(io::Error::other)?;
    let selected = if preview {
        overlay_environments(run, root)?
    } else {
        config::selection::read(&run.ctx().root, &run.ctx().env)
            .map_err(io::Error::other)?
            .active_environments()
    };
    cmd.current_dir(root)
        .env("MISE_ENV", selected)
        .env("MISE_CONFIG_DIR", root)
        .env("MISE_AUTO_INSTALL", "0")
        .env("MISE_EXEC_AUTO_INSTALL", "0")
        .stdin(Stdio::null());
    if preview {
        // The validation checkout has no user-provided global config discovery.
        // Retain the real HOME and selection so native rendering resolves paths
        // and machine overrides the same way it does after installation.
        cmd.env("MISE_TRUSTED_CONFIG_PATHS", root)
            .env("MISE_NO_HOOKS", "1")
            .env_remove("MISE_GLOBAL_CONFIG_FILE")
            .env_remove("MISE_CONFIG_FILE")
            .env(
                "MISE_SYSTEM_CONFIG_DIR",
                root.join(".dots-no-system-config"),
            )
            .env(
                "MISE_SYSTEM_CONFIG_FILE",
                root.join(".dots-no-system-config/config.toml"),
            )
            .env("MISE_CEILING_PATHS", root)
            .env("MISE_OFFLINE", "1")
            .env("MISE_AUTO_UPDATE", "0")
            .env("MISE_CACHE_DIR", root.join(".dots-cache"))
            .env("MISE_STATE_DIR", root.join(".dots-state"))
            .env("MISE_TMP_DIR", root.join(".dots-tmp"))
            .env_remove("MISE_LOG_FILE")
            .env("DOTS_ROOT", root);
    }
    Ok(cmd)
}

#[derive(Clone, Copy)]
pub(super) enum MiseProbe {
    ConfigList,
    DotfileStatus,
    DotfilePreview,
}

impl MiseProbe {
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::ConfigList => &["config", "ls", "--json"],
            Self::DotfileStatus => &["bootstrap", "dotfiles", "status", "--json"],
            Self::DotfilePreview => &[
                "bootstrap",
                "dotfiles",
                "apply",
                "--dry-run",
                "--force",
                "--yes",
            ],
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ConfigList => "mise config ls",
            Self::DotfileStatus => "mise bootstrap dotfiles status",
            Self::DotfilePreview => "mise bootstrap dotfiles apply --dry-run",
        }
    }
}

#[derive(Debug)]
pub(super) struct ProbeFailure {
    operation: &'static str,
    status: ExitStatus,
}

impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.operation, self.status)
    }
}

impl std::error::Error for ProbeFailure {}

pub(super) fn probe(
    run: &Run<'_>,
    root: &Path,
    operation: MiseProbe,
    preview: bool,
) -> io::Result<Vec<u8>> {
    if run.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let output = run.probe(command(run, root, operation.args(), preview)?)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(ProbeFailure {
            operation: operation.name(),
            status: output.status,
        }))
    }
}

fn check_ignored(git: &Git<'_>, root: &Path, paths: &[PathBuf]) -> io::Result<()> {
    let git = git.at(root);
    git.get(["init", "--quiet", "--template="])?;
    let mut input = tempfile::tempfile_in(root.join(".git"))?;
    for path in paths {
        input.write_all(path.as_os_str().as_bytes())?;
        input.write_all(&[0])?;
    }
    input.rewind()?;
    let expected = paths.iter().collect::<std::collections::BTreeSet<_>>();
    let output = git
        .command([
            "-c",
            "core.excludesFile=/dev/null",
            "check-ignore",
            "--no-index",
            "-z",
            "--stdin",
        ])?
        .stdin(Stdio::from(input))
        .output()?;
    let output = checked(output)?;
    let actual = super::git::records(&output)
        .map(super::git::relative)
        .collect::<io::Result<std::collections::BTreeSet<_>>>()?;
    if !expected.iter().all(|path| actual.contains(path.as_path())) {
        return Err(invalid("merged ignore rules expose machine-local files"));
    }
    Ok(())
}

/// Check the root patterns as well as the native selector probes. One
/// ignored sample cannot prove that a specific future host stays ignored.
fn protect_future_hosts(root: &Path) -> io::Result<()> {
    const PREFIX: &[u8] = b"config.host-";
    const SUFFIX: &[u8] = b".toml";
    let Image::Regular { bytes, .. } = Dir::open(root)?.read_path(Path::new(".gitignore"))? else {
        return Err(invalid(
            "no root ignore rules protect future host selectors",
        ));
    };
    let mut protected = false;
    let patterns = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    for line in patterns.split(|&byte| byte == b'\n') {
        let mut line = line.strip_suffix(b"\r").unwrap_or(line);
        while let Some(trimmed) = line.strip_suffix(b" ") {
            line = trimmed;
        }
        if line.is_empty() || line.starts_with(b"#") || line.ends_with(b"/") {
            continue;
        }
        let negated = line.starts_with(b"!");
        if negated {
            line = &line[1..];
        }
        line = line.strip_prefix(b"/").unwrap_or(line);
        while let Some(name) = line.strip_prefix(b"**/") {
            line = name;
        }
        if !negated && line.contains(&b'/') {
            continue;
        }

        let wildcard = |byte: &u8| matches!(byte, b'*' | b'?' | b'[' | b'\\');
        let prefix = line.iter().position(wildcard).unwrap_or(line.len());
        let suffix = line.iter().rposition(wildcard).map_or(0, |last| last + 1);
        if negated {
            let first = line[..prefix].to_ascii_lowercase();
            let last = line[suffix..].to_ascii_lowercase();
            if (first.starts_with(PREFIX) || PREFIX.starts_with(&first))
                && (last.ends_with(SUFFIX) || SUFFIX.ends_with(&last))
            {
                protected = false;
            }
        } else if suffix == prefix + 1
            && line[prefix] == b'*'
            && PREFIX.starts_with(&line[..prefix])
            && SUFFIX.ends_with(&line[suffix..])
        {
            protected = true;
        }
    }
    if protected {
        Ok(())
    } else {
        Err(invalid("merged ignore rules expose future host selectors"))
    }
}

fn is_mise_config(path: &Path) -> bool {
    path.components().count() == 1
        && path.to_str().is_some_and(|name| {
            name == "miserc.toml"
                || name == "config.toml"
                || (name.starts_with("config.") && name.ends_with(".toml"))
        })
}

fn parse_at(path: &Path, bytes: &[u8]) -> io::Result<DocumentMut> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    text.parse::<DocumentMut>().map_err(|error| {
        let start = error.span().map_or(0, |span| span.start);
        let prefix = bytes.get(..start).unwrap_or_default();
        let line = prefix.iter().filter(|b| **b == b'\n').count() + 1;
        let column = prefix.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        io::Error::other(ConfigLocation {
            path: path.into(),
            line,
            column,
        })
    })
}

fn parse(bytes: &[u8]) -> io::Result<DocumentMut> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    text.parse().map_err(io::Error::other)
}

fn retire(
    root: &Path,
    docs: &BTreeMap<PathBuf, DocumentMut>,
    machine: &BTreeMap<PathBuf, Image>,
    survives_live: impl Fn(&Path) -> io::Result<bool>,
) -> io::Result<Vec<Retirement>> {
    let text = match Dir::open(root)?.read_path(Path::new("dots/retired-declarations"))? {
        Image::Absent => return Ok(Vec::new()),
        Image::Regular { bytes, .. } => String::from_utf8(bytes).map_err(io::Error::other)?,
        _ => return Err(invalid("retired declarations must be a regular file")),
    };
    let mut by_file = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for row in text.lines() {
        let Some((key, source)) = retired_row(row) else {
            continue;
        };
        let source_path = Path::new(source);
        if source_exists(root, source_path)? || survives_live(source_path)? {
            continue;
        }
        let locations = docs
            .iter()
            .filter_map(|(path, doc)| {
                let item = doc.get("dotfiles")?.as_table_like()?.get(&key)?;
                Some((path.clone(), item))
            })
            .collect::<Vec<_>>();
        let [(path, Item::Table(table))] = locations.as_slice() else {
            continue;
        };
        if !machine.contains_key(path)
            || table.is_implicit()
            || table.is_dotted()
            || table.iter().any(|(_, item)| !item.is_value())
            || table.get("source").and_then(Item::as_str) != Some(source)
        {
            continue;
        }
        by_file.entry(path.clone()).or_default().insert(key);
    }
    by_file
        .into_iter()
        .map(|(path, names)| {
            let Image::Regular { bytes, mode } = &machine[&path] else {
                unreachable!()
            };
            let original = std::str::from_utf8(bytes).map_err(io::Error::other)?;
            let parsed = Document::parse(original).map_err(io::Error::other)?;
            let Some(entries) = parsed.get("dotfiles").and_then(Item::as_table_like) else {
                return Err(invalid("missing retired dotfile declarations"));
            };
            let mut ranges = Vec::new();
            let mut sources = BTreeSet::new();
            for name in names {
                let table = entries
                    .get(&name)
                    .and_then(Item::as_table)
                    .ok_or_else(|| invalid("retired declaration changed while editing"))?;
                ranges.push(declaration_range(original, table)?);
                if let Some(source) = table.get("source").and_then(Item::as_str) {
                    sources.insert(PathBuf::from(source));
                }
            }
            let mut after = original.to_owned();

            ranges.sort_by_key(|span| (span.start, span.end));
            let mut combined: Vec<Range<usize>> = Vec::new();
            for range in ranges {
                if let Some(last) = combined.last_mut().filter(|last| last.end >= range.start) {
                    last.end = last.end.max(range.end);
                } else {
                    combined.push(range);
                }
            }
            for range in combined.into_iter().rev() {
                after.replace_range(range, "");
            }
            parse(after.as_bytes())?;
            Ok(Retirement {
                path: path.to_owned(),
                before: Image::Regular {
                    bytes: bytes.clone(),
                    mode: *mode,
                },
                after: Image::Regular {
                    bytes: after.into_bytes(),
                    mode: *mode,
                },
                sources,
            })
        })
        .collect()
}

pub(super) fn source_exists(root: &Path, path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(root.join(path)) {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn retired_row(row: &str) -> Option<(String, &str)> {
    let (path, source) = row.split_once('\t')?;
    let keys = Key::parse(path).ok()?;
    let [section, key] = keys.as_slice() else {
        return None;
    };
    if section.get() != "dotfiles"
        || key.get().is_empty()
        || source.contains(['\\', '\t'])
        || source
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return None;
    }
    Some((key.get().to_owned(), source))
}

fn declaration_range(original: &str, table: &Table) -> io::Result<Range<usize>> {
    let heading = table
        .span()
        .ok_or_else(|| invalid("retired header has no source range"))?;
    let last = table
        .iter()
        .filter_map(|(_, item)| item.span())
        .map(|span| span.end)
        .max()
        .unwrap_or(heading.end);
    let header = whole_line(original, heading);
    let mut start = header.start;
    let mut end = whole_line(original, last..last).end;
    if start == 0 {
        // Drop the separator when the first table is removed.
        end += original[end..]
            .strip_prefix("\r\n")
            .map_or_else(|| usize::from(original[end..].starts_with('\n')), |_| 2);
    } else {
        let after_previous = start - 1;
        let previous = original[..after_previous]
            .rfind('\n')
            .map_or(0, |pos| pos + 1);
        if original[previous..after_previous]
            .trim_end_matches('\r')
            .is_empty()
        {
            start = previous;
        }
    }
    Ok(start..end)
}

fn whole_line(original: &str, span: Range<usize>) -> Range<usize> {
    let start = original[..span.start].rfind('\n').map_or(0, |pos| pos + 1);
    let end = original[span.end..]
        .find('\n')
        .map_or(original.len(), |pos| span.end + pos + 1);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_future_host_selectors_even_with_a_specific_negation() {
        let fixture = tempfile::tempdir().unwrap();
        let ignore = fixture.path().join(".gitignore");
        for pattern in [
            "/config.host-*.toml\n",
            "/config.host-*.toml\n!config.host-readme.md\n",
            "!/**/config.host-tomorrow.toml\n**/config.host-*.toml\n",
            "*.toml\n!/config.toml\n",
        ] {
            fs::write(&ignore, pattern).unwrap();
            assert!(protect_future_hosts(fixture.path()).is_ok(), "{pattern}");
        }
        for pattern in [
            "/config.host-today.toml\n",
            "/config.host-*.toml\n!/config.host-tomorrow.toml\n",
            "/config.host-*.toml\n!/**/config.host-tomorrow.toml\n",
            "/config.host-*.toml\n!/config.host-[a/b].toml\n",
        ] {
            fs::write(&ignore, pattern).unwrap();
            assert!(protect_future_hosts(fixture.path()).is_err(), "{pattern}");
        }
    }

    #[test]
    fn retirement_compares_decoded_source_and_preserves_other_tables() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::create_dir(root.join("dots")).unwrap();
        fs::write(root.join("dots/retired-declarations"),
            b"dotfiles.gone\tdotfiles/gone\ndotfiles.backslash\tdotfiles\\backslash\ndotfiles.duplicate\tdotfiles/duplicate\n").unwrap();
        let config = r#"# keep this comment
[dotfiles.gone]
source = """dotfiles/gone"""

[dotfiles.backslash]
source = 'dotfiles\backslash'

[dotfiles.gone-too]
source = "dotfiles/gone"

[dotfiles.duplicate]
source = "dotfiles/duplicate"
"#;
        let path = PathBuf::from("config.local.toml");
        let documents = BTreeMap::from([
            (path.clone(), config.parse().unwrap()),
            (
                PathBuf::from("config.toml"),
                "[dotfiles.duplicate]\nsource='dotfiles/duplicate'\n"
                    .parse()
                    .unwrap(),
            ),
        ]);
        let files = BTreeMap::from([(
            path,
            Image::Regular {
                bytes: config.as_bytes().to_vec(),
                mode: 0o600,
            },
        )]);
        let result = retire(root, &documents, &files, |_| Ok(false)).unwrap();
        assert_eq!(result.len(), 1);
        let Image::Regular { bytes, mode } = &result[0].after else {
            panic!("retirement must produce a regular config")
        };
        let text = std::str::from_utf8(bytes).unwrap();
        assert_eq!(*mode, 0o600);
        assert!(text.starts_with("# keep this comment\n"));
        assert!(!text.contains("[dotfiles.gone]"));
        assert!(text.contains("[dotfiles.gone-too]"));
        assert!(text.contains("[dotfiles.backslash]"));
        assert!(text.contains("[dotfiles.duplicate]"));
    }

    #[test]
    fn retirement_rows_decode_quoted_keys_and_reject_wrong_namespaces_and_paths() {
        assert_eq!(
            retired_row(" 'dotfiles' . 'name.with space'\ttemplates/file"),
            Some(("name.with space".into(), "templates/file"))
        );
        for row in [
            "vars.setting\ttemplates/file",
            "dotfiles.name.extra\ttemplates/file",
            "dotfiles.name.\ttemplates/file",
            "dotfiles.name",
            "dotfiles.name\ttemplates/file\textra",
        ] {
            assert!(retired_row(row).is_none(), "{row:?}");
        }
        for source in [
            "/absolute",
            "../outside",
            ".",
            "a/.",
            "a/",
            "a//b",
            "a/../b",
            "a\\b",
        ] {
            assert!(
                retired_row(&format!("dotfiles.name\t{source}")).is_none(),
                "{source:?}"
            );
        }
        assert!(retired_row("dotfiles.name\t.hidden/my..file").is_some());
    }

    fn retirement_fixture(config: &str, rows: &str) -> io::Result<Vec<Retirement>> {
        retirement_fixture_with_source(config, rows, |_| Ok(false))
    }

    fn retirement_fixture_with_source(
        config: &str,
        rows: &str,
        surviving: impl Fn(&Path) -> io::Result<bool>,
    ) -> io::Result<Vec<Retirement>> {
        let fixture = tempfile::tempdir()?;
        fs::create_dir(fixture.path().join("dots"))?;
        fs::write(fixture.path().join("dots/retired-declarations"), rows)?;
        let file = PathBuf::from("config.local.toml");
        let parsed = parse(config.as_bytes())?;
        let docs = BTreeMap::from([(file.clone(), parsed)]);
        let machine = BTreeMap::from([(
            file,
            Image::Regular {
                bytes: config.as_bytes().to_vec(),
                mode: 0o600,
            },
        )]);
        retire(fixture.path(), &docs, &machine, surviving)
    }

    #[test]
    fn retirement_keeps_a_source_that_still_exists_in_the_live_checkout() {
        let live = tempfile::tempdir().unwrap();
        fs::create_dir(live.path().join("templates")).unwrap();
        fs::write(live.path().join("templates/file"), b"ignored live input").unwrap();
        let config = "[dotfiles.name]\nmode = 'template'\nsource = 'templates/file'\n";
        let rows = "dotfiles.name\ttemplates/file\n";
        assert!(
            retirement_fixture_with_source(config, rows, |path| source_exists(live.path(), path))
                .unwrap()
                .is_empty()
        );
        assert_eq!(retirement_fixture(config, rows).unwrap().len(), 1);
    }

    #[test]
    fn retirement_keeps_missing_template_sources_with_different_decoded_spellings() {
        for source in [
            "templates/other",
            "templates/./file",
            "./templates/file",
            "templates//file",
            "templates/file/",
            "templates\\file",
            "~/templates/file",
        ] {
            let local = format!("[dotfiles.name]\nmode = 'template'\nsource = '{source}'\n");
            assert!(
                retirement_fixture(&local, "dotfiles.name\ttemplates/file\n")
                    .unwrap()
                    .is_empty(),
                "{source:?}"
            );
        }
    }

    #[test]
    fn retirement_rejects_malformed_input_and_keeps_dotted_or_nested_definitions() {
        for malformed in [
            "[dotfiles.name]\nsource='templates/file'\n[dotfiles.name]\n",
            "[dotfiles.name\nsource='templates/file'\n",
            "[dotfiles.name]\nsource='templates/file\n",
            "[dotfiles.name]\nsource=['templates/file'\n",
        ] {
            assert!(retirement_fixture(malformed, "dotfiles.name\ttemplates/file\n").is_err());
        }
        for dotted_or_nested in [
            "dotfiles.name.source='templates/file'\n",
            "[dotfiles]\nname.source='templates/file'\n",
            "[dotfiles.name]\nsource='templates/file'\n[dotfiles.name.nested]\nx=1\n",
            "[[dotfiles.name]]\nsource='templates/file'\n",
        ] {
            assert!(
                retirement_fixture(dotted_or_nested, "dotfiles.name\ttemplates/file\n")
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn retirement_removes_only_the_table_and_its_separator_including_crlf() {
        let top = "# leading comment\n[env]\nKEPT = 'yes'\n";
        let bottom = "\n# following comment\n[settings]\nexperimental = false";
        let table = "[dotfiles.\"one.two\"]\nsource = '''templates/file'''\n";
        let row = "dotfiles . 'one.two'\ttemplates/file\n";
        for (local, expected) in [
            (format!("{top}\n{table}"), top.into()),
            (format!("{table}\n{top}"), top.into()),
            (
                format!("{top}\n{table}{bottom}").replace('\n', "\r\n"),
                format!("{top}{bottom}").replace('\n', "\r\n"),
            ),
        ] {
            let edits = retirement_fixture(&local, row).unwrap();
            assert_eq!(edits.len(), 1);
            assert_eq!(
                &edits[0].after,
                &Image::Regular {
                    bytes: expected.into_bytes(),
                    mode: 0o600
                }
            );
        }
    }

    #[test]
    fn invalid_herdr_rows_preserve_the_header_while_the_valid_neighbor_retires() {
        let kept = "[dotfiles.herdr]\nsource = 'templates/herdr.tera'\n";
        let local = format!("{kept}\n[dotfiles.neighbor]\nsource = 'templates/neighbor.tera'\n");
        for invalid in [
            "dotfiles.herdr",
            "env.X\ttemplates/herdr.tera",
            "dotfiles.herdr.child\ttemplates/herdr.tera",
            "dotfiles.\ttemplates/herdr.tera",
            "dotfiles.herdr\ttemplates/herdr.tera\textra",
            "dotfiles.herdr\ttemplates/.",
            "dotfiles.herdr\ttemplates/",
            "dotfiles.herdr\t.",
            "dotfiles.herdr\t../outside",
            "dotfiles.herdr\t/outside",
            "dotfiles.herdr\ttemplates\\herdr.tera",
        ] {
            assert!(retired_row(invalid).is_none(), "{invalid:?}");
            let rows = format!("{invalid}\ndotfiles.neighbor\ttemplates/neighbor.tera\n");
            let edits = retirement_fixture(&local, &rows).unwrap();
            assert_eq!(edits.len(), 1);
            assert_eq!(
                edits[0].after,
                Image::Regular {
                    bytes: kept.as_bytes().to_vec(),
                    mode: 0o600
                }
            );
        }
    }

    #[test]
    fn exact_herdr_row_keeps_nested_and_array_headers_with_the_same_missing_source() {
        let row = "dotfiles.herdr\ttemplates/herdr.tera\n";
        for header in ["[dotfiles.herdr.more]", "[[dotfiles.herdr]]"] {
            let local = format!("{header}\nsource = 'templates/herdr.tera'\n");
            assert!(
                retirement_fixture(&local, row).unwrap().is_empty(),
                "{header}"
            );
        }
        let exact = "[dotfiles.herdr]\nsource = 'templates/herdr.tera'\n";
        assert_eq!(retirement_fixture(exact, row).unwrap().len(), 1);
    }
}
