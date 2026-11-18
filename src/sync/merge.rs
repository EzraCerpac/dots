use std::collections::BTreeSet;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::{
    git::{index_row, invalid, oid, relative},
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
    let output = git.output([
        "merge-tree",
        "--write-tree",
        "--name-only",
        "-z",
        "-X",
        "theirs",
        "--allow-unrelated-histories",
        local,
        published,
    ])?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(invalid("could not merge published configuration"));
    }
    let mut rows = output.stdout.split(|byte| *byte == 0);
    let native = oid(rows.next().ok_or_else(|| invalid("missing merged tree"))?)?;
    let tree = if output.status.success() {
        native
    } else {
        let remaining = rows
            .take_while(|row| !row.is_empty())
            .map(relative)
            .collect::<io::Result<BTreeSet<_>>>()?;
        if remaining.is_empty() {
            return Err(invalid("merge has no classified conflicting paths"));
        }
        let roots = remaining
            .iter()
            .map(|path| conflict_root(path, [local, published]))
            .collect::<BTreeSet<_>>();
        let includes = |path: &Path| roots.iter().any(|root| path.starts_with(root));
        git.get(["read-tree", &native])?;
        let mut rows = Vec::new();
        for path in git
            .tree(&native)?
            .keys()
            .filter(|path| remaining.contains(*path) || includes(path))
        {
            index_row(&mut rows, path, None, local.len());
        }
        for (path, entry) in git
            .tree(published)?
            .iter()
            .filter(|(path, _)| includes(path))
        {
            index_row(&mut rows, path, Some(entry), local.len());
        }
        git.update_index(&rows)?;
        git.id(["write-tree"])?
    };
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
    use crate::sync::tests::{fixture_commit, git_fixture};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

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
            ("type", &b"base"[..]),
            ("deleted", &b"base"[..]),
        ] {
            fs::write(root.join(path), bytes).unwrap();
        }
        git.get(["add", "-A"]).unwrap();
        let base = fixture_commit(&git, "base");

        fs::write(root.join("binary"), b"local\0").unwrap();
        fs::set_permissions(root.join("binary"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(root.join("type")).unwrap();
        symlink("other", root.join("type")).unwrap();
        fs::write(root.join("deleted"), b"local").unwrap();
        fs::create_dir(root.join("prefix")).unwrap();
        fs::write(root.join("prefix/leaf"), b"local").unwrap();
        git.get(["add", "-A"]).unwrap();
        let local = fixture_commit(&git, "local");

        git.get(["switch", "-qc", "published", &base]).unwrap();
        fs::write(root.join("binary"), b"published\0").unwrap();
        fs::write(root.join("type"), b"published").unwrap();
        fs::remove_file(root.join("deleted")).unwrap();
        fs::write(root.join("prefix"), b"published").unwrap();
        git.get(["add", "-A"]).unwrap();
        let published = fixture_commit(&git, "published");

        let scratch = Scratch::new(&git, &root.join(".git")).unwrap();
        let merged = with_published(&scratch, &local, Some(&published), "dev").unwrap();
        assert_eq!(
            scratch.git.tree(&merged).unwrap(),
            git.tree(&published).unwrap()
        );
        assert_eq!(
            scratch
                .git
                .text(["rev-list", "--parents", "--max-count=1", &merged])
                .unwrap(),
            format!("{merged} {local} {published}")
        );
    }
}
