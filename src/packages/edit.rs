use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::Fail;
use crate::config::Doc;
use crate::targets::{Declaration, Section};
use toml_edit::{DocumentMut, Item, Table, TableLike};

use super::{
    adopt::{Replacement, needed},
    io_fail, missing,
};

pub const TOOLS: &[&str] = &["tools"];
pub const PACKAGES: &[&str] = &["bootstrap", "packages"];
pub const DOTFILES: &[&str] = &["dotfiles"];

pub fn table<'a>(
    doc: &'a DocumentMut,
    section: &[&str],
) -> Result<Option<&'a dyn TableLike>, Fail> {
    let mut current: &dyn TableLike = doc.as_table();
    for key in section {
        let Some(item) = current.get(key) else {
            return Ok(None);
        };
        current = item
            .as_table_like()
            .ok_or_else(|| Fail::usage(format!("{key} must be a TOML table")))?;
    }
    Ok(Some(current))
}

pub fn entry<'a>(
    doc: &'a DocumentMut,
    section: &[&str],
    key: &str,
) -> Result<Option<&'a Item>, Fail> {
    Ok(table(doc, section)?.and_then(|table| table.get(key)))
}

fn table_mut<'a>(
    doc: &'a mut DocumentMut,
    section: &[&str],
    create: bool,
) -> Result<&'a mut dyn TableLike, Fail> {
    let mut current: &mut dyn TableLike = doc.as_table_mut();
    for (index, key) in section.iter().enumerate() {
        if create && !current.contains_key(key) {
            let mut table = Table::new();
            table.set_implicit(index + 1 < section.len());
            current.insert(key, Item::Table(table));
        }
        current = current
            .get_mut(key)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| Fail::usage(format!("{key} must be a TOML table")))?;
    }
    Ok(current)
}

pub fn put(
    doc: &mut DocumentMut,
    section: &[&str],
    key: &str,
    mut value: Item,
) -> Result<(), Fail> {
    let table = table_mut(doc, section, true)?;
    if let Some(old) = table.get_mut(key) {
        if let (Some(old), Some(value)) = (old.as_value(), value.as_value_mut()) {
            *value.decor_mut() = old.decor().clone();
        }
        *old = value;
    } else {
        table.insert(key, value);
    }
    Ok(())
}

