use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{receipt::Receipt, run::Run};

use super::git::{Entry, Git, Tree, invalid, relative};
use super::guard::{CStrName, Dir, Image, private_dir, replace_exact, write_private};
use super::validation::{self, Retirement};
use super::{issue, path_issue};

#[derive(Deserialize)]
struct Status {
    files: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    target: PathBuf,
    source: Option<PathBuf>,
    mode: String,
    origin: Option<Origin>,
    permissions: Option<String>,
    #[serde(default)]
    omitted: usize,
}

#[derive(Deserialize)]
struct Origin {
    config: PathBuf,
    source: Option<PathBuf>,
}

impl Target {
    fn effective_source(&self) -> Option<&Path> {
        self.origin
            .as_ref()
            .and_then(|origin| origin.source.as_deref())
            .or(self.source.as_deref())
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct KnownCopy {
    source: PathBuf,
    entry: Entry,
    mode: Option<u32>,
}

#[derive(Default)]
pub(super) struct Copies {
    known: BTreeMap<PathBuf, KnownCopy>,
    observed: BTreeMap<PathBuf, Image>,
    captured: BTreeMap<PathBuf, Image>,
}

const STATE: &str = "dots-sync-copies.json";
const COPY_ACTION: &str = "Review the source and deployed copy, then rerun dots sync.";

impl Copies {
    pub fn load(dir: &Path) -> io::Result<Self> {
        match File::open(dir.join(STATE)) {
            Ok(file) => {
                let known = serde_json::from_reader(file).map_err(io::Error::other)?;
                Ok(Self {
                    known,
                    observed: BTreeMap::new(),
                    captured: BTreeMap::new(),
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn persist(&self, dir: &Path) -> io::Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut tmp, &self.known).map_err(io::Error::other)?;
        tmp.as_file().sync_all()?;
        tmp.persist(dir.join(STATE))?;
        File::open(dir)?.sync_all()
    }

    pub fn seed(
        &mut self,
        run: &Run<'_>,
        scratch: &super::snapshot::Scratch<'_>,
        tree: &Tree,
    ) -> io::Result<()> {
        // Resolve the previous declarations even when today's local edit
        // already replaced a copy with a template or an absent entry.
        let (_view, root, data) = validation::historical_status(run, scratch, tree)?;
        let Status { files } = serde_json::from_slice(&data).map_err(io::Error::other)?;
        let configs = Configs::read_at(run, &root, true)?;
        for target in files
            .into_iter()
            .filter(|target| target.mode == "copy" && target.omitted == 0)
        {
            let dest = absolute_target(run, &target.target)?;
            let rules = configs.rules(run, &target, &dest);
            let Some(source) = target
                .effective_source()
                .and_then(|source| source.strip_prefix(&root).ok())
            else {
                continue;
            };
            for (leaf, entry) in tree.iter().filter(|(leaf, _)| leaf.starts_with(source)) {
                let relative = leaf.strip_prefix(source).expect("prefix was selected");
                if !rules.includes(relative) {
                    continue;
                }
                let path = dest.join(rules.destination(relative));
                self.known.entry(path).or_insert_with(|| KnownCopy {
                    source: leaf.clone(),
                    entry: entry.clone(),
                    mode: None,
                });
            }
        }
        Ok(())
    }

    pub fn capture(
        &mut self,
        run: &Run<'_>,
        git: &Git<'_>,
        git_dir: &Path,
        base: &Tree,
        receipt: &mut Receipt,
    ) -> io::Result<()> {
        let root = Dir::open(git.root())?;
        let filesystem = Dir::open(Path::new("/"))?;
        let configs = Configs::read_at(run, git.root(), false)?;
        let discovered = status(run, git.root())?;
        for target in &discovered {
            if matches!(target.mode.as_str(), "track" | "track-local") {
                continue;
            }
            let path = absolute_target(run, &target.target)?;
            if let Ok(image) = filesystem.read_path(fs_relative(&path)?) {
                self.observed.insert(path, image);
            }
        }
        for target in discovered {
            if target.mode != "copy" {
                continue;
            }
            let destination = absolute_target(run, &target.target)?;
            let rules = configs.rules(run, &target, &destination);
            let Some(source) = target
                .effective_source()
                .and_then(|source| source.strip_prefix(git.root()).ok())
            else {
                continue;
            };
            let revisions = base.iter().filter(|(path, _)| path.starts_with(source));
            for (leaf, entry) in revisions {
                let relative = leaf.strip_prefix(source).expect("prefix was selected");
                if !rules.includes(relative) {
                    continue;
                }
                let path = destination.join(rules.destination(relative));
                let record = self
                    .known
                    .entry(path.clone())
                    .or_insert_with(|| KnownCopy {
                        source: leaf.clone(),
                        entry: entry.clone(),
                        mode: None,
                    })
                    .clone();
                // A different declaration must prove a clean takeover first.
                if record.source != *leaf || !matches!(entry.mode.as_str(), "100644" | "100755") {
                    continue;
                }
                if run.cancelled() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let common = Image::from_git(git, Some(&record.entry))?;
                let deployed = match filesystem.read_path(fs_relative(&path)?) {
                    Ok(image @ Image::Regular { .. }) => image,
                    Ok(Image::Absent) => continue,
                    _ => {
                        path_issue(
                            receipt,
                            "Kept copy whose type could not be verified",
                            &path,
                            COPY_ACTION,
                        );
                        continue;
                    }
                };
                self.observed.insert(path.clone(), deployed.clone());
                let live = root.read_path(leaf)?;
                if same_copy(&deployed, &common, record.mode) || live == deployed {
                    continue;
                }
                if same_copy(&deployed, &common, None) {
                    path_issue(
                        receipt,
                        "Kept a locally changed copy mode",
                        &path,
                        COPY_ACTION,
                    );
                    continue;
                }
                if !live.git_eq(&common) {
                    path_issue(
                        receipt,
                        "Both the copy and source changed",
                        &path,
                        COPY_ACTION,
                    );
                    continue;
                }
                // Persist the common source before displacement, so a crash
                // cannot make the changed deployed bytes into a new baseline.
                self.persist(git_dir)?;
                let Image::Regular { bytes, .. } = &deployed else {
                    unreachable!()
                };
                let Image::Regular { mode, .. } = &live else {
                    continue;
                };
                let replacement = Image::Regular {
                    bytes: bytes.clone(),
                    mode: *mode,
                };
                if install(
                    git.root(),
                    leaf,
                    git_dir,
                    git_dir,
                    &live,
                    &replacement,
                    receipt,
                )
                .is_err()
                {
                    path_issue(
                        receipt,
                        "Could not capture deployed copy",
                        &path,
                        COPY_ACTION,
                    );
                    if git_dir.join(PENDING).exists() {
                        return Err(invalid("copy capture recovery is pending"));
                    }
                } else {
                    self.captured.insert(path, deployed);
                }
            }
        }
        self.persist(git_dir)
    }

    pub fn confirm_capture(
        &mut self,
        git: &Git<'_>,
        git_dir: &Path,
        tree: &Tree,
    ) -> io::Result<()> {
        if self.captured.is_empty() {
            return Ok(());
        }
        for (path, observed) in std::mem::take(&mut self.captured) {
            let Some(known) = self.known.get_mut(&path) else {
                continue;
            };
            let Some(entry) = tree.get(&known.source) else {
                continue;
            };
            let shared = Image::from_git(git, Some(entry))?;
            if let Image::Regular { mode, .. } = &observed {
                if same_copy(&observed, &shared, None) {
                    known.entry = entry.clone();
                    known.mode = Some(*mode);
                }
            }
        }
        self.persist(git_dir)
    }

    pub fn apply(
        &mut self,
        run: &Run<'_>,
        git: &Git<'_>,
        git_dir: &Path,
        tree: &Tree,
        receipt: &mut Receipt,
    ) -> io::Result<()> {
        let filesystem = Dir::open(Path::new("/"))?;
        let targets = status(run, git.root())?;
        if targets.is_empty() {
            return Ok(());
        }
        let configs = Configs::read_at(run, git.root(), false)?;
        let mut jobs = BTreeMap::new();
        for target in targets {
            if matches!(target.mode.as_str(), "track" | "track-local") {
                continue;
            }
            let path = absolute_target(run, &target.target)?;
            let definition = configs.definition(&target, &path);
            let group = configs.group(run, &target, &path);
            let depends = [definition.as_ref(), group.map(|(_, item)| item)]
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("depends"))
                .flat_map(strings)
                .collect::<Vec<_>>();
            jobs.insert(
                path,
                (
                    target,
                    definition,
                    depends,
                    group.map(|(name, _)| name.to_owned()),
                ),
            );
        }

        let mut observed = std::mem::take(&mut self.observed)
            .into_iter()
            .map(|(path, image)| (path, Ok(image)))
            .collect::<BTreeMap<_, _>>();
        for path in jobs.keys().chain(self.known.keys()) {
            if !observed.contains_key(path) {
                observed.insert(path.clone(), filesystem.read_path(fs_relative(path)?));
            }
        }
        let all_jobs = jobs.keys().cloned().collect::<BTreeSet<_>>();
        let mut group_members = BTreeMap::<String, Vec<PathBuf>>::new();
        for (path, (_, _, _, group)) in &jobs {
            if let Some(name) = group {
                group_members
                    .entry(name.clone())
                    .or_default()
                    .push(path.clone());
            }
        }
        let dependencies = |names: &[String]| {
            names
                .iter()
                .flat_map(|name| {
                    group_members.get(name).cloned().unwrap_or_else(|| {
                        absolute_target(run, Path::new(name)).into_iter().collect()
                    })
                })
                .collect::<Vec<_>>()
        };
        for hook in &configs.pre {
            if invoke(run, git.root(), hook).is_err() {
                issue(
                    receipt,
                    "mise pre-dotfiles hook failed",
                    "Check the pre-dotfiles hook and rerun dots sync.",
                );
                return Ok(());
            }
        }

        let mut results = BTreeMap::new();
        let mut changed = BTreeSet::new();
        while !jobs.is_empty() {
            if run.cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let next = jobs
                .iter()
                .find(|(_, (_, _, deps, _))| {
                    dependencies(deps).iter().all(|dep| !jobs.contains_key(dep))
                })
                .map(|(path, _)| path.clone());
            let Some(path) = next else {
                issue(
                    receipt,
                    "Dotfile dependencies contain a cycle",
                    "Repair the dependency declarations and rerun dots sync.",
                );
                break;
            };
            let (target, definition, depends, _) = jobs.remove(&path).expect("job was present");
            if dependencies(&depends)
                .iter()
                .any(|dep| results.get(dep) == Some(&false))
            {
                path_issue(
                    receipt,
                    "Kept dotfile because its dependency was blocked",
                    &path,
                    COPY_ACTION,
                );
                results.insert(path, false);
                continue;
            }
            let changed_now = self.apply_target(
                run,
                git,
                git_dir,
                tree,
                receipt,
                &filesystem,
                &observed,
                &path,
                &target,
                definition.as_ref(),
                &configs,
                &all_jobs,
                &mut changed,
            );
            results.insert(path.clone(), matches!(changed_now, Ok(true)));
            if changed_now.is_err() {
                path_issue(
                    receipt,
                    "Could not apply dotfile safely",
                    &path,
                    COPY_ACTION,
                );
            }
        }
        self.persist(git_dir)?;

        let mut reloaded = BTreeSet::new();
        for (selector, command) in &configs.reload {
            let matches = absolute_target(run, Path::new(selector))
                .ok()
                .is_some_and(|pattern| changed.iter().any(|path| path_matches(&pattern, path)));
            if matches && reloaded.insert(command) && invoke(run, git.root(), command).is_err() {
                issue(
                    receipt,
                    "mise dotfile reload failed",
                    "Check the matching history.reload command and rerun it.",
                );
            }
        }
        for hook in &configs.post {
            if invoke(run, git.root(), hook).is_err() {
                issue(
                    receipt,
                    "mise post-dotfiles hook failed",
                    "Check the post-dotfiles hook and rerun dots sync.",
                );
                break;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_target(
        &mut self,
        run: &Run<'_>,
        git: &Git<'_>,
        git_dir: &Path,
        tree: &Tree,
        receipt: &mut Receipt,
        filesystem: &Dir,
        observed: &BTreeMap<PathBuf, io::Result<Image>>,
        destination: &Path,
        target: &Target,
        definition: Option<&Item>,
        configs: &Configs,
        all_jobs: &BTreeSet<PathBuf>,
        changed: &mut BTreeSet<PathBuf>,
    ) -> io::Result<bool> {
        let mut desired = BTreeMap::new();
        let mut sources = BTreeMap::new();
        match target.mode.as_str() {
            "copy" | "symlink-each" => {
                let source = target
                    .effective_source()
                    .ok_or_else(|| invalid("copy has no source"))?;
                let rules = configs.rules(run, target, destination);
                let mut walk = CopyWalk {
                    source,
                    destination,
                    tree,
                    root: git.root(),
                    rules: &rules,
                    target,
                    claimed: all_jobs,
                    desired: &mut desired,
                    sources: &mut sources,
                };
                walk.expand(Path::new(""))?;
            }
            "symlink" => {
                let source = target
                    .effective_source()
                    .ok_or_else(|| invalid("link has no source"))?;
                let rules = configs.rules(run, target, destination);
                desired.insert(
                    destination.to_owned(),
                    Image::Link(link_target(source, destination, rules.relative)?),
                );
            }
            "absent" => {
                desired.insert(destination.to_owned(), Image::Absent);
            }
            "template" | "inline" | "content" => {
                let image = configs.render(run, git, git_dir, target, definition)?;
                desired.insert(destination.to_owned(), image);
            }
            _ => return Err(invalid("unrecognized mise dotfile mode")),
        }

        let mut clean = true;
        for (path, mut replacement) in desired {
            let relative_path = fs_relative(&path)?;
            let current = filesystem.read_path(relative_path)?;
            let known = self.known.get(&path);
            let baseline = known
                .map(|known| Image::from_git(git, Some(&known.entry)))
                .transpose()?;
            if let Image::Regular { mode, .. } = &mut replacement {
                if let (
                    Some(known),
                    Image::Regular {
                        mode: current_mode, ..
                    },
                ) = (known, &current)
                {
                    if known.mode.is_none() && path != destination {
                        if let Some(Image::Regular { mode: old, .. }) = baseline.as_ref() {
                            if (old ^ *mode) & 0o100 != 0 && (current_mode ^ *mode) & 0o100 != 0 {
                                path_issue(
                                    receipt,
                                    "Kept a copy with unverified deployed permissions",
                                    &path,
                                    COPY_ACTION,
                                );
                                clean = false;
                                continue;
                            }
                        }
                        *mode = *current_mode;
                    }
                }
            }
            if current != replacement {
                let still_observed = observed
                    .get(&path)
                    .and_then(|value| value.as_ref().ok())
                    .is_none_or(|before| *before == current);
                let manageable = match (&current, known, &baseline) {
                    (Image::Absent, _, _) => true,
                    (_, Some(known), Some(base)) => same_copy(&current, base, known.mode),
                    (Image::Regular { .. }, None, _) => matches!(
                        target.mode.as_str(),
                        "absent" | "template" | "inline" | "content"
                    ),
                    (Image::Link(_), None, _) => {
                        matches!(target.mode.as_str(), "absent" | "symlink")
                    }
                    _ => false,
                };
                if !still_observed || !manageable {
                    path_issue(
                        receipt,
                        "Kept a changed or unmanaged dotfile",
                        &path,
                        COPY_ACTION,
                    );
                    clean = false;
                    continue;
                }

                let Some(parent) = path.parent() else {
                    return Err(invalid("dotfile target has no parent"));
                };
                filesystem.parent(relative_path, !matches!(replacement, Image::Absent))?;
                if install(
                    Path::new("/"),
                    relative_path,
                    git_dir,
                    parent,
                    &current,
                    &replacement,
                    receipt,
                )
                .is_err()
                {
                    path_issue(
                        receipt,
                        "Kept dotfile after a guarded write failed",
                        &path,
                        COPY_ACTION,
                    );
                    if git_dir.join(PENDING).exists() {
                        return Err(invalid("dotfile replacement recovery is pending"));
                    }
                    clean = false;
                    continue;
                }
                changed.insert(path.clone());
            }
            self.known.remove(&path);
            if let (Some(source), Image::Regular { mode, .. }) = (sources.get(&path), &replacement)
            {
                if let Ok(source) = source.strip_prefix(git.root()) {
                    if let Some(entry) = tree.get(source) {
                        if replacement.git_eq(&Image::from_git(git, Some(entry))?) {
                            self.known.insert(
                                path,
                                KnownCopy {
                                    source: source.into(),
                                    entry: entry.clone(),
                                    mode: Some(*mode),
                                },
                            );
                        }
                    }
                }
            }
        }
        Ok(clean)
    }
}

pub(super) fn retire(
    git: &Git<'_>,
    git_dir: &Path,
    retired: Vec<Retirement>,
    receipt: &mut Receipt,
) -> io::Result<bool> {
    let mut clean = true;
    for row in retired {
        if install(
            git.root(),
            &row.path,
            git_dir,
            git_dir,
            &row.before,
            &row.after,
            receipt,
        )
        .is_err()
        {
            path_issue(
                receipt,
                "Kept a changed machine-local declaration",
                &row.path,
                "Review the machine-local file and rerun dots sync.",
            );
            clean = false;
        }
    }
    Ok(clean)
}

const PENDING: &str = "dots-sync-write.json";

pub(super) fn has_pending(directory: &Path) -> bool {
    match fs::symlink_metadata(directory.join(PENDING)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        _ => true,
    }
}

#[derive(Deserialize, Serialize)]
struct WritePlan {
    anchor: PathBuf,
    path: PathBuf,
    stage: PathBuf,
}

fn install(
    anchor: &Path,
    path: &Path,
    git_dir: &Path,
    recovery_parent: &Path,
    before: &Image,
    after: &Image,
    receipt: &mut Receipt,
) -> io::Result<()> {
    let plan = prepare_write(
        anchor,
        path,
        git_dir,
        recovery_parent,
        before,
        after,
        |record, path| {
            record
                .persist_noclobber(path)
                .map(drop)
                .map_err(io::Error::other)
        },
    )?;
    complete_write(git_dir, &plan, receipt)
}

fn prepare_write(
    anchor: &Path,
    path: &Path,
    git_dir: &Path,
    recovery_parent: &Path,
    before: &Image,
    after: &Image,
    save_record: impl FnOnce(tempfile::NamedTempFile, PathBuf) -> io::Result<()>,
) -> io::Result<WritePlan> {
    let private = private_dir(recovery_parent, ".dots-recovery-")?;
    write_private(&private.path().join("expected"), before)?;
    write_private(&private.path().join("replacement"), after)?;
    File::open(private.path())?.sync_all()?;
    let plan = WritePlan {
        anchor: anchor.into(),
        path: path.into(),
        stage: private.path().into(),
    };
    let mut record = tempfile::NamedTempFile::new_in(git_dir)?;
    record.write_all(&serde_json::to_vec(&plan).map_err(io::Error::other)?)?;
    record.as_file().sync_all()?;
    // Publish the private staging directory before any live rename.
    save_record(record, git_dir.join(PENDING))?;
    let _stage = private.keep();
    File::open(git_dir)?.sync_all()?;
    Ok(plan)
}

pub(super) fn recover(git_dir: &Path, receipt: &mut Receipt) -> io::Result<()> {
    let pending = git_dir.join(PENDING);
    let stream = match File::open(&pending) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let plan: WritePlan = serde_json::from_reader(stream).map_err(io::Error::other)?;
    complete_write(git_dir, &plan, receipt)
}

fn complete_write(git_dir: &Path, plan: &WritePlan, receipt: &mut Receipt) -> io::Result<()> {
    reconcile_write(git_dir, plan, receipt, replace_exact)
}

fn reconcile_write(
    git_dir: &Path,
    plan: &WritePlan,
    receipt: &mut Receipt,
    write: impl FnOnce(&Dir, &Path, &Dir, &CStrName, &CStrName, &Image, &Image) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if !plan.anchor.is_absolute() || !plan.stage.is_absolute() {
        return Err(invalid("invalid guarded write journal"));
    }
    relative(plan.path.as_os_str().as_bytes())?;
    let root = Dir::open(&plan.anchor)?;
    let stage = Dir::open(&plan.stage)?;
    let before = stage.read_path(Path::new("expected"))?;
    let after = stage.read_path(Path::new("replacement"))?;
    let result = write(
        &root,
        &plan.path,
        &stage,
        &CStrName::new("previous")?,
        &CStrName::new("replacement")?,
        &before,
        &after,
    );
    let previous = plan.stage.join("previous");
    if fs::symlink_metadata(&previous).is_ok() {
        receipt.recovery_files.push(previous.clone());
        if result.is_err() && root.read_path(&plan.path)? == Image::Absent {
            return result;
        }
    }
    // Removing the control record before cleaning staging makes either a
    // retry or completed displacement recoverable across interruption.
    fs::remove_file(git_dir.join(PENDING))?;
    File::open(git_dir)?.sync_all()?;
    if fs::symlink_metadata(previous).is_ok() {
        for leaf in ["expected", "replacement"] {
            let _ = fs::remove_file(plan.stage.join(leaf));
        }
    } else {
        fs::remove_dir_all(&plan.stage)?;
    }
    result
}

fn same_copy(current: &Image, base: &Image, mode: Option<u32>) -> bool {
    match (current, base) {
        (
            Image::Regular {
                bytes: actual,
                mode: actual_mode,
            },
            Image::Regular {
                bytes: expected, ..
            },
        ) => actual == expected && mode.is_none_or(|mode| mode == *actual_mode),
        _ => false,
    }
}

fn status(run: &Run<'_>, root: &Path) -> io::Result<Vec<Target>> {
    let output = validation::probe(
        run,
        root,
        ["bootstrap", "dotfiles", "status", "--json"],
        false,
    )?;
    serde_json::from_slice::<Status>(&output)
        .map(|status| status.files)
        .map_err(io::Error::other)
}

fn absolute_target(run: &Run<'_>, path: &Path) -> io::Result<PathBuf> {
    let mut parts = path.components();
    if let Some(std::path::Component::Normal(tilde)) = parts.next() {
        if tilde == "~" {
            let home = run
                .ctx()
                .env
                .home()
                .ok_or_else(|| invalid("HOME is unavailable"))?;
            let result = home.join(parts.as_path());
            fs_relative(&result)?;
            return Ok(result);
        }
    }
    fs_relative(path)?;
    Ok(path.to_owned())
}

fn fs_relative(path: &Path) -> io::Result<&Path> {
    let relative = path.strip_prefix("/").map_err(io::Error::other)?;
    use std::os::unix::ffi::OsStrExt;
    self::relative(relative.as_os_str().as_bytes())?;
    Ok(relative)
}

#[derive(Default)]
struct Rules {
    exclude: Vec<String>,
    dot_prefix: bool,
    manifest: bool,
    relative: bool,
}

impl Rules {
    fn includes(&self, path: &Path) -> bool {
        !self.exclude.iter().any(|pattern| {
            if pattern.contains('/') {
                path_matches(Path::new(pattern), path)
            } else {
                path.components()
                    .any(|part| component_matches(pattern.as_ref(), part.as_os_str()))
            }
        })
    }

    fn destination(&self, path: &Path) -> PathBuf {
        if !self.dot_prefix {
            return path.to_owned();
        }
        path.components()
            .map(|part| {
                let part = part.as_os_str();
                part.to_str()
                    .and_then(|part| part.strip_prefix("dot-"))
                    .map_or_else(|| part.to_owned(), |tail| format!(".{tail}").into())
            })
            .collect()
    }
}

fn path_matches(pattern: &Path, path: &Path) -> bool {
    fn matches(pattern: &[&std::ffi::OsStr], path: &[&std::ffi::OsStr]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((head, tail)) if *head == "**" => {
                (0..=path.len()).any(|n| matches(tail, &path[n..]))
            }
            Some((head, tail)) => path
                .split_first()
                .is_some_and(|(value, rest)| component_matches(head, value) && matches(tail, rest)),
        }
    }
    let pattern = pattern
        .components()
        .map(|component| component.as_os_str())
        .collect::<Vec<_>>();
    let path = path
        .components()
        .map(|component| component.as_os_str())
        .collect::<Vec<_>>();
    matches(&pattern, &path)
}

fn component_matches(pattern: &std::ffi::OsStr, value: &std::ffi::OsStr) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(pattern) = CString::new(pattern.as_bytes()) else {
        return false;
    };
    let Ok(value) = CString::new(value.as_bytes()) else {
        return false;
    };
    // SAFETY: both arguments are live, NUL-terminated byte strings.
    unsafe { nix::libc::fnmatch(pattern.as_ptr(), value.as_ptr(), 0) == 0 }
}

struct CopyWalk<'a> {
    source: &'a Path,
    destination: &'a Path,
    root: &'a Path,
    tree: &'a Tree,
    rules: &'a Rules,
    target: &'a Target,
    claimed: &'a BTreeSet<PathBuf>,
    desired: &'a mut BTreeMap<PathBuf, Image>,
    sources: &'a mut BTreeMap<PathBuf, PathBuf>,
}

impl CopyWalk<'_> {
    fn expand(&mut self, relative: &Path) -> io::Result<()> {
        if !self.rules.includes(relative) {
            return Ok(());
        }
        let top = relative.as_os_str().is_empty();
        let source = if top {
            self.source.to_owned()
        } else {
            self.source.join(relative)
        };
        let target = if top {
            self.destination.to_owned()
        } else {
            self.destination.join(self.rules.destination(relative))
        };
        if !top && self.claimed.contains(&target) {
            return Ok(());
        }
        let md = fs::symlink_metadata(&source)?;
        if md.is_dir() {
            for leaf in fs::read_dir(&source)? {
                let file = leaf?.file_name();
                if file != ".git" {
                    self.expand(&relative.join(file))?;
                }
            }
        } else if !self.rules.manifest
            || source
                .strip_prefix(self.root)
                .is_ok_and(|path| self.tree.contains_key(path))
        {
            let image = if self.target.mode == "symlink-each" {
                Image::Link(link_target(&source, &target, self.rules.relative)?)
            } else {
                read_copy(&source, self.target.permissions.as_deref())?
            };
            self.sources.insert(target.clone(), source);
            self.desired.insert(target, image);
        }
        Ok(())
    }
}

fn read_copy(source: &Path, permissions: Option<&str>) -> io::Result<Image> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(source)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("copy source is not a regular file"));
    }
    let mode = match permissions {
        Some(value) => u32::from_str_radix(value, 8).map_err(io::Error::other)? & 0o777,
        None => metadata.permissions().mode() & 0o777,
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(Image::Regular { bytes, mode })
}

fn link_target(source: &Path, destination: &Path, relative: bool) -> io::Result<PathBuf> {
    if !relative {
        return Ok(source.to_owned());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("symlink target has no parent"))?;
    let common = parent
        .ancestors()
        .find(|path| source.starts_with(path))
        .ok_or_else(|| invalid("symlink paths have no common ancestor"))?;
    let mut result = PathBuf::new();
    for _ in parent
        .strip_prefix(common)
        .map_err(io::Error::other)?
        .components()
    {
        result.push("..");
    }
    result.push(source.strip_prefix(common).map_err(io::Error::other)?);
    Ok(result)
}

#[derive(Default)]
struct Configs {
    docs: Vec<(PathBuf, DocumentMut)>,
    pre: Vec<String>,
    post: Vec<String>,
    reload: Vec<(String, String)>,
}

impl Configs {
    fn read_at(run: &Run<'_>, root: &Path, preview: bool) -> io::Result<Self> {
        #[derive(Deserialize)]
        struct Config {
            path: PathBuf,
        }
        let bytes = validation::probe(run, root, ["config", "ls", "--json"], preview)?;
        let configs: Vec<Config> = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        let mut result = Self::default();
        for config in configs {
            if !config.path.starts_with(root) {
                continue;
            }
            let text = fs::read_to_string(&config.path)?;
            let doc: DocumentMut = text.parse().map_err(io::Error::other)?;
            if let Some(hooks) = doc.get("bootstrap").and_then(|item| item.get("hooks")) {
                if let Some(pre) = hooks.get("pre-dotfiles") {
                    result.pre.extend(strings(pre));
                }
                if let Some(post) = hooks.get("post-dotfiles") {
                    result.post.extend(strings(post));
                }
            }
            if let Some(reload) = doc
                .get("history")
                .and_then(|item| item.get("reload"))
                .and_then(Item::as_table_like)
            {
                for (path, command) in reload.iter() {
                    result.reload.extend(
                        strings(command)
                            .into_iter()
                            .map(|command| (path.to_owned(), command)),
                    );
                }
            }
            result.docs.push((config.path, doc));
        }
        Ok(result)
    }

