use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{
    git::{Git, index_row, invalid, oid, records, relative},
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
    let directory = scratch.dir.path().join(format!("merge-{label}"));
    if directory.exists() {
        fs::remove_dir_all(&directory)?;
    }
    fs::create_dir(&directory)?;
    let object_format = scratch.git.text(["rev-parse", "--show-object-format"])?;
    scratch.live.get([
        "init",
        "--quiet",
        "--template=",
        &format!("--object-format={object_format}"),
        directory
            .to_str()
            .ok_or_else(|| invalid("invalid scratch path"))?,
    ])?;
    let git = scratch.live.at(&directory).quarantined(scratch.objects());
    git.get(["-c", "core.hooksPath=/dev/null", "reset", "--hard", local])?;
    let whole = whole_conflicts(&git, local, published)?;

    let merged = git.output([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "merge.autoStash=false",
        "-c",
        "rerere.enabled=false",
        "-c",
        "merge.verifySignatures=false",
        "-c",
        "user.name=dots sync",
        "-c",
        "user.email=dots@localhost",
        "merge",
        "-s",
        "ort",
        "-X",
        "theirs",
        "--no-edit",
        "--no-commit",
        "--no-ff",
        "--allow-unrelated-histories",
        published,
    ])?;
    if !matches!(merged.status.code(), Some(0 | 1)) {
        return Err(invalid("could not merge published configuration"));
    }
    if !whole.is_empty() {
        let take_whole = |path: &Path| whole.iter().any(|item| path.starts_with(item));
        let tree = git.tree(published)?;
        let mut entries = Vec::new();
        let listed = git.get(["ls-files", "-z"])?;
        let indexed = records(&listed)
            .map(relative)
            .collect::<io::Result<BTreeSet<_>>>()?;
        for path in indexed.iter().filter(|path| take_whole(path)) {
            index_row(&mut entries, path, None, local.len());
        }
        for (path, entry) in tree.iter().filter(|(path, _)| take_whole(path)) {
            index_row(&mut entries, path, Some(entry), local.len());
        }
        git.update_index(&entries)?;
    }
    let tree = git.id(["write-tree"])?;
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

fn whole_conflicts(git: &Git<'_>, local: &str, published: &str) -> io::Result<BTreeSet<PathBuf>> {
    let output = git.output([
        "merge-tree",
        "--write-tree",
        "--messages",
        "--name-only",
        "-z",
        "--allow-unrelated-histories",
        local,
        published,
    ])?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(invalid("could not classify merge conflicts"));
    }
    let mut tokens = output.stdout.split(|b| *b == 0);
    let mut next = || {
        tokens
            .next()
            .ok_or_else(|| invalid("incomplete merge diagnostics"))
    };
    oid(next()?)?;
    while !next()?.is_empty() {}
    let mut whole = BTreeSet::new();
    loop {
        let count = next()?;
        if count.is_empty() {
            return Ok(whole);
        }
        let count = std::str::from_utf8(count)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or_else(|| invalid("invalid merge diagnostic path count"))?;
        let mut paths = Vec::new();
        for _ in 0..count {
            paths.push(relative(next()?)?);
        }
        let kind = next()?;
        let _message = next()?;
        if kind.starts_with(b"CONFLICT") && kind != b"CONFLICT (contents)" {
            whole.extend(paths);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::tests::{fixture_commit, git_fixture};
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

        let scratch = Scratch::new(&git, &root.join(".git"), &root.join(".git")).unwrap();
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
