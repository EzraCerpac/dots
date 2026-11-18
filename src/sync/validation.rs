use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Seek, Write};
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;
use toml_edit::{Document, DocumentMut, Item, Key, Table};

use crate::{Env, config, process::Mise, run::Run};

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
    pub retired: Vec<Retirement>,
    local: BTreeMap<PathBuf, Image>,
}

impl Validated {
    pub fn still_current(&self, root: &Path) -> io::Result<bool> {
        Ok(self.local == local_files(root)?)
    }
}

pub(super) struct Retirement {
    pub path: PathBuf,
    pub before: Image,
    pub after: Image,
}

pub(super) fn validate(run: &Run<'_>, scratch: &Scratch<'_>, tree: &Tree) -> io::Result<Validated> {
    let preview = private_dir(scratch.dir.path(), "validate-")?;
    let root = preview.path().canonicalize()?;
    let mut documents = materialize(scratch, tree, &root)?;
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
        ignored.push(path.clone());
    }
    check_ignored(&scratch.live, &root, &ignored)?;
    let retired = retire(&root, &documents, &local)?;
    for (path, before) in &local {
        let image = retired
            .iter()
            .find(|row| row.path == *path)
            .map_or(before, |row| &row.after);
        write_private(&root.join(path), image)?;
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
    config::selection::read(&selected_root, &env).map_err(io::Error::other)?;
    let config = probe(run, &root, ["config", "ls", "--json"], true)?;
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
    probe(
        run,
        &root,
        [
            "bootstrap",
            "dotfiles",
            "apply",
            "--dry-run",
            "--force",
            "--yes",
        ],
        true,
    )?;
    Ok(Validated { retired, local })
}

fn local_files(root: &Path) -> io::Result<BTreeMap<PathBuf, Image>> {
    let anchored = Dir::open(root)?;
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(root)? {
        let path = PathBuf::from(entry?.file_name());
        if local(&path) && !matches!(path.to_str(), Some("locks" | ".setup-state")) {
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
    tree: &Tree,
    root: &Path,
) -> io::Result<BTreeMap<PathBuf, DocumentMut>> {
    let mut documents = BTreeMap::new();
    for (path, entry) in tree {
        if local(path) {
            return Err(invalid("configuration tracks a machine-local file"));
        }
        let image = Image::from_git(&scratch.git, Some(entry))?;
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

pub(super) fn historical_status(
    run: &Run<'_>,
    scratch: &Scratch<'_>,
    tree: &Tree,
) -> io::Result<(tempfile::TempDir, PathBuf, Vec<u8>)> {
    let preview = private_dir(scratch.dir.path(), "historical-")?;
    let root = preview.path().canonicalize()?;
    materialize(scratch, tree, &root)?;
    for (path, image) in local_files(scratch.live.root())? {
        write_private(&root.join(path), &image)?;
    }
    let data = probe(
        run,
        &root,
        ["bootstrap", "dotfiles", "status", "--json"],
        true,
    )?;
    Ok((preview, root, data))
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
    let selected =
        config::selection::read(&run.ctx().root, &run.ctx().env).map_err(io::Error::other)?;
    cmd.current_dir(root)
        .env("MISE_ENV", selected.active_environments())
        .env("MISE_CONFIG_DIR", root)
        .env("MISE_AUTO_INSTALL", "0")
        .env("MISE_EXEC_AUTO_INSTALL", "0")
        .stdin(Stdio::null());
    if preview {
        // The validation checkout has no user-provided global config discovery.
        // Retain the real HOME and selection so native rendering resolves paths
        // and machine overrides the same way it does after installation.
        cmd.env("MISE_TRUSTED_CONFIG_PATHS", root)
            .env_remove("MISE_GLOBAL_CONFIG_FILE")
            .env_remove("MISE_CONFIG_FILE")
            .env(
                "MISE_SYSTEM_CONFIG_DIR",
                root.join(".dots-no-system-config"),
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

pub(super) fn probe(
    run: &Run<'_>,
    root: &Path,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    preview: bool,
) -> io::Result<Vec<u8>> {
    if run.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let output = run.probe(command(run, root, args, preview)?)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(invalid(
            "mise rejected the configuration or dotfile operation",
        ))
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
        match fs::symlink_metadata(root.join(source)) {
            Ok(_) => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                ()
            }
            Err(error) => return Err(error),
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
            for name in names {
                let table = entries
                    .get(&name)
                    .and_then(Item::as_table)
                    .ok_or_else(|| invalid("retired declaration changed while editing"))?;
                ranges.push(declaration_range(original, table)?);
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
            })
        })
        .collect()
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
        let result = retire(root, &documents, &files).unwrap();
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
        retire(fixture.path(), &docs, &machine)
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
