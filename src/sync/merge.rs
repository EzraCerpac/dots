use std::collections::BTreeSet;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::{
    git::{Entry, Tree, index_row, invalid, oid, relative, split},
    snapshot::Scratch,
};

/// Merge outside the live index and worktree. Git's favored-side strategy
/// handles conflicting text hunks. For structural conflicts use the published
/// tree's exact entries, including modes and absent files.
pub(super) fn with_published(
    scratch: &Scratch<'_>,
    local: &str,
    published: Option<&str>,
    label: &str,
) -> io::Result<String> {
    let Some(published) = published else {
        return Ok(local.into());
    };
    if scratch.git.ancestor(published, local)? {
        return Ok(local.into());
    }
    if scratch.git.ancestor(local, published)? {
        return Ok(published.into());
    }
    let git = scratch
        .live
        .indexed(&scratch.dir.path().join("merge.index"));
    let args = [
        "merge-tree",
        "--write-tree",
        "-z",
        "--allow-unrelated-histories",
    ];
    // The favored merge hides binary conflicts after choosing their blob,
    // even if the resulting mode still comes from the local side.
    let neutral = git.output(args.into_iter().chain(["--messages", local, published]))?;
    let (mut tree, rows) = merge_output(&neutral)?;
    if !neutral.status.success() {
        let (binaries, renames) = classify_conflicts(rows, [local, published])?;
        let favored = git.output(args.into_iter().chain([
            "--name-only",
            "--no-messages",
            "-X",
            "theirs",
            local,
            published,
        ]))?;
        let (native, rows) = merge_output(&favored)?;
        let remaining = rows
            .take_while(|row| !row.is_empty())
            .map(relative)
            .collect::<io::Result<BTreeSet<_>>>()?;
        if !favored.status.success() && remaining.is_empty() {
            return Err(invalid("merge has no classified conflicting paths"));
        }
        let mut roots = remaining
            .iter()
            .map(|path| conflict_root(path, [local, published]))
            .collect::<BTreeSet<_>>();
        let published_tree = git.tree(published)?;
        for rename in renames {
            if roots.contains(&rename.suggested)
                && published_tree.get(&rename.original) == Some(&rename.entry)
            {
                roots.insert(rename.original);
            }
        }
        let includes = |path: &Path| roots.iter().any(|root| path.starts_with(root));
        let mut index = Vec::new();
        if !remaining.is_empty() || !binaries.is_empty() {
            let merged = git.tree(&native)?;
            for path in merged
                .keys()
                .filter(|path| remaining.contains(*path) || includes(path))
            {
                index_row(&mut index, path, None, local.len());
            }
            if !remaining.is_empty() {
                for (path, entry) in published_tree.iter().filter(|(path, _)| includes(path)) {
                    index_row(&mut index, path, Some(entry), local.len());
                }
            }
            for (path, entry) in binaries
                .iter()
                .filter(|(path, _)| !remaining.contains(*path) && !includes(path))
            {
                let chosen = merged
                    .get(path)
                    .filter(|chosen| chosen.oid == entry.oid)
                    .ok_or_else(|| invalid("could not verify the published binary merge entry"))?;
                if chosen != entry {
                    index_row(&mut index, path, Some(entry), local.len());
                }
            }
        }
        tree = if index.is_empty() {
            native
        } else {
            git.get(["read-tree", &native])?;
            git.update_index(&index)?;
            git.id(["write-tree"])?
        };
    }
    scratch.git.id([
        "-c",
        "commit.gpgSign=false",
        "commit-tree",
        &tree,
        "-p",
        local,
        "-p",
        published,
        "-m",
        &format!("Merge published {label}"),
    ])
}

fn merge_output(
    output: &std::process::Output,
) -> io::Result<(String, impl Iterator<Item = &[u8]>)> {
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(invalid("could not merge published configuration"));
    }
    let mut rows = output.stdout.split(|byte| *byte == 0);
    let tree = oid(rows.next().ok_or_else(|| invalid("missing merged tree"))?)?;
    Ok((tree, rows))
}

struct SuggestedRename {
    suggested: PathBuf,
    original: PathBuf,
    entry: Entry,
}

fn classify_conflicts<'a>(
    mut rows: impl Iterator<Item = &'a [u8]>,
    parents: [&str; 2],
) -> io::Result<(Tree, Vec<SuggestedRename>)> {
    let mut theirs = Tree::new();
    for row in rows.by_ref().take_while(|row| !row.is_empty()) {
        let (header, name) = split(row, b'\t')?;
        if let Some(header) = header.strip_suffix(b" 3") {
            let (mode, object) = split(header, b' ')?;
            theirs.insert(
                relative(name)?,
                Entry {
                    mode: std::str::from_utf8(mode).map_err(io::Error::other)?.into(),
                    oid: oid(object)?,
                },
            );
        }
    }
    let mut binaries = Tree::new();
    let mut renames = Vec::new();
    while let Some(count) = rows.next().filter(|count| !count.is_empty()) {
        let count: usize = std::str::from_utf8(count)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| invalid("invalid merge diagnostic path count"))?;
        let paths = rows.by_ref().take(count).collect::<Vec<_>>();
        let kind = rows
            .next()
            .ok_or_else(|| invalid("missing merge diagnostic type"))?;
        rows.next()
            .ok_or_else(|| invalid("missing merge diagnostic message"))?;
        if kind == b"CONFLICT (directory rename suggested)"
            && let [suggested, original] = paths.as_slice()
        {
            let suggested = relative(suggested)?;
            // Git can report the published stage under the suggested path,
            // including its synthetic suffix when a directory is in the way.
            let mut entries = theirs
                .iter()
                .filter(|(path, _)| conflict_root(path, parents) == suggested);
            if let Some((_, entry)) = entries.next()
                && entries.next().is_none()
            {
                renames.push(SuggestedRename {
                    suggested,
                    original: relative(original)?,
                    entry: entry.clone(),
                });
            }
        } else if kind == b"CONFLICT (binary)" {
            for path in paths {
                let path = relative(path)?;
                let entry = theirs
                    .get(&path)
                    .filter(|entry| matches!(entry.mode.as_str(), "100644" | "100755"))
                    .ok_or_else(|| invalid("missing published binary conflict entry"))?;
                binaries.insert(path, entry.clone());
            }
        }
    }
    Ok((binaries, renames))
}

