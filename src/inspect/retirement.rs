use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

use crate::config::physical;
use crate::{Args, Ctx, Fail};

pub fn retire_dotfiles(ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    let root = ctx.root.path();
    let list = root.join("dots/retired-dotfiles");
    let file = match fs::File::open(&list) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(failure(&list, error)),
    };
    let home = ctx
        .env
        .home()
        .ok_or_else(|| Fail::usage("set HOME before retiring dotfiles"))?;
    let home = home.canonicalize().map_err(|error| failure(home, error))?;
    let mut failed = false;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| failure(&list, error))?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(relative) = relative_entry(&line) else {
            eprintln!(
                "dots: {}",
                Fail::usage(format!(
                    "retire-dotfiles ignored entry outside HOME: {line}"
                ))
            );
            continue;
        };
        let target = home.join(relative);
        match remove_if_retired(root, &home, &target) {
            Ok(true) => println!("Removed retired link {}", line.escape_default()),
            Ok(false) => {}
            Err(error) => {
                eprintln!("dots: {}", failure(&target, error));
                failed = true;
            }
        }
    }
    Ok(u8::from(failed))
}

fn relative_entry(entry: &str) -> Option<&Path> {
    let path = Path::new(entry.strip_prefix("~/")?.trim_start_matches('/'));
    (!path
        .components()
        .any(|component| component == Component::ParentDir))
    .then_some(path)
}

fn failure(path: &Path, error: io::Error) -> Fail {
    Fail::new(1, format!("retire-dotfiles: {}: {error}", path.display()))
}

fn remove_if_retired(root: &Path, home: &Path, target: &Path) -> io::Result<bool> {
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Ok(false);
    };
    let parent = match parent.canonicalize() {
        Ok(parent) if parent.starts_with(home) => parent,
        Ok(_) => return Ok(false),
        Err(error) if missing(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    let target = parent.join(name);
    let link = match fs::read_link(&target) {
        Ok(link) => link,
        Err(error) if missing(&error) || error.kind() == io::ErrorKind::InvalidInput => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if !missing_inside_root(&parent.join(&link), root)?
        || !fs::read_link(&target).is_ok_and(|current| current == link)
    {
        return Ok(false);
    }
    remove_observed_link(&target, &link, root)
}

fn missing_inside_root(destination: &Path, root: &Path) -> io::Result<bool> {
    match destination.metadata() {
        Ok(_) => return Ok(false),
        Err(error) if missing(&error) => {}
        Err(error) => return Err(error),
    }
    let mut prefix = destination;
    loop {
        match physical(prefix) {
            Ok(path) => {
                if prefix != destination && !path.metadata().is_ok_and(|entry| !entry.is_dir()) {
                    // A dangling alias may hide the actual obstruction.
                    return Ok(false);
                }
                return Ok(path != root && path.starts_with(root));
            }
            // A former source directory can now be a file. Resolve the
            // obstructing prefix, including symlinks, before retiring the link.
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                if !matches!(prefix.components().next_back(), Some(Component::Normal(_))) {
                    return Ok(false);
                }
                let Some(parent) = prefix.parent() else {
                    return Ok(false);
                };
                prefix = parent;
            }
            Err(error) if missing(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
}

fn remove_observed_link(target: &Path, observed: &Path, root: &Path) -> io::Result<bool> {
    let parent = target.parent().expect("retirement target has a parent");
    let work = tempfile::Builder::new()
        .prefix(".dots-retire-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)?;
    let mut original = tempfile::TempPath::try_from_path(work.path().join("original"))?;
    original.disable_cleanup(true);
    match fs::rename(target, &original) {
        Ok(()) => {}
        Err(error) if missing(&error) => return Ok(false),
        Err(error) => return Err(error),
    }
    let still_retired = if fs::read_link(&original).is_ok_and(|link| link == observed) {
        missing_inside_root(&parent.join(observed), root)
    } else {
        Ok(false)
    };
    if matches!(still_retired, Ok(true)) {
        fs::remove_file(&original)?;
        return Ok(true);
    }
    // A replacement belongs to its writer. Leave both entries recoverable
    // if another writer has claimed the target since the displacement.
    if let Err(error) = original.persist_noclobber(target) {
        let saved = work.keep().join("original");
        return Err(io::Error::other(format!(
            "could not restore changed target, original kept at {}: {}",
            saved.display(),
            error.error
        )));
    }
    still_retired
}

fn missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn retains_a_regular_file_at_a_previously_observed_link_path() {
        let world = tempfile::tempdir().unwrap();
        let path = world.path().join("target");
        fs::write(&path, b"saved contents").unwrap();
        assert!(!remove_observed_link(&path, Path::new("retired"), world.path()).unwrap());
        assert_eq!(fs::read(path).unwrap(), b"saved contents");
    }

    #[test]
    fn keeps_links_with_sources_or_outside_destinations_and_never_follows_a_target_parent_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("config");
        let home = dir.path().join("home");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&home).unwrap();
        let root = root.canonicalize().unwrap();
        let home = home.canonicalize().unwrap();
        fs::write(root.join("existing"), "source").unwrap();
        symlink(&root, home.join("alias")).unwrap();
        symlink("alias/retired/source", home.join("retired")).unwrap();
        symlink(root.join("existing"), home.join("existing")).unwrap();
        symlink(root.join("existing/former-leaf"), home.join("blocked-leaf")).unwrap();
        let outside_file = dir.path().join("outside-file");
        fs::write(&outside_file, "outside").unwrap();
        symlink(&outside_file, root.join("file-alias")).unwrap();
        symlink(
            root.join("file-alias/former-leaf"),
            home.join("outside-blocked"),
        )
        .unwrap();
        symlink(root.join("missing"), dir.path().join("outside-home")).unwrap();
        symlink(dir.path().join("outside"), root.join("escape")).unwrap();
        symlink(root.join("escape/missing"), home.join("outside-source")).unwrap();
        symlink(dir.path(), home.join("out")).unwrap();
        fs::write(home.join("regular"), "keep").unwrap();

        assert!(remove_if_retired(&root, &home, &home.join("retired")).unwrap());
        assert!(remove_if_retired(&root, &home, &home.join("blocked-leaf")).unwrap());
        assert!(!remove_if_retired(&root, &home, &home.join("outside-blocked")).unwrap());
        assert!(fs::symlink_metadata(home.join("outside-blocked")).is_ok());
        for kept in ["existing", "out/outside-home", "regular", "outside-source"] {
            assert!(!remove_if_retired(&root, &home, &home.join(kept)).unwrap());
            assert!(fs::symlink_metadata(home.join(kept)).is_ok());
        }
        assert!(relative_entry("~/ok/../other").is_none());
        assert!(relative_entry("/not-home").is_none());
    }
}
