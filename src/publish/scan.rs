use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use toml_edit::{DocumentMut, value};

use crate::Fail;

use super::failure;
use super::repo::{Workspace, git_commit, object_id, stdout};

pub(super) fn outgoing(workspace: &Workspace<'_, '_>, base: &str, head: &str) -> Result<(), Fail> {
    let staging = tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .map_err(|_| stage_error())?;
    let scratch = fs::canonicalize(staging.path()).map_err(|_| stage_error())?;
    let rules = scratch.join("rules");
    private_dirs(&rules)?;
    copy_rules(workspace, head, &rules)?;
    // Each scanner cwd sits inside the policy sandbox. Its relative paths
    // are for data; approved policy files sit outside the scanned subtree.
    let inputs = rules.join("inputs");

    let history = workspace.git(
        &[
            "rev-list",
            "--parents",
            &format!("{}..{}", git_commit(base), git_commit(head)),
        ],
        "list the outgoing commits for screening",
    )?;
    let mut screened = false;
    for revision in stdout(&history)?.lines() {
        let mut ids = revision.split_ascii_whitespace();
        let commit = object_id(ids.next().ok_or_else(stage_error)?)?;
        let revision = git_commit(commit);
        let parent = ids.next().map(object_id).transpose()?.map(git_commit);
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
        if let Some(parent) = &parent {
            args.push(parent);
        }
        args.push(&revision);
        let changed = workspace.git(&args, "enumerate the outgoing changed blobs")?;
        if changed.stdout.is_empty() {
            continue;
        }

        let root = inputs.join(commit);
        private_dirs(&root)?;
        let records = changed.stdout.strip_suffix(b"\0").ok_or_else(stage_error)?;
        let mut fields = records.split(|byte| *byte == 0);
        let mut historical_ignores = Vec::new();
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
            let id = object_id(id)?;
            if pathname.eq_ignore_ascii_case(b".gitleaksignore") {
                historical_ignores.push((pathname, id));
            } else {
                write_blob(workspace, &root, pathname, id)?;
            }
        }
        workspace.gitleaks(&root, OsStr::new("."), &rules)?;
        // Gitleaks also loads <source>/.gitleaksignore implicitly. Keep the
        // historical file outside the directory scan, then scan it as data.
        for (name, id) in historical_ignores {
            write_blob(workspace, &root, name, id)?;
            let path = OsStr::from_bytes(name);
            workspace.gitleaks(&root, path, &rules)?;
            fs::remove_file(root.join(path)).map_err(|_| stage_error())?;
        }
        screened = true;
        fs::remove_dir_all(&root).map_err(|_| stage_error())?;
    }
    if !screened {
        let empty = inputs.join("empty");
        private_dirs(&empty)?;
        workspace.gitleaks(&empty, OsStr::new("."), &rules)?;
    }
    Ok(())
}

fn copy_rules(workspace: &Workspace<'_, '_>, head: &str, rules: &Path) -> Result<(), Fail> {
    if let Some(ignore) = pinned_policy_blob(workspace, head, ".gitleaksignore")? {
        private_file(&rules.join(".gitleaksignore"), &ignore)?;
    }
    let mut path = ".gitleaks.toml".to_owned();
    let mut copy = rules.join(&path);
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(path.clone()) {
            return Err(policy_error());
        }
        let data = match pinned_policy_blob(workspace, head, &path)? {
            Some(data) => data,
            None if seen.len() == 1 => b"[extend]\nuseDefault = true\n".to_vec(),
            None => return Err(policy_error()),
        };
        let mut doc = std::str::from_utf8(&data)
            .ok()
            .and_then(|text| text.parse::<DocumentMut>().ok())
            .ok_or_else(policy_error)?;
        // Viper treats dotted keys and mixed case as aliases. Reject aliases
        // that could leave a second path outside the parsed extension table.
        if doc.iter().any(|(key, _)| {
            key.contains('.') || (key.eq_ignore_ascii_case("extend") && key != "extend")
        }) {
            return Err(policy_error());
        }
        let mut next = None;
        if let Some(extend) = doc.get_mut("extend") {
            let extend = extend.as_table_like_mut().ok_or_else(policy_error)?;
            if extend
                .iter()
                .any(|(key, _)| !matches!(key, "path" | "useDefault" | "disabledRules"))
            {
                return Err(policy_error());
            }
            if let Some(item) = extend.get_mut("path") {
                let extension = item.as_str().ok_or_else(policy_error)?;
                if !extension.is_empty() {
                    let extension = repository_path(extension).ok_or_else(policy_error)?;
                    let next_copy = rules.join(format!("extend-{}.toml", seen.len()));
                    *item = value(next_copy.to_str().ok_or_else(policy_error)?);
                    next = Some((extension, next_copy));
                }
            }
        }
        private_file(&copy, doc.to_string().as_bytes())?;
        let Some((extension, next_copy)) = next else {
            return Ok(());
        };
        path = extension;
        copy = next_copy;
    }
}

fn repository_path(path: &str) -> Option<String> {
    let mut normal = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => (),
            Component::Normal(name) if !name.as_bytes().eq_ignore_ascii_case(b".git") => {
                normal.push(name)
            }
            _ => return None,
        }
    }
    normal
        .to_str()
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

fn pinned_policy_blob(
    workspace: &Workspace<'_, '_>,
    head: &str,
    path: &str,
) -> Result<Option<Vec<u8>>, Fail> {
    let result = workspace.git(
        &[
            "--literal-pathspecs",
            "ls-tree",
            "-z",
            "--full-tree",
            &git_commit(head),
            "--",
            path,
        ],
        "read the pinned secret-scan settings",
    )?;
    if result.stdout.is_empty() {
        return Ok(None);
    }
    let entry = stdout(&result)?
        .strip_suffix('\0')
        .ok_or_else(policy_error)?;
    let (metadata, name) = entry.split_once('\t').ok_or_else(policy_error)?;
    let mut fields = metadata.split_ascii_whitespace();
    if name != path
        || !matches!(fields.next(), Some("100644" | "100755"))
        || fields.next() != Some("blob")
    {
        return Err(policy_error());
    }
    let id = object_id(fields.next().ok_or_else(policy_error)?)?;
    read_blob(workspace, id).map(Some)
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
    let blob = read_blob(workspace, id)?;
    private_dirs(path.parent().ok_or_else(stage_error)?)?;
    private_file(&path, &blob)
}

fn read_blob(workspace: &Workspace<'_, '_>, id: &str) -> Result<Vec<u8>, Fail> {
    let blob = workspace.git(
        &["cat-file", "blob", &format!("{id}^{{blob}}")],
        "load an outgoing blob for screening",
    )?;
    Ok(blob.stdout)
}

fn private_dirs(path: &Path) -> Result<(), Fail> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| stage_error())
}

fn private_file(path: &Path, body: &[u8]) -> Result<(), Fail> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(body))
        .map_err(|_| stage_error())
}

fn stage_error() -> Fail {
    failure("could not privately stage the outgoing Git blobs for gitleaks")
}

fn policy_error() -> Fail {
    failure(
        "gitleaks rules and extensions must resolve to regular TOML files in the approved commit",
    )
}