fn conflict_root(path: &Path, parents: [&str; 2]) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    let Some(tilde) = bytes.iter().rposition(|byte| *byte == b'~') else {
        return path.to_owned();
    };
    if parents.iter().any(|parent| {
        bytes[tilde + 1..]
            .strip_prefix(parent.as_bytes())
            .is_some_and(|suffix| {
                suffix.is_empty()
                    || suffix.strip_prefix(b"_").is_some_and(|number| {
                        !number.is_empty() && number.iter().all(u8::is_ascii_digit)
                    })
            })
    }) {
        PathBuf::from(std::ffi::OsString::from_vec(bytes[..tilde].to_vec()))
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Git;
    use crate::sync::tests::{fixture_commit, git_fixture};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn cr12_published_rename_preserves_the_published_file_at_its_path() {
        let (_world, ctx) = git_fixture();
        let git = Git::new(&ctx, ctx.root.path());
        let root = git.root();
        fs::create_dir(root.join("old")).unwrap();
        fs::write(
            root.join("old/original"),
            b"file renamed to the published path\n",
        )
        .unwrap();
        fs::write(
            root.join("old/neighbor"),
            b"separate file under the moved directory\n",
        )
        .unwrap();
        git.get(["add", "-A"]).unwrap();
        let base = fixture_commit(&git, "baseline");
        git.get(["mv", "old", "new"]).unwrap();
        fs::create_dir(root.join("new/renamed")).unwrap();
        fs::write(root.join("new/renamed/leaf"), b"local descendant\n").unwrap();
        git.get(["add", "-A"]).unwrap();
        let local = fixture_commit(&git, "move directory locally");
        git.get(["switch", "-qc", "published", &base]).unwrap();
        git.get(["mv", "old/original", "old/renamed"]).unwrap();
        let published = fixture_commit(&git, "rename the file upstream");
        let scratch = Scratch::new(&git, &root.join(".git")).unwrap();
        let merged = with_published(&scratch, &local, Some(&published), "dev").unwrap();
        let tree = git.tree(&merged).unwrap();
        let published = git.tree(&published).unwrap();
        assert_eq!(
            tree.get(Path::new("old/renamed")),
            published.get(Path::new("old/renamed")),
            "{tree:?}"
        );
        assert!(tree.contains_key(Path::new("new/neighbor")), "{tree:?}");
    }

    #[test]
    fn published_tree_wins_binary_type_delete_and_directory_conflicts() {
        let (_fixture, mut ctx) = git_fixture();
        ctx.env
            .vars
            .insert("GIT_AUTHOR_NAME".into(), "Fixture".into());
        ctx.env
            .vars
            .insert("GIT_AUTHOR_EMAIL".into(), "fixture@example.invalid".into());
        ctx.env
            .vars
            .insert("GIT_COMMITTER_NAME".into(), "Fixture".into());
        ctx.env.vars.insert(
            "GIT_COMMITTER_EMAIL".into(),
            "fixture@example.invalid".into(),
        );
        let git = Git::new(&ctx, ctx.root.path());
        let root = git.root();
        for (path, bytes) in [
            ("binary", &b"base\0"[..]),
            ("mode-only", &b"unchanged local contents\0"[..]),
            ("type", &b"base"[..]),
            ("deleted", &b"base"[..]),
        ] {
            fs::write(root.join(path), bytes).unwrap();
        }
        git.get(["add", "-A"]).unwrap();
        let base = fixture_commit(&git, "base");

        fs::write(root.join("binary"), b"local\0").unwrap();
        fs::set_permissions(root.join("binary"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(root.join("mode-only"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(root.join("type")).unwrap();
        symlink("other", root.join("type")).unwrap();
        fs::write(root.join("deleted"), b"local").unwrap();
        fs::create_dir(root.join("prefix")).unwrap();
        fs::write(root.join("prefix/leaf"), b"local").unwrap();
        git.get(["add", "-A"]).unwrap();
        let local = fixture_commit(&git, "local");

        git.get(["switch", "-qc", "published", &base]).unwrap();
        fs::write(root.join("binary"), b"published\0").unwrap();
        fs::write(root.join("mode-only"), b"clean published bytes\0").unwrap();
        fs::write(root.join("type"), b"published").unwrap();
        fs::remove_file(root.join("deleted")).unwrap();
        fs::write(root.join("prefix"), b"published").unwrap();
        git.get(["add", "-A"]).unwrap();
        let published = fixture_commit(&git, "published");

        let scratch = Scratch::new(&git, &root.join(".git")).unwrap();
        let merged = with_published(&scratch, &local, Some(&published), "dev").unwrap();
        let mut expected = git.tree(&published).unwrap();
        // The local executable bit is an independent edit for this path.
        expected.get_mut(Path::new("mode-only")).unwrap().mode = "100755".into();
        assert_eq!(scratch.git.tree(&merged).unwrap(), expected);
        assert_eq!(
            scratch
                .git
                .text(["rev-list", "--parents", "--max-count=1", &merged])
                .unwrap(),
            format!("{merged} {local} {published}")
        );
    }
}
