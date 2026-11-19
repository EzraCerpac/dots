use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, BufReader, Read, Seek},
    path::{Path, PathBuf},
};

use crate::Fail;

use super::failure;

const MAX_CONTENTS: u64 = 4 * 1024 * 1024 * 1024;
const MAX_METADATA: u64 = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_PATH: usize = 4096;
const LIMIT: &str = "release archive exceeds inspection limits";

#[derive(Clone, Copy)]
pub(super) enum Format {
    Zip,
    Tar,
    Gzip,
    Bzip2,
    Xz,
}

impl Format {
    pub fn detect(name: &str) -> Result<Option<Self>, Fail> {
        let name = name.to_ascii_lowercase();
        let has = |suffixes: &[&str]| suffixes.iter().any(|suffix| name.ends_with(suffix));
        if has(&[".dmg", ".pkg", ".msi", ".deb", ".rpm", ".apk"]) {
            return Err(failure(
                "unsupported installer format; use --asset to select a ZIP/tar app bundle or a prebuilt CLI asset",
            ));
        }
        if has(&[".tar.zst", ".tzst", ".7z", ".rar"]) {
            return Err(failure(
                "unsupported archive format; use --asset to select a ZIP, tar, or prebuilt CLI asset",
            ));
        }
        Ok(if name.ends_with(".zip") {
            Some(Self::Zip)
        } else if name.ends_with(".tar") {
            Some(Self::Tar)
        } else if has(&[".tar.gz", ".tgz"]) {
            Some(Self::Gzip)
        } else if has(&[".tar.bz2", ".tbz", ".tbz2"]) {
            Some(Self::Bzip2)
        } else if has(&[".tar.xz", ".txz"]) {
            Some(Self::Xz)
        } else {
            None
        })
    }
}

pub(super) fn contains_app(file: File, format: Format) -> Result<bool, Fail> {
    let file = BufReader::new(file);
    let entries = match format {
        Format::Zip => return inspect_zip(file)?.contains_app(),
        Format::Tar => inspect_tar(file)?,
        Format::Gzip => inspect_tar(flate2::read::MultiGzDecoder::new(file))?,
        Format::Bzip2 => inspect_tar(bzip2::read::MultiBzDecoder::new(file))?,
        Format::Xz => inspect_tar(lzma_rust2::XzReader::new_mem_limit(file, true, 128 * 1024))?,
    };
    entries.contains_app()
}

fn inspect_zip(reader: impl Read + Seek) -> Result<Entries, Fail> {
    let mut zip = zip::ZipArchive::new(reader).map_err(archive_error)?;
    if zip.len() > MAX_ENTRIES {
        return Err(failure(LIMIT));
    }
    zip.validate_header_consistency().map_err(archive_error)?;
    let mut entries = Entries::default();
    for index in 0..zip.len() {
        let entry = zip.by_index_data(index).map_err(archive_error)?;
        let size = entry.size();
        let name = entry.name().map_err(archive_error)?.into_owned();
        let kind = match entry.unix_mode().unwrap_or_default() & 0o170000 {
            0 if entry.is_dir() => Kind::Directory,
            0 | 0o100000 => Kind::File,
            0o040000 => Kind::Directory,
            0o120000 => {
                let mut target = String::new();
                if size > MAX_PATH as u64 {
                    return Err(failure(LIMIT));
                }
                zip.by_index(index)
                    .map_err(archive_error)?
                    .take((MAX_PATH + 1) as u64)
                    .read_to_string(&mut target)
                    .map_err(archive_error)?;
                link_path(&name, &target, false)?;
                Kind::Symlink
            }
            _ => {
                return Err(failure(format!(
                    "unsupported release archive entry: {name}"
                )));
            }
        };
        entries.add(&name, size, kind)?;
    }
    Ok(entries)
}