pub fn delete(doc: &mut DocumentMut, section: &[&str], key: &str) -> Result<(), Fail> {
    if table(doc, section)?.is_some() {
        table_mut(doc, section, false)?.remove(key);
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Contents {
    bytes: Vec<u8>,
    mode: u32,
}

fn contents(path: &Path) -> io::Result<Option<Contents>> {
    let Some(metadata) = missing(fs::symlink_metadata(path))? else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    Ok(Some(Contents {
        bytes: fs::read(path)?,
        mode: metadata.permissions().mode(),
    }))
}

pub struct Edit {
    pub path: PathBuf,
    pub doc: Doc,
    before: Option<Contents>,
}

impl Edit {
    pub fn open(path: &Path) -> Result<Self, Fail> {
        let before = contents(path).map_err(|error| io_fail("could not read", path, error))?;
        let edit = Self {
            path: path.into(),
            doc: Doc::open(path)?,
            before,
        };
        edit.check()?;
        Ok(edit)
    }

    pub fn check(&self) -> Result<(), Fail> {
        let current =
            contents(&self.path).map_err(|error| io_fail("could not read", &self.path, error))?;
        if current != self.before {
            return Err(Fail::new(
                1,
                format!(
                    "configuration changed while inspecting {}",
                    self.path.display()
                ),
            ));
        }
        Ok(())
    }

    pub fn install(self) -> Result<Option<Written>, Fail> {
        let rendered = self.doc.document.to_string().into_bytes();
        let bytes = self.before.as_ref().map_or_else(
            || rendered.clone(),
            |before| preserve_line_endings(&before.bytes, &rendered),
        );
        self.check()?;
        if self.before.as_ref().is_some_and(|old| old.bytes == bytes) {
            return Ok(None);
        }
        if let Some(before) = &self.before {
            let original = needed(&self.path)?;
            let mut replacement = Replacement::new(&self.path, original)?;
            let file = stage_bytes(&replacement.parked, &bytes, before.mode)
                .map_err(|error| io_fail("could not stage", &self.path, error))?;
            file.persist_noclobber(&replacement.parked)
                .map_err(|error| io_fail("could not stage", &self.path, error.error))?;
            self.check()?;
            replacement.install()?;
        } else {
            let file = stage_bytes(&self.path, &bytes, 0o600)
                .map_err(|error| io_fail("could not stage", &self.path, error))?;
            file.persist_noclobber(&self.path)
                .map_err(|error| io_fail("could not install", &self.path, error.error))?;
        }
        Ok(Some(Written {
            path: self.path,
            before: self.before,
            bytes,
        }))
    }
}

fn preserve_line_endings(before: &[u8], rendered: &[u8]) -> Vec<u8> {
    fn line_parts(line: &[u8]) -> (&[u8], &[u8]) {
        if let Some(text) = line.strip_suffix(b"\r\n") {
            (text, b"\r\n")
        } else if let Some(text) = line.strip_suffix(b"\n") {
            (text, b"\n")
        } else {
            (line, b"")
        }
    }

    let original: Vec<_> = before
        .split_inclusive(|byte| *byte == b'\n')
        .map(line_parts)
        .collect();
    let mut positions: BTreeMap<&[u8], VecDeque<usize>> = BTreeMap::new();
    for (index, (text, _)) in original.iter().enumerate() {
        positions.entry(text).or_default().push_back(index);
    }
    let mut next_original = 0;
    let mut ending: &[u8] = b"\n";
    let mut output = Vec::with_capacity(rendered.len());
    let mut lines = rendered.split_inclusive(|byte| *byte == b'\n').peekable();
    while let Some(line) = lines.next() {
        let (text, _) = line_parts(line);
        let matched = positions.get_mut(text).and_then(|positions| {
            while positions
                .front()
                .is_some_and(|index| *index < next_original)
            {
                positions.pop_front();
            }
            positions.pop_front()
        });
        if let Some(index) = matched {
            next_original = index + 1;
        }
        if let Some((_, original_ending)) = matched
            .or_else(|| (next_original == 0).then_some(0))
            .and_then(|index| original.get(index))
            && !original_ending.is_empty()
        {
            ending = original_ending;
        }
        output.extend_from_slice(text);
        if lines.peek().is_some() || before.ends_with(b"\n") || before.is_empty() {
            output.extend_from_slice(ending);
        }
    }
    output
}

fn stage_bytes(path: &Path, bytes: &[u8], mode: u32) -> io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().expect("absolute config path"))?;
    file.write_all(bytes)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.as_file().sync_all()?;
    Ok(file)
}

pub struct Written {
    path: PathBuf,
    before: Option<Contents>,
    bytes: Vec<u8>,
}

pub fn section(declaration: &Declaration) -> &'static [&'static str] {
    match declaration.section {
        Section::Tools => TOOLS,
        Section::Packages => PACKAGES,
    }
}

impl Written {
    pub fn restore(self) -> Result<(), Fail> {
        self.restore_declarations(&[])
    }

    pub fn restore_declarations(self, removed: &[Declaration]) -> Result<(), Fail> {
        let path = &self.path;
        let current = contents(path).map_err(|error| io_fail("could not inspect", path, error))?;
        if current.as_ref().is_some_and(|current| {
            current.bytes == self.bytes
                && self.before.as_ref().map_or(0o600, |old| old.mode & 0o7777)
                    == current.mode & 0o7777
        }) {
            return self.restore_exact(current.expect("checked current contents"));
        }
        if let (Some(before), Some(_)) = (&self.before, current)
            && !removed.is_empty()
        {
            // Restore our keys into the later version of the file. Preserve
            // unrelated edits and leave any replacement of a removed key alone.
            let original: DocumentMut = std::str::from_utf8(&before.bytes)
                .expect("parsed before editing")
                .parse()
                .expect("parsed before editing");
            let mut edit = Edit::open(path)?;
            let mut conflicts = Vec::new();
            for declaration in removed
                .iter()
                .filter(|declaration| &declaration.path == path)
            {
                if let Err(error) = restore_entry(&original, &mut edit.doc.document, declaration) {
                    conflicts.push(error.to_string());
                }
            }
            edit.install()?;
            if conflicts.is_empty() {
                return Ok(());
            }
            return Err(Fail::new(1, conflicts.join("; ")));
        }
        Err(Fail::new(
            1,
            format!("kept a later edit to {}", path.display()),
        ))
    }