    fn definition(&self, target: &Target, path: &Path) -> Option<Item> {
        let direct = |entries: &dyn toml_edit::TableLike| {
            entries
                .get(target.target.to_str()?)
                .or_else(|| entries.get(path.to_str()?))
                .cloned()
        };
        self.docs
            .iter()
            .rev()
            .filter(|(config, _)| {
                target
                    .origin
                    .as_ref()
                    .is_none_or(|origin| *config == origin.config)
            })
            .find_map(|(_, doc)| {
                doc.get("dotfiles")
                    .and_then(Item::as_table_like)
                    .and_then(direct)
                    .or_else(|| {
                        doc.get("dotfile_groups")?
                            .as_table_like()?
                            .iter()
                            .filter_map(|(_, group)| group.get("entries")?.as_table_like())
                            .find_map(direct)
                    })
            })
    }

    fn group<'a>(
        &'a self,
        run: &Run<'_>,
        target: &Target,
        path: &Path,
    ) -> Option<(&'a str, &'a Item)> {
        self.docs
            .iter()
            .rev()
            .filter(|(config, _)| {
                target
                    .origin
                    .as_ref()
                    .is_none_or(|origin| *config == origin.config)
            })
            .flat_map(|(_, doc)| {
                doc.get("dotfile_groups")
                    .and_then(Item::as_table_like)
                    .into_iter()
                    .flat_map(toml_edit::TableLike::iter)
            })
            .find(|(_, group)| {
                let dest = group.get("target").and_then(Item::as_str).unwrap_or("~");
                if absolute_target(run, Path::new(dest)).is_ok_and(|dest| dest == path) {
                    return true;
                }
                group
                    .get("entries")
                    .and_then(Item::as_table_like)
                    .is_some_and(|entries| {
                        target
                            .target
                            .to_str()
                            .and_then(|name| entries.get(name))
                            .is_some()
                    })
            })
    }

