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
use super::repo::{Workspace, git_commit, jj_commit, object_id, stdout};

pub(super) fn outgoing(
    workspace: &Workspace<'_, '_>,
    base: &str,
    head: &str,
    conflicts: bool,
) -> Result<(), Fail> {
    let staging = tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .map_err(|_| stage_error())?;
    let scratch = fs::canonicalize(staging.path()).map_err(|_| stage_error())?;
    let rules = scratch.join("rules");
    private_dirs(&rules)?;
    copy_rules(workspace, head, &rules, conflicts)?;
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
    for line in stdout(&history)?.lines() {
        let mut ids = line.split_ascii_whitespace();
        let commit = object_id(ids.next().ok_or_else(stage_error)?)?;
        let parents = ids.map(object_id).collect::<Result<Vec<_>, _>>()?;
        if conflicts && commit == head {
            for (index, tree) in conflict_sides(workspace, commit)?.iter().enumerate() {
                let root = inputs.join(format!("{commit}-{index}"));
                screened |= screen_tree(workspace, &root, &rules, tree, &parents)?;
            }
        } else {
            let root = inputs.join(commit);
            screened |= screen_tree(workspace, &root, &rules, &git_commit(commit), &parents)?;
        }
    }
    if !screened {
        let empty = inputs.join("empty");
        private_dirs(&empty)?;
        workspace.gitleaks(&empty, OsStr::new("."), &rules)?;
    }
    Ok(())
}

fn screen_tree(
    workspace: &Workspace<'_, '_>,
    root: &Path,
    rules: &Path,
    tree: &str,
    parents: &[&str],
) -> Result<bool, Fail> {
    let changed = diff_blobs(workspace, tree, parents.first().copied())?;
    let mut blobs = blob_changes(&changed)?;
    for parent in parents.iter().skip(1) {
        if blobs.is_empty() {
            break;
        }
        // An identical path/blob reaches this tree through that parent.
        // Following those parents leads to published main or to the outgoing
        // commit that introduced the pair, which we screen separately.
        let changed = diff_blobs(workspace, tree, Some(parent))?;
        let paths: HashSet<_> = blob_changes(&changed)?
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        blobs.retain(|(path, _)| paths.contains(path));
    }
    if blobs.is_empty() {
        return Ok(false);
    }
    private_dirs(root)?;
    let mut historical_ignores = Vec::new();
    for (path, id) in blobs {
        if path.eq_ignore_ascii_case(b".gitleaksignore") {
            historical_ignores.push((path, id));
        } else {
            write_blob(workspace, root, path, id)?;
        }
    }
    workspace.gitleaks(root, OsStr::new("."), rules)?;
    // Gitleaks also loads <source>/.gitleaksignore implicitly. Keep the
    // historical file outside the directory scan, then scan it as data.
    for (name, id) in historical_ignores {
        write_blob(workspace, root, name, id)?;
        let path = OsStr::from_bytes(name);
        workspace.gitleaks(root, path, rules)?;
        fs::remove_file(root.join(path)).map_err(|_| stage_error())?;
    }
    fs::remove_dir_all(root).map_err(|_| stage_error())?;
    Ok(true)
}

fn conflict_sides(workspace: &Workspace<'_, '_>, commit: &str) -> Result<Vec<String>, Fail> {
    let tree = workspace.git(
        &["ls-tree", "-z", "--full-tree", &git_commit(commit)],
        "locate the sides of the prepared JJ conflict",
    )?;
    // JJ saves full logical trees for each side. Diff the side tree roots
    // directly so a tracked `.jjconflict-*` path in a side stays ordinary
    // data and the generated wrappers and obsolete base terms stay out.
    let mut sides = Vec::new();
    let entries = tree.stdout.strip_suffix(b"\0").ok_or_else(stage_error)?;
    for entry in entries.split(|&byte| byte == 0) {
        let separator = entry
            .iter()
            .position(|&byte| byte == b'\t')
            .ok_or_else(stage_error)?;
        if let Some(index) = entry[separator + 1..].strip_prefix(b".jjconflict-side-")
            && !index.is_empty()
            && index.iter().all(u8::is_ascii_digit)
        {
            let meta = std::str::from_utf8(&entry[..separator]).map_err(|_| stage_error())?;
            let id = meta.strip_prefix("040000 tree ").ok_or_else(stage_error)?;
            sides.push(format!("{}^{{tree}}", object_id(id)?));
        }
    }
    if sides.len() < 2 {
        return Err(stage_error());
    }
    Ok(sides)
}