    fn restore_exact(self, current: Contents) -> Result<(), Fail> {
        let path = &self.path;
        let unchanged = || contents(path).is_ok_and(|contents| contents.as_ref() == Some(&current));
        let changed = || Fail::new(1, format!("kept a later edit to {}", path.display()));
        let Some(before) = self.before else {
            let owned = super::adopt::needed(path)?;
            if !unchanged() {
                return Err(changed());
            }
            return super::adopt::Stashed::new(path, &owned).map(drop);
        };
        let backup = stage_bytes(path, &before.bytes, before.mode)
            .map_err(|error| io_fail("could not stage original", path, error))?;
        if !unchanged() {
            return Err(changed());
        }
        let swap = || {
            super::adopt::exchange(path, backup.path())
                .map_err(|error| io_fail("could not restore", path, error))
        };
        swap()?;
        if contents(backup.path()).is_ok_and(|contents| contents.as_ref() == Some(&current)) {
            return Ok(());
        }
        if contents(path).is_ok_and(|contents| contents.as_ref() == Some(&before)) && swap().is_ok()
        {
            return Err(changed());
        }
        let (_, remaining) = backup
            .keep()
            .map_err(|error| io_fail("could not retain", path, error.error))?;
        Err(Fail::new(
            1,
            format!(
                "{} changed during rollback; saved displaced file at {}",
                path.display(),
                remaining.display()
            ),
        ))
    }
}

fn restore_entry(
    original: &DocumentMut,
    current: &mut DocumentMut,
    declaration: &Declaration,
) -> Result<(), Fail> {
    let section = section(declaration);
    let previous = table(original, section)?.expect("removed declaration's section");
    let key = &declaration.key;
    let (original_key, value) = previous.get_key_value(key).expect("removed declaration");
    let current = table_mut(current, section, false)?;
    if let Some(replacement) = current.get(key) {
        return if replacement.to_string() == value.to_string() {
            Ok(())
        } else {
            Err(Fail::new(
                1,
                format!(
                    "kept a later replacement for {key} in {}",
                    declaration.path.display()
                ),
            ))
        };
    }
    let mut ordered: Vec<_> = current
        .iter()
        .map(|(key, item)| (current.key(key).expect("table entry").clone(), item.clone()))
        .collect();
    let mut original_order = previous.iter().skip_while(|(name, _)| *name != key);
    original_order.next();
    let position = original_order
        .find_map(|(next, _)| ordered.iter().position(|(key, _)| key.get() == next))
        .unwrap_or(ordered.len());
    ordered.insert(position, (original_key.clone(), value.clone()));
    current.clear();
    for (key, item) in ordered {
        current.entry_format(&key).or_insert(item);
    }
    Ok(())
}

pub fn rollback(error: Fail, restores: impl IntoIterator<Item = Result<(), Fail>>) -> Fail {
    let errors: Vec<_> = restores
        .into_iter()
        .filter_map(Result::err)
        .map(|error| error.to_string())
        .collect();
    if errors.is_empty() {
        error
    } else {
        Fail::new(
            1,
            format!("{error}; rollback incomplete: {}", errors.join("; ")),
        )
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::cell::RefCell;

    use super::*;

    thread_local! {
        static BEFORE_REPLACE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(in crate::packages) fn before_config_replace() {
        if let Some(before_replace) = BEFORE_REPLACE.take() {
            before_replace();
        }
    }

    #[test]
    fn edit_rejects_a_later_file_at_the_replacement_boundary() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, b"[tools]\r\nknown = 1\r\n").unwrap();
        let mut edit = Edit::open(&path).unwrap();
        put(&mut edit.doc.document, TOOLS, "new", toml_edit::value(2)).unwrap();
        let destination = path.clone();
        BEFORE_REPLACE.set(Some(Box::new(move || {
            fs::write(destination, "# later edit\n").unwrap();
        })));
        assert!(edit.install().is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "# later edit\n");
    }

    #[test]
    fn preserves_original_line_endings_and_final_newline() {
        let before = b"# fixture\r\n[dotfiles]\r\n";
        assert_eq!(
            preserve_line_endings(before, b"# fixture\n[dotfiles]\nnew = 1\n"),
            b"# fixture\r\n[dotfiles]\r\nnew = 1\r\n"
        );
        assert_eq!(
            preserve_line_endings(b"[tools]\nknown = 1", b"[tools]\nknown = 1\nnew = 2\n"),
            b"[tools]\nknown = 1\nnew = 2"
        );
        assert_eq!(
            preserve_line_endings(
                b"# leading\r\n[tools]\nknown = 1\r\n# trailing\n",
                b"# leading\n[tools]\nknown = 1\nnew = 2\n# trailing\n",
            ),
            b"# leading\r\n[tools]\nknown = 1\r\nnew = 2\r\n# trailing\n"
        );
    }
}