    fn rules(&self, run: &Run<'_>, target: &Target, path: &Path) -> Rules {
        let definition = self.definition(target, path);
        let group = self.group(run, target, path).map(|(_, item)| item);
        let property = |name| {
            definition
                .as_ref()
                .and_then(|item| item.get(name))
                .or_else(|| group.and_then(|item| item.get(name)))
        };
        let exclude = property("exclude").map(strings).unwrap_or_default();
        let dot_prefix = property("dot_prefix")
            .and_then(Item::as_bool)
            .unwrap_or(false);
        let manifest = property("manifest").and_then(Item::as_str) == Some("git");
        let relative = property("relative")
            .and_then(Item::as_bool)
            .unwrap_or(false);
        Rules {
            exclude,
            dot_prefix,
            manifest,
            relative,
        }
    }

    fn render(
        &self,
        run: &Run<'_>,
        git: &Git<'_>,
        git_dir: &Path,
        target: &Target,
        item: Option<&Item>,
    ) -> io::Result<Image> {
        let tmp = private_dir(git_dir, "dots-dotfile-")?;
        let root = tmp.path().join("config");
        fs::create_dir(&root)?;
        let destination = tmp.path().join("rendered");
        let destination_text = destination
            .to_str()
            .ok_or_else(|| invalid("invalid private target path"))?;
        let mut inserted = false;
        for (path, mut doc) in self.docs.clone() {
            doc.remove("dotfiles");
            doc.remove("dotfile_groups");
            if let Some(bootstrap) = doc.get_mut("bootstrap").and_then(Item::as_table_like_mut) {
                bootstrap.remove("hooks");
            }
            if let Some(history) = doc.get_mut("history").and_then(Item::as_table_like_mut) {
                history.remove("reload");
            }
            if target
                .origin
                .as_ref()
                .is_some_and(|origin| origin.config == path)
            {
                let mut definition = item.cloned().unwrap_or_else(|| Item::Table(Table::new()));
                if let Some(table) = definition.as_table_like_mut() {
                    table.remove("depends");
                    if let Some(source) = target.effective_source() {
                        if let Some(source) = source.to_str().filter(|_| source.is_absolute()) {
                            table.insert("source", value(source));
                        }
                    }
                    if let Some(array) = table.get_mut("variants").and_then(Item::as_array_mut) {
                        for entry in array.iter_mut() {
                            if let Some(entry) = entry.as_inline_table_mut() {
                                entry.remove("target");
                                entry.remove("source");
                            }
                        }
                    }
                    if item.is_none() {
                        table.insert("mode", value("template"));
                    }
                }
                let mut dotfiles = Table::new();
                dotfiles.insert(destination_text, definition);
                doc.insert("dotfiles", Item::Table(dotfiles));
                inserted = true;
            }
            let relative = path.strip_prefix(git.root()).map_err(io::Error::other)?;
            if relative.components().count() != 1 {
                return Err(invalid("cannot isolate a nested mise configuration"));
            }
            write_private(
                &root.join(relative),
                &Image::Regular {
                    bytes: doc.to_string().into_bytes(),
                    mode: 0o600,
                },
            )?;
        }
        if !inserted {
            return Err(invalid("cannot isolate mise template definition"));
        }
        for selector in ["miserc.toml", "mise.lock"] {
            if let Ok(image @ Image::Regular { .. }) =
                Dir::open(git.root())?.read_path(Path::new(selector))
            {
                if !root.join(selector).exists() {
                    write_private(&root.join(selector), &image)?;
                }
            }
        }
        let mut command = validation::command(
            run,
            &root,
            [
                "bootstrap",
                "dotfiles",
                "apply",
                "--yes",
                "--",
                destination_text,
            ],
            true,
        )?;
        command
            .env("DOTS_ROOT", git.root())
            .env("MISE_STATE_DIR", tmp.path().join("state"));
        if !run.probe(command)?.status.success() {
            return Err(invalid("mise could not stage a dotfile"));
        }
        let image = Dir::open(tmp.path())?.read_path(Path::new("rendered"))?;
        if !matches!(image, Image::Regular { .. } | Image::Link(_)) {
            return Err(invalid("mise did not create a staged dotfile"));
        }
        Ok(image)
    }
}