fn inspect_tar(reader: impl Read) -> Result<Entries, Fail> {
    // tar consumes extended GNU/PAX headers inside next(). Give each group of
    // headers a smaller budget than file data to bound metadata allocations.
    let budget = Cell::new(MAX_METADATA);
    let mut tar = tar::Archive::new(Budget {
        reader,
        remaining: &budget,
    });
    let mut entries = Entries::default();
    for entry in tar.entries().map_err(archive_error)? {
        let mut entry = entry.map_err(archive_error)?;
        let name = std::str::from_utf8(&entry.path_bytes())
            .map_err(archive_error)?
            .to_owned();
        let entry_type = entry.header().entry_type();
        let kind = if entry_type.is_file() {
            Kind::File
        } else if entry_type.is_dir() {
            Kind::Directory
        } else if entry_type.is_symlink() || entry_type.is_hard_link() {
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| failure("archive link has no target"))?;
            let target = std::str::from_utf8(&target).map_err(archive_error)?;
            let target = link_path(&name, target, entry_type.is_hard_link())?;
            if entry_type.is_hard_link() {
                Kind::Hardlink(target)
            } else {
                Kind::Symlink
            }
        } else {
            return Err(failure(format!(
                "unsupported release archive entry: {name}"
            )));
        };
        let size = entry.size();
        entries.add(&name, size, kind)?;
        budget.set(size);
        if io::copy(&mut entry, &mut io::sink()).map_err(archive_error)? != size {
            return Err(failure("truncated release archive entry"));
        }
        budget.set(MAX_METADATA);
    }
    Ok(entries)
}

fn archive_error(error: impl std::fmt::Display) -> Fail {
    failure(format!("could not inspect release archive: {error}"))
}

struct Budget<'a, R> {
    reader: R,
    remaining: &'a Cell<u64>,
}

impl<R: Read> Read for Budget<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.remaining.get();
        if remaining == 0 {
            return Err(io::Error::other(LIMIT));
        }
        let limit = buffer.len().min(remaining as usize);
        let count = self.reader.read(&mut buffer[..limit])?;
        self.remaining.set(remaining - count as u64);
        Ok(count)
    }
}

#[derive(Default)]
struct Entries {
    by_path: BTreeMap<PathBuf, Kind>,
    size: u64,
    count: usize,
}

#[derive(PartialEq)]
enum Kind {
    File,
    Directory,
    Symlink,
    Hardlink(PathBuf),
}

impl Entries {
    fn add(&mut self, name: &str, size: u64, kind: Kind) -> Result<(), Fail> {
        self.count += 1;
        self.size = self.size.checked_add(size).ok_or_else(|| failure(LIMIT))?;
        if self.count > MAX_ENTRIES || self.size > MAX_CONTENTS {
            return Err(failure(LIMIT));
        }
        let path = entry_path(name)?;
        if path.as_os_str().is_empty() && kind == Kind::Directory {
            return Ok(());
        }
        if path.as_os_str().is_empty() {
            return Err(failure("release archive entry has no file name"));
        }
        if self.by_path.insert(path, kind).is_some() {
            return Err(failure(format!("duplicate release archive path: {name}")));
        }
        Ok(())
    }

    fn contains_app(self) -> Result<bool, Fail> {
        let mut apps = BTreeSet::new();
        for (path, kind) in &self.by_path {
            if path.ancestors().skip(1).any(|parent| {
                self.by_path
                    .get(parent)
                    .is_some_and(|kind| *kind != Kind::Directory)
            }) {
                return Err(failure(format!(
                    "archive entry traverses a non-directory: {}",
                    path.display()
                )));
            }
            if let Kind::Hardlink(target) = kind
                && self.by_path.get(target) != Some(&Kind::File)
            {
                return Err(failure(format!(
                    "archive hard link has no regular file target: {}",
                    path.display()
                )));
            }
            if path.starts_with("__MACOSX")
                || !path.ends_with("Contents/Info.plist")
                || !matches!(kind, Kind::File | Kind::Hardlink(_))
            {
                continue;
            }
            if let Some(app) = path.parent().and_then(Path::parent)
                && app.extension().is_some_and(|ext| ext == "app")
            {
                apps.insert(app);
            }
        }
        let distinct = apps
            .iter()
            .filter(|app| !app.ancestors().skip(1).any(|parent| apps.contains(parent)))
            .count();
        match distinct {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(failure(
                "release contains multiple apps; automatic app installation is ambiguous",
            )),
        }
    }
}

