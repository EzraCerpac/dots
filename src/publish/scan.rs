use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::Fail;

use super::failure;
use super::repo::{Workspace, object_id, stdout};

pub(super) fn outgoing(workspace: &Workspace<'_, '_>, base: &str, head: &str) -> Result<(), Fail> {
    let staging = tempfile::tempdir().map_err(|_| stage_error())?;
    let rules = staging.path().join("rules");
    fs::create_dir(&rules).map_err(|_| stage_error())?;
    copy_rules(workspace, head, &rules)?;

    let history = workspace.git(
        &["rev-list", "--parents", &format!("{base}..{head}")],
        "list the outgoing commits for screening",
    )?;
    for revision in stdout(&history)?.lines() {
        let mut ids = revision.split_ascii_whitespace();
        let commit = object_id(ids.next().ok_or_else(stage_error)?)?;
        let mut args = vec![
            "diff-tree",
            "--root",
            "--raw",
            "--no-abbrev",
            "--no-commit-id",
            "--no-renames",
            "--diff-filter=AMT",
            "-r",
            "-z",
        ];
        // Against the first parent, a merge includes any blob added by its
        // resolution. The other outgoing commits are screened independently.
        if let Some(parent) = ids.next() {
            args.push(object_id(parent)?);
        }
        args.push(commit);
        let changed = workspace.git(&args, "enumerate the outgoing changed blobs")?;
        if changed.stdout.is_empty() {
            continue;
        }

        let root = staging.path().join(commit);
        fs::create_dir(&root).map_err(|_| stage_error())?;
        let records = changed.stdout.strip_suffix(b"\0").ok_or_else(stage_error)?;
        let mut fields = records.split(|byte| *byte == 0);
        while let Some(metadata) = fields.next() {
            let pathname = fields.next().ok_or_else(stage_error)?;
            let metadata = std::str::from_utf8(metadata).map_err(|_| stage_error())?;
            let fields: Vec<_> = metadata.split_ascii_whitespace().collect();
            let [_, mode, _, id, _] = fields.as_slice() else {
                return Err(stage_error());
            };
            // Gitlinks name commits in another repository. Symbolic links
            // are blobs, so screen their target text without following them.
            if *mode == "160000" {
                continue;
            }
            if !matches!(*mode, "100644" | "100755" | "120000") {
                return Err(stage_error());
            }
            write_blob(workspace, &root, pathname, object_id(id)?)?;
        }
        workspace.gitleaks(&root, &rules)?;
        fs::remove_dir_all(&root).map_err(|_| stage_error())?;
    }
    Ok(())
}

fn copy_rules(workspace: &Workspace<'_, '_>, head: &str, rules: &Path) -> Result<(), Fail> {
    let result = workspace.git(
        &[
            "ls-tree",
            "-z",
            head,
            "--",
            ".gitleaks.toml",
            ".gitleaksignore",
        ],
        "read the pinned secret-scan settings",
    )?;
    for entry in result
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let text = std::str::from_utf8(entry).map_err(|_| stage_error())?;
        let (metadata, name) = text.split_once('\t').ok_or_else(stage_error)?;
        let fields: Vec<_> = metadata.split_ascii_whitespace().collect();
        let [mode, "blob", id] = fields.as_slice() else {
            return Err(stage_error());
        };
        if !matches!(*mode, "100644" | "100755") {
            return Err(failure(
                "the pinned secret-scan settings must be regular files",
            ));
        }
        write_blob(workspace, rules, name.as_bytes(), object_id(id)?)?;
    }
    if !rules.join(".gitleaks.toml").is_file() {
        fs::write(
            rules.join(".gitleaks.toml"),
            "[extend]\nuseDefault = true\n",
        )
        .map_err(|_| stage_error())?;
    }
    Ok(())
}

fn write_blob(
    workspace: &Workspace<'_, '_>,
    root: &Path,
    pathname: &[u8],
    id: &str,
) -> Result<(), Fail> {
    if pathname.split(|byte| *byte == b'/').any(|part| {
        part.is_empty() || part == b"." || part == b".." || part.eq_ignore_ascii_case(b".git")
    }) {
        return Err(stage_error());
    }
    let path = root.join(OsStr::from_bytes(pathname));
    let blob = workspace.git(
        &["cat-file", "blob", id],
        "load an outgoing blob for screening",
    )?;
    fs::create_dir_all(path.parent().ok_or_else(stage_error)?).map_err(|_| stage_error())?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(&blob.stdout))
        .map_err(|_| stage_error())
}

fn stage_error() -> Fail {
    failure("could not privately stage the outgoing Git blobs for gitleaks")
}