fn strings(item: &Item) -> Vec<String> {
    if let Some(value) = item.as_str() {
        vec![value.into()]
    } else {
        item.as_array()
            .into_iter()
            .flatten()
            .filter_map(toml_edit::Value::as_str)
            .map(str::to_owned)
            .collect()
    }
}

fn invoke(run: &Run<'_>, _root: &Path, expression: &str) -> io::Result<()> {
    if run.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let command = crate::process::Mise::new(run.ctx())
        .map_err(io::Error::other)?
        .exec("/bin/sh", ["-o", "errexit", "-c", expression])
        .map_err(io::Error::other)?;
    if run.probe(command)?.status.success() {
        Ok(())
    } else {
        Err(invalid("mise hook failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::guard::replace_at_boundary;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        git: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("checkout");
            let git = root.join(".git");
            fs::create_dir_all(&git).unwrap();
            fs::write(root.join("source"), b"committed source").unwrap();
            fs::write(temp.path().join("deployed"), b"deployed edit").unwrap();
            Self {
                _temp: temp,
                root,
                git,
            }
        }

        fn receipt(&self) -> Receipt {
            Receipt::new(Some(self.root.clone()), "fixture")
        }

        fn prepare(
            &self,
            save: impl FnOnce(tempfile::NamedTempFile, PathBuf) -> io::Result<()>,
        ) -> io::Result<WritePlan> {
            prepare_write(
                &self.root,
                Path::new("source"),
                &self.git,
                &self.git,
                &Dir::open(&self.root)?.read_path(Path::new("source"))?,
                &Dir::open(self._temp.path())?.read_path(Path::new("deployed"))?,
                save,
            )
        }

        fn stage(&self) -> WritePlan {
            self.prepare(|record, path| {
                record
                    .persist_noclobber(path)
                    .map(drop)
                    .map_err(io::Error::other)
            })
            .unwrap()
        }

        fn original_versions(&self) {
            assert_eq!(
                fs::read(self.root.join("source")).unwrap(),
                b"committed source"
            );
            assert_eq!(
                fs::read(self._temp.path().join("deployed")).unwrap(),
                b"deployed edit"
            );
        }
    }

    #[test]
    fn failed_capture_journal_keeps_both_versions_and_can_retry() {
        let fixture = Fixture::new();
        let failure = fixture.prepare(|_, _| Err(io::Error::from_raw_os_error(nix::libc::ENOSPC)));
        assert_eq!(
            failure.err().unwrap().raw_os_error(),
            Some(nix::libc::ENOSPC)
        );
        fixture.original_versions();
        assert_eq!(fs::read_dir(&fixture.git).unwrap().count(), 0);

        fixture.stage();
        recover(&fixture.git, &mut fixture.receipt()).unwrap();
        assert_eq!(
            fs::read(fixture.root.join("source")).unwrap(),
            b"deployed edit"
        );
    }

    #[test]
    fn interrupted_capture_recovers_on_both_sides_of_source_displacement() {
        for after_move in [false, true] {
            let fixture = Fixture::new();
            let plan = fixture.stage();
            if after_move {
                fs::rename(fixture.root.join("source"), plan.stage.join("previous")).unwrap();
            }
            let mut receipt = fixture.receipt();
            recover(&fixture.git, &mut receipt).unwrap();
            assert_eq!(
                fs::read(fixture.root.join("source")).unwrap(),
                b"deployed edit"
            );
            assert_eq!(receipt.recovery_files, [plan.stage.join("previous")]);
            assert_eq!(
                fs::read(&receipt.recovery_files[0]).unwrap(),
                b"committed source"
            );
            assert!(!fixture.git.join(PENDING).exists());
            assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 2);
        }
    }

    #[test]
    fn cross_device_capture_keeps_both_versions_and_clears_its_journal() {
        let fixture = Fixture::new();
        let plan = fixture.stage();
        let mut receipt = fixture.receipt();
        let error = reconcile_write(
            &fixture.git,
            &plan,
            &mut receipt,
            |root, path, stage, old, new, before, after| {
                replace_at_boundary(
                    root,
                    path,
                    stage,
                    old,
                    new,
                    before,
                    after,
                    || Err(io::Error::from_raw_os_error(nix::libc::EXDEV)),
                    || Ok(()),
                )
            },
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::EXDEV));
        fixture.original_versions();
        assert!(receipt.recovery_files.is_empty());
        assert_eq!(fs::read_dir(&fixture.git).unwrap().count(), 0);
    }

    #[test]
    fn competing_writer_keeps_target_and_reports_the_displaced_edit() {
        let fixture = Fixture::new();
        let plan = fixture.stage();
        let mut receipt = fixture.receipt();
        let live = fixture.root.join("source");
        let error = reconcile_write(
            &fixture.git,
            &plan,
            &mut receipt,
            |root, path, stage, old, new, before, after| {
                replace_at_boundary(
                    root,
                    path,
                    stage,
                    old,
                    new,
                    before,
                    after,
                    || fs::write(&live, b"last moment edit"),
                    || fs::write(&live, b"competing writer"),
                )
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(live).unwrap(), b"competing writer");
        assert_eq!(receipt.recovery_files, [plan.stage.join("previous")]);
        assert_eq!(
            fs::read(&receipt.recovery_files[0]).unwrap(),
            b"last moment edit"
        );
        assert!(!fixture.git.join(PENDING).exists());
    }

    #[test]
    fn a_late_claim_survives_publication_of_bytes_from_a_symlink_source() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let source = fixture._temp.path().join("linked-source");
        symlink(fixture._temp.path().join("deployed"), &source).unwrap();
        let live = fixture.root.join("new-target");
        let planned = read_copy(&source, None).unwrap();
        let plan = prepare_write(
            &fixture.root,
            Path::new("new-target"),
            &fixture.git,
            &fixture.git,
            &Image::Absent,
            &planned,
            |record, path| {
                record
                    .persist_noclobber(path)
                    .map(drop)
                    .map_err(io::Error::other)
            },
        )
        .unwrap();
        let mut receipt = fixture.receipt();
        let error = reconcile_write(
            &fixture.git,
            &plan,
            &mut receipt,
            |root, path, stage, old, new, before, after| {
                replace_at_boundary(
                    root,
                    path,
                    stage,
                    old,
                    new,
                    before,
                    after,
                    || Ok(()),
                    || fs::write(&live, b"another file"),
                )
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&live).unwrap(), b"another file");
        assert_eq!(fs::read(source).unwrap(), b"deployed edit");
    }
}