fn entry_path(name: &str) -> Result<PathBuf, Fail> {
    if !valid_path(name) || name.split('/').any(|part| part == "..") {
        return Err(failure(format!("unsafe release archive path: {name:?}")));
    }
    Ok(name
        .split('/')
        .filter(|part| !matches!(*part, "" | "."))
        .collect())
}

fn link_path(name: &str, target: &str, hard: bool) -> Result<PathBuf, Fail> {
    let invalid = || failure(format!("unsafe release archive link: {name:?}"));
    if !valid_path(target) {
        return Err(invalid());
    }
    if hard {
        return entry_path(target);
    }
    let entry = entry_path(name)?;
    let mut resolved = entry
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf();
    let mut seen_name = false;
    for part in target.split('/').filter(|part| !matches!(*part, "" | ".")) {
        if part == ".." {
            // A symlink before a later ".." could change where the climb starts.
            if seen_name || !resolved.pop() {
                return Err(invalid());
            }
        } else {
            seen_name = true;
            resolved.push(part);
        }
    }
    Ok(resolved)
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && !path.starts_with('/')
        && !path.contains(['\\', '\0'])
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn detects_zip_bundle_but_ignores_metadata_and_nested_helpers() {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in [
            "Example.app/Contents/Info.plist",
            "Example.app/Contents/Helpers/Helper.app/Contents/Info.plist",
            "__MACOSX/Example.app/Contents/Info.plist",
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut zip, b"fixture").unwrap();
        }
        let mut file = zip.finish().unwrap();
        file.set_position(0);
        assert!(inspect_zip(file).unwrap().contains_app().unwrap());
    }

    #[test]
    fn refuses_multiple_bundles_and_entries_below_links() {
        let mut entries = Entries::default();
        entries
            .add("One.app/Contents/Info.plist", 1, Kind::File)
            .unwrap();
        entries
            .add("Two.app/Contents/Info.plist", 1, Kind::File)
            .unwrap();
        assert!(entries.contains_app().is_err());

        let mut entries = Entries::default();
        entries.add("one", 0, Kind::Symlink).unwrap();
        entries.add("./one/file", 1, Kind::File).unwrap();
        assert!(entries.contains_app().is_err());
    }

    #[test]
    fn applies_path_rules_to_tar_entries() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut file = tar::Header::new_ustar();
        file.set_mode(0o755);
        file.set_size(1);
        tar.append_data(&mut file, "tool", b"x".as_slice()).unwrap();
        let mut link = tar::Header::new_ustar();
        link.set_mode(0o777);
        link.set_size(0);
        link.set_entry_type(tar::EntryType::Symlink);
        tar.append_link(&mut link, "bad/link", "../../outside")
            .unwrap();
        assert!(inspect_tar(tar.into_inner().unwrap().as_slice()).is_err());
        assert!(link_path("bundle/Contents/relative", "../other", false).is_ok());
        assert!(entry_path("/absolute").is_err());
        assert!(entry_path("dir/../other").is_err());
    }

    #[test]
    fn rejects_unsupported_installers_and_archives() {
        for name in ["Tool.DMG", "tool.pkg", "tool.tar.zst", "tool.7z"] {
            assert!(Format::detect(name).is_err(), "{name}");
        }
        assert!(matches!(
            Format::detect("tool.TAR.BZ2").unwrap(),
            Some(Format::Bzip2)
        ));
        assert!(Format::detect("tool-linux-x64").unwrap().is_none());
    }
}
