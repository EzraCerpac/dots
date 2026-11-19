use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Canonicalize existing components, including symlinks before any `..`.
/// A nonexistent tail is allowed so init and edits can name new paths.
pub(crate) fn physical(path: &Path) -> io::Result<PathBuf> {
    resolve(path, 0)
}

fn resolve(path: &Path, links: u8) -> io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => return Ok(path),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(error) if links == 40 => return Err(error),
        Err(_) => {}
    }
    let (Some(parent), Some(name)) = (path.parent(), path.components().next_back()) else {
        return Err(io::Error::new(io::ErrorKind::NotFound, "missing path"));
    };
    let mut resolved = resolve(parent, links)?;
    match fs::metadata(&resolved) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "not a directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if name.as_os_str() == ".." {
        resolved.pop();
        return Ok(resolved);
    }
    let candidate = resolved.join(name);
    match fs::read_link(&candidate) {
        Ok(target) => resolve(&resolved.join(target), links + 1),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(candidate)
        }
        Err(error) => Err(error),
    }
}