fn diff_blobs(
    workspace: &Workspace<'_, '_>,
    tree: &str,
    parent: Option<&str>,
) -> Result<Vec<u8>, Fail> {
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
    let parent = parent.map(git_commit);
    if let Some(parent) = &parent {
        args.push(parent);
    }
    args.push(tree);
    Ok(workspace
        .git(&args, "enumerate the outgoing changed blobs")?
        .stdout)
}

fn blob_changes(diff: &[u8]) -> Result<Vec<(&[u8], &str)>, Fail> {
    if diff.is_empty() {
        return Ok(Vec::new());
    }
    let records = diff.strip_suffix(b"\0").ok_or_else(stage_error)?;
    let mut fields = records.split(|byte| *byte == 0);
    let mut blobs = Vec::new();
    while let Some(metadata) = fields.next() {
        let path = fields.next().ok_or_else(stage_error)?;
        let metadata = std::str::from_utf8(metadata).map_err(|_| stage_error())?;
        let fields: Vec<_> = metadata.split_ascii_whitespace().collect();
        let [_, mode, old, new, _] = fields.as_slice() else {
            return Err(stage_error());
        };
        // A mode-only change has identical path and bytes. A Gitlink names
        // a commit elsewhere, while a symlink's target text is a local blob.
        if old == new || *mode == "160000" {
            continue;
        }
        if !matches!(*mode, "100644" | "100755" | "120000") {
            return Err(stage_error());
        }
        blobs.push((path, object_id(new)?));
    }
    Ok(blobs)
}

fn copy_rules(
    workspace: &Workspace<'_, '_>,
    head: &str,
    rules: &Path,
    conflicts: bool,
) -> Result<(), Fail> {
    if let Some(ignore) = pinned_policy_blob(workspace, head, ".gitleaksignore", conflicts)? {
        private_file(&rules.join(".gitleaksignore"), &ignore)?;
    }
    let mut path = ".gitleaks.toml".to_owned();
    let mut copy = rules.join(&path);
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(path.clone()) {
            return Err(policy_error());
        }
        let data = match pinned_policy_blob(workspace, head, &path, conflicts)? {
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
    conflicts: bool,
) -> Result<Option<Vec<u8>>, Fail> {
    if conflicts {
        return logical_policy_blob(workspace, head, path);
    }
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

fn logical_policy_blob(
    workspace: &Workspace<'_, '_>,
    head: &str,
    path: &str,
) -> Result<Option<Vec<u8>>, Fail> {
    // The Git conflict wrapper can omit real files. Query JJ's pinned tree
    // for the path, any children if it is a directory, and non-directory
    // ancestors such as symlinks or conflicts.
    let mut fileset = format!("root:{path:?}");
    for ancestor in Path::new(path).ancestors().skip(1) {
        if !ancestor.as_os_str().is_empty() {
            fileset.push_str(&format!(" | root-file:{ancestor:?}"));
        }
    }
    let head = jj_commit(head);
    let entries = workspace.jj(
        &[
            "file",
            "list",
            "-r",
            &head,
            "-T",
            r#"json(path) ++ "\t" ++ file_type ++ "\n""#,
            "--",
            &fileset,
        ],
        "read the logical pinned secret-scan settings",
    )?;
    let mut found = false;
    for record in stdout(&entries)?.lines() {
        let (name, kind) = record.split_once('\t').ok_or_else(policy_error)?;
        if kind == "conflict" {
            return Err(failure(
                "the gitleaks policy has merge conflicts; resolve the policy locally before publishing",
            ));
        }
        let name: String = serde_json::from_str(name).map_err(|_| policy_error())?;
        if found || name != path || kind != "file" {
            return Err(policy_error());
        }
        found = true;
    }
    if !found {
        return Ok(None);
    }
    let blob = workspace.jj(
        &[
            r#"--config=templates.file_show='""'"#,
            "file",
            "show",
            "-r",
            &head,
            "--",
            &format!("root-file:{path:?}"),
        ],
        "read the logical pinned policy bytes",
    )?;
    Ok(Some(blob.stdout))
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
