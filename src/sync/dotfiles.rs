use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{receipt::Receipt, run::Run};

use super::git::{Entry, Git, Tree, invalid, local, relative};
use super::guard::{CStrName, Dir, Image, private_dir, replace_exact, write_private};
use super::validation::{self, Retirement, Validated};
use super::{issue, path_issue};

#[derive(Deserialize)]
struct Status {
    files: Vec<Target>,
}

#[derive(Clone, Deserialize, Eq, PartialEq)]
struct Target {
    target: PathBuf,
    source: Option<PathBuf>,
    mode: String,
    origin: Option<Origin>,
    permissions: Option<String>,
    #[serde(default)]
    omitted: usize,
}

#[derive(Clone, Deserialize, Eq, PartialEq)]
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

pub(super) struct ApplyPlan {
    root: PathBuf,
    selectors: String,
    targets: Vec<Target>,
    configs: Configs,
    observed: BTreeMap<PathBuf, Image>,
    _external: tempfile::TempDir,
    external: BTreeMap<PathBuf, PathBuf>,
}

impl ApplyPlan {
    pub fn new(run: &Run<'_>, root: PathBuf, live: &Path, selectors: String) -> io::Result<Self> {
        let targets = planned_targets(run, &root, live)?;
        let mut configs = Configs::read_at(run, &root, true)?;
        for (path, _) in &mut configs.docs {
            remap_path(path, &root, live);
        }
        let filesystem = Dir::open(Path::new("/"))?;
        let mut observed = BTreeMap::new();
        let external_view = private_dir(
            root.parent()
                .ok_or_else(|| invalid("validation root has no parent"))?,
            "external-sources-",
        )?;
        let mut external = BTreeMap::new();
        for target in &targets {
            if matches!(target.mode.as_str(), "track" | "track-local") {
                continue;
            }
            let path = absolute_target(run, &target.target)?;
            if let Ok(image) = filesystem.read_path(fs_relative(&path)?) {
                observed.insert(path, image);
            }
            if let Some(source) = target.effective_source()
                && source.is_absolute()
                && !source.starts_with(live)
                && !external.contains_key(source)
            {
                let staged = external_view.path().join(external.len().to_string());
                if validation::source_exists(Path::new("/"), source)? {
                    stage_external_source(source, &staged, live, &root)?;
                }
                external.insert(source.to_owned(), staged);
            }
        }
        Ok(Self {
            root,
            selectors,
            targets,
            configs,
            observed,
            _external: external_view,
            external,
        })
    }

    fn source_path(&self, live: &Path, source: &Path) -> io::Result<PathBuf> {
        if let Ok(relative) = source.strip_prefix(live) {
            return Ok(self.root.join(relative));
        }
        self.external
            .get(source)
            .cloned()
            .ok_or_else(|| invalid("source has no validated representation"))
    }

    fn verify_source(&self, path: &Path) -> io::Result<()> {
        let physical = path.canonicalize()?;
        if !physical.starts_with(&self.root) && !physical.starts_with(self._external.path()) {
            return Err(invalid("source escapes the validated workspace"));
        }
        Ok(())
    }
}

fn remap_path(path: &mut PathBuf, overlay: &Path, live: &Path) {
    if let Ok(relative) = path.strip_prefix(overlay) {
        *path = live.join(relative);
    }
}

fn planned_targets(run: &Run<'_>, overlay: &Path, live: &Path) -> io::Result<Vec<Target>> {
    let mut targets = status_at(run, overlay, true)?;
    for target in &mut targets {
        remap_path(&mut target.target, overlay, live);
        if let Some(source) = target.source.as_mut() {
            remap_path(source, overlay, live);
        }
        if let Some(origin) = target.origin.as_mut() {
            remap_path(&mut origin.config, overlay, live);
            if let Some(source) = origin.source.as_mut() {
                remap_path(source, overlay, live);
            }
        }
    }
    Ok(targets)
}

#[derive(Eq, PartialEq)]
struct HookFile {
    mode: Option<u32>,
    digest: [u8; 32],
}

impl HookFile {
    fn of(image: &Image) -> Option<Self> {
        match image {
            Image::Regular { bytes, mode } => Some(Self {
                mode: Some(*mode),
                digest: Sha256::digest(bytes).into(),
            }),
            Image::Link(link) => Some(Self {
                mode: None,
                digest: Sha256::digest(link.as_os_str().as_bytes()).into(),
            }),
            _ => None,
        }
    }
}

fn hook_tree(root: &Path) -> io::Result<BTreeMap<PathBuf, HookFile>> {
    let view = Dir::open(root)?;
    let mut files = BTreeMap::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(root.join(&directory))? {
            let path = directory.join(entry?.file_name());
            if relative(path.as_os_str().as_bytes()).is_err()
                || local(&path)
                || path.components().next().is_some_and(|name| {
                    matches!(
                        name.as_os_str().to_str(),
                        Some(
                            ".dots-cache" | ".dots-state" | ".dots-tmp" | ".dots-no-system-config"
                        )
                    )
                })
            {
                continue;
            }
            let image = view.read_path(&path)?;
            if image == Image::Directory {
                pending.push(path);
            } else if let Some(image) = HookFile::of(&image) {
                files.insert(path, image);
            }
        }
    }
    Ok(files)
}

fn without_deployment_permissions(mut doc: DocumentMut) -> String {
    let remove_entry_permissions = |table: &mut dyn toml_edit::TableLike| {
        for (_, entry) in table.iter_mut() {
            if let Some(entry) = entry.as_table_like_mut() {
                entry.remove("permissions");
            }
        }
    };
    if let Some(dotfiles) = doc.get_mut("dotfiles").and_then(Item::as_table_like_mut) {
        remove_entry_permissions(dotfiles);
    }
    if let Some(groups) = doc
        .get_mut("dotfile_groups")
        .and_then(Item::as_table_like_mut)
    {
        for (_, group) in groups.iter_mut() {
            if let Some(group) = group.as_table_like_mut() {
                group.remove("permissions");
                if let Some(entries) = group.get_mut("entries").and_then(Item::as_table_like_mut) {
                    remove_entry_permissions(entries);
                }
            }
        }
    }
    doc.to_string()
}

fn checked_hook_targets(run: &Run<'_>, plan: &ApplyPlan, live: &Path) -> io::Result<Vec<Target>> {
    if validation::overlay_environments(run, &plan.root)? != plan.selectors {
        return Err(invalid("pre-dotfiles hook changed the machine selection"));
    }
    let discovered = Configs::read_at(run, &plan.root, true)?;
    if discovered.docs.len() != plan.configs.docs.len() {
        return Err(invalid(
            "pre-dotfiles hook changed the selected configuration files",
        ));
    }
    for ((old_path, old), (mut path, current)) in plan.configs.docs.iter().zip(discovered.docs) {
        remap_path(&mut path, &plan.root, live);
        if old_path != &path
            || without_deployment_permissions(old.clone())
                != without_deployment_permissions(current)
        {
            return Err(invalid("pre-dotfiles hook changed a validated declaration"));
        }
    }
    let mut targets = planned_targets(run, &plan.root, live)?;
    targets.sort_by(|first, second| first.target.cmp(&second.target));
    let mut approved = plan.targets.iter().collect::<Vec<_>>();
    approved.sort_by(|first, second| first.target.cmp(&second.target));
    if targets.len() != approved.len()
        || targets.iter().zip(approved).any(|(actual, approved)| {
            let mut actual = (*actual).clone();
            actual.permissions.clone_from(&approved.permissions);
            &actual != approved
        })
    {
        return Err(invalid("pre-dotfiles hook changed the resolved targets"));
    }
    validation::probe(
        run,
        &plan.root,
        [
            "bootstrap",
            "dotfiles",
            "apply",
            "--dry-run",
            "--force",
            "--yes",
        ],
        true,
    )?;
    Ok(targets)
}

fn apply_pre_hooks(
    run: &Run<'_>,
    git: &Git<'_>,
    git_dir: &Path,
    tree: &Tree,
    validated: &mut Validated,
    receipt: &mut Receipt,
) -> io::Result<bool> {
    let plan = &validated.apply;
    if plan.configs.pre.is_empty() {
        return Ok(true);
    }
    let initial = hook_tree(&plan.root)?;
    let live = Dir::open(git.root())?;
    let originals = initial
        .keys()
        .map(|path| {
            live.read_path(path)
                .map(|image| (path, HookFile::of(&image)))
        })
        .collect::<io::Result<BTreeMap<_, _>>>()?;
    let mut targets = None;
    let mut continue_apply = true;
    for hook in &plan.configs.pre {
        validated.check_live_inputs(git)?;
        if hook_failed(
            receipt,
            "mise pre-dotfiles hook failed",
            "Check the pre-dotfiles hook and rerun dots sync.",
            invoke(run, &plan.root, &plan.root, &plan.selectors, hook),
        ) {
            continue_apply = false;
            break;
        }
        match validated
            .check_overlay_locals()
            .and_then(|()| checked_hook_targets(run, plan, git.root()))
        {
            Ok(confirmed) => targets = Some(confirmed),
            Err(_) => {
                issue(
                    receipt,
                    "Pre-dotfiles hook changed validated apply inputs",
                    "Review the hook's checkout edits and rerun dots sync.",
                );
                continue_apply = false;
                break;
            }
        }
    }
    if run.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let after = hook_tree(&plan.root)?;
    let view = Dir::open(&plan.root)?;
    let mut edits = Vec::new();
    for path in initial.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        if initial.get(path) == after.get(path) {
            continue;
        }
        let current = live.read_path(path)?;
        let mut replacement = view.read_path(path)?;
        let original_matches = match originals.get(path) {
            Some(before) => HookFile::of(&current) == *before,
            None => current == Image::Absent,
        };
        if !original_matches || !current.git_eq(&validated.expected_source(git, tree, path)?) {
            path_issue(
                receipt,
                "Kept a checkout edit that conflicts with a pre-dotfiles hook",
                path,
                COPY_ACTION,
            );
            return Ok(false);
        }
        if let (Image::Regular { mode, .. }, Image::Regular { mode: old, .. }) =
            (&mut replacement, &current)
            && initial.get(path).and_then(|before| before.mode) == Some(*mode)
        {
            *mode = *old;
        }
        edits.push((path.to_owned(), current, replacement));
    }
    for (path, before, after) in edits {
        if run.cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        validated.check_live_inputs(git)?;
        if install(
            git.root(),
            &path,
            git_dir,
            git_dir,
            (&before, &after),
            None,
            receipt,
        )
        .is_err()
        {
            path_issue(
                receipt,
                "Kept a checkout edit after a guarded pre-dotfiles write failed",
                &path,
                COPY_ACTION,
            );
            return Ok(false);
        }
        validated.record_hook_file(&path, after);
    }
    if continue_apply && let Some(targets) = targets {
        validated.apply.targets = targets;
    }
    Ok(continue_apply)
}

fn stage_external_source(source: &Path, out: &Path, live: &Path, view: &Path) -> io::Result<()> {
    if fs::symlink_metadata(source)?.is_dir() {
        fs::create_dir(out)?;
        for child in fs::read_dir(source)? {
            let child = child?;
            stage_external_source(&child.path(), &out.join(child.file_name()), live, view)?;
        }
        return Ok(());
    }
    let physical = source.canonicalize()?;
    let staged;
    let readable = if let Ok(relative) = physical.strip_prefix(live) {
        staged = view.join(relative);
        if !staged.canonicalize()?.starts_with(view) {
            return Err(invalid("source escapes the validated workspace"));
        }
        &staged
    } else {
        &physical
    };
    write_private(out, &read_copy(readable, None)?)
}

struct ApplyOutputs {
    directory: PathBuf,
    needed: BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, PathBuf>,
}

impl ApplyOutputs {
    fn record(&mut self, target: &Path, image: &Image) -> io::Result<()> {
        if self.needed.contains(target) && matches!(image, Image::Regular { .. }) {
            let staged = self.directory.join(self.files.len().to_string());
            write_private(&staged, image)?;
            self.files.insert(target.to_owned(), staged);
        }
        Ok(())
    }

    fn still_current(&self, filesystem: &Dir, dependencies: &[PathBuf]) -> io::Result<bool> {
        let staged = Dir::open(&self.directory)?;
        for dependency in dependencies {
            if let Some(path) = self.files.get(dependency) {
                let expected = staged.read_path(
                    path.strip_prefix(&self.directory)
                        .map_err(io::Error::other)?,
                )?;
                if filesystem.read_path(fs_relative(dependency)?)? != expected {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct KnownCopy {
    source: PathBuf,
    entry: Entry,
    mode: Option<u32>,
    #[serde(default)]
    deployed: Option<[u8; 32]>,
}

impl KnownCopy {
    fn matches(&self, observed: &Image, base: &Image, mode: Option<u32>) -> bool {
        if let (
            Some(digest),
            Image::Regular {
                bytes,
                mode: actual,
            },
        ) = (self.deployed, observed)
        {
            <[u8; 32]>::from(Sha256::digest(bytes)) == digest
                && mode.is_none_or(|mode| mode == *actual)
        } else {
            same_copy(observed, base, mode)
        }
    }
}

#[derive(Default)]
pub(super) struct Copies {
    state: CopyState,
    observed: BTreeMap<PathBuf, Image>,
}

#[derive(Default, Deserialize, Serialize)]
struct CopyState {
    known: BTreeMap<PathBuf, KnownCopy>,
    captured: BTreeMap<PathBuf, Captured>,
    changed: BTreeSet<PathBuf>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct Captured {
    mode: u32,
    digest: [u8; 32],
}

const STATE: &str = "dots-sync-copies.json";
const COPY_ACTION: &str = "Review the source and deployed copy, then rerun dots sync.";

impl Copies {
    pub fn load(dir: &Path) -> io::Result<Self> {
        match File::open(dir.join(STATE)) {
            Ok(file) => {
                #[derive(Deserialize)]
                #[serde(untagged)]
                enum OnDisk {
                    Current(CopyState),
                    Previous(BTreeMap<PathBuf, KnownCopy>),
                }
                let state = match serde_json::from_reader(file).map_err(io::Error::other)? {
                    OnDisk::Current(state) => state,
                    OnDisk::Previous(known) => CopyState {
                        known,
                        ..CopyState::default()
                    },
                };
                Ok(Self {
                    state,
                    ..Self::default()
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn persist(&self, dir: &Path) -> io::Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut tmp, &self.state).map_err(io::Error::other)?;
        tmp.as_file().sync_all()?;
        tmp.persist(dir.join(STATE))?;
        File::open(dir)?.sync_all()
    }

    pub fn seed(
        &mut self,
        run: &Run<'_>,
        scratch: &super::snapshot::Scratch<'_>,
        revision: &str,
        tree: &Tree,
    ) -> io::Result<()> {
        // Resolve the previous declarations even when today's local edit
        // already replaced a copy with a template or an absent entry.
        let (_view, root, data) = validation::historical_status(run, scratch, revision, tree)?;
        let Status { files } = serde_json::from_slice(&data).map_err(io::Error::other)?;
        let configs = Configs::read_at(run, &root, true)?;
        for target in files
            .into_iter()
            .filter(|target| target.mode == "copy" && target.omitted == 0)
        {
            let dest = absolute_target(run, &target.target)?;
            let rules = configs.rules(run, &target, &dest);
            if rules.create_only {
                continue;
            }
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
                self.state.known.entry(path).or_insert_with(|| KnownCopy {
                    source: leaf.clone(),
                    entry: entry.clone(),
                    mode: None,
                    deployed: None,
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
        revision: &str,
        base: &Tree,
        receipt: &mut Receipt,
    ) -> io::Result<()> {
        let root = Dir::open(git.root())?;
        let filesystem = Dir::open(Path::new("/"))?;
        let configs = Configs::read_at(run, git.root(), false)?;
        let discovered = status_at(run, git.root(), false)?;
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
            if rules.create_only
                || (target.origin.is_some()
                    && configs.definition(run, &target, &destination).is_none()
                    && configs.group(run, &target, &destination).is_none())
            {
                continue;
            }
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
                    .state
                    .known
                    .entry(path.clone())
                    .or_insert_with(|| KnownCopy {
                        source: leaf.clone(),
                        entry: entry.clone(),
                        mode: None,
                        deployed: None,
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
                if record.matches(&deployed, &common, record.mode) || live == deployed {
                    continue;
                }
                if record.matches(&deployed, &common, None) {
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
                let Image::Regular {
                    bytes,
                    mode: deployed_mode,
                } = &deployed
                else {
                    unreachable!()
                };
                let Image::Regular { mode, .. } = &live else {
                    continue;
                };
                let replacement = Image::Regular {
                    bytes: bytes.clone(),
                    mode: *mode,
                };
                super::verify_dev(git, revision)?;
                if install(
                    git.root(),
                    leaf,
                    git_dir,
                    git_dir,
                    (&live, &replacement),
                    Some(WriteEffect::Capture {
                        deployed: path.clone(),
                        baseline: Captured {
                            mode: *deployed_mode,
                            digest: Sha256::digest(bytes).into(),
                        },
                    }),
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
                    self.state = Self::load(git_dir)?.state;
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
        if self.state.captured.is_empty() {
            return Ok(());
        }
        for (path, observed) in std::mem::take(&mut self.state.captured) {
            let Some(known) = self.state.known.get_mut(&path) else {
                continue;
            };
            let Some(entry) = tree.get(&known.source) else {
                self.state.captured.insert(path, observed);
                continue;
            };
            let shared = Image::from_git(git, Some(entry))?;
            if let Image::Regular { bytes, .. } = &shared
                && <[u8; 32]>::from(Sha256::digest(bytes)) == observed.digest
            {
                known.entry = entry.clone();
                known.mode = Some(observed.mode);
                known.deployed = Some(observed.digest);
            } else {
                self.state.captured.insert(path, observed);
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
        validated: &mut Validated,
        receipt: &mut Receipt,
    ) -> io::Result<()> {
        validated.ready_to_apply(git)?;
        if validated.apply.targets.is_empty()
            || !apply_pre_hooks(run, git, git_dir, tree, validated, receipt)?
        {
            return Ok(());
        }
        let filesystem = Dir::open(Path::new("/"))?;
        let ApplyPlan {
            root,
            selectors,
            targets,
            configs,
            ..
        } = &validated.apply;
        let mut jobs = BTreeMap::new();
        let mut results = BTreeMap::new();
        for target in targets {
            if matches!(target.mode.as_str(), "track" | "track-local") {
                continue;
            }
            let path = absolute_target(run, &target.target)?;
            let declaration = configs.definition(run, target, &path);
            let definition = declaration.map(|decl| decl.item.clone());
            let group = declaration
                .and_then(|decl| decl.group)
                .or_else(|| configs.group(run, target, &path));
            if target.origin.is_some() && definition.is_none() && group.is_none() {
                path_issue(
                    receipt,
                    "Kept a dotfile whose declaration could not be verified",
                    &path,
                    COPY_ACTION,
                );
                results.insert(path, false);
                continue;
            }
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
                    declaration.map(|decl| decl.name.to_owned()),
                ),
            );
        }

        let mut observed = validated.apply.observed.clone();
        observed.append(&mut self.observed);
        let all_jobs = jobs.keys().cloned().collect::<BTreeSet<_>>();
        let mut outputs = ApplyOutputs {
            directory: validated.apply._external.path().join("outputs"),
            needed: targets
                .iter()
                .filter_map(Target::effective_source)
                .filter(|source| all_jobs.contains(*source))
                .map(Path::to_owned)
                .collect(),
            files: BTreeMap::new(),
        };
        fs::create_dir(&outputs.directory)?;
        let mut group_members = BTreeMap::<String, Vec<PathBuf>>::new();
        for (path, (_, _, _, group, declaration)) in &jobs {
            for name in [group, declaration].into_iter().flatten() {
                group_members
                    .entry(name.clone())
                    .or_default()
                    .push(path.clone());
            }
        }
        let dependencies = |names: &[String], current: &Path| {
            names
                .iter()
                .flat_map(|name| {
                    group_members.get(name).map_or_else(
                        || {
                            absolute_target(run, Path::new(name))
                                .into_iter()
                                .collect::<Vec<_>>()
                        },
                        |paths| {
                            paths
                                .iter()
                                .filter(|path| path.as_path() != current)
                                .cloned()
                                .collect()
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        for (path, (_, _, names, _, _)) in &jobs {
            outputs.needed.extend(
                dependencies(names, path)
                    .into_iter()
                    .filter(|dep| all_jobs.contains(dep)),
            );
        }
        while !jobs.is_empty() {
            if run.cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let next = jobs
                .iter()
                .find(|(path, (_, _, deps, _, _))| {
                    dependencies(deps, path)
                        .iter()
                        .all(|dep| !jobs.contains_key(dep))
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
            let (target, definition, depends, _, _) = jobs.remove(&path).expect("job was present");
            let waiting_for = dependencies(&depends, &path);
            if waiting_for
                .iter()
                .any(|dep| results.get(dep) == Some(&false))
                || !outputs.still_current(&filesystem, &waiting_for)?
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
                target,
                definition.as_ref(),
                validated,
                &all_jobs,
                &waiting_for,
                &mut outputs,
            );
            results.insert(path.clone(), matches!(changed_now, Ok(true)));
            if changed_now.is_err() {
                path_issue(
                    receipt,
                    "Could not apply dotfile safely",
                    &path,
                    COPY_ACTION,
                );
                if has_pending(git_dir) {
                    return Err(invalid("dotfile replacement recovery is pending"));
                }
            }
        }
        self.persist(git_dir)?;

        let mut reloaded = BTreeSet::new();
        let mut reload_failed = false;
        for (selector, command) in &configs.reload {
            let matches = absolute_target(run, Path::new(selector))
                .ok()
                .is_some_and(|pattern| {
                    self.state
                        .changed
                        .iter()
                        .any(|path| path_matches(&pattern, path))
                });
            if matches && reloaded.insert(command) {
                validated.ready_to_apply(git)?;
                reload_failed |= hook_failed(
                    receipt,
                    "mise dotfile reload failed",
                    "Check the matching history.reload command, then rerun dots sync.",
                    invoke(run, root, git.root(), selectors, command),
                );
            }
        }
        for hook in &configs.post {
            validated.ready_to_apply(git)?;
            if hook_failed(
                receipt,
                "mise post-dotfiles hook failed",
                "Check the post-dotfiles hook and rerun dots sync.",
                invoke(run, root, git.root(), selectors, hook),
            ) {
                break;
            }
        }
        if !reload_failed {
            self.state.changed.clear();
            self.persist(git_dir)?;
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
        observed: &BTreeMap<PathBuf, Image>,
        destination: &Path,
        target: &Target,
        definition: Option<&Item>,
        validated: &Validated,
        all_jobs: &BTreeSet<PathBuf>,
        dependencies: &[PathBuf],
        outputs: &mut ApplyOutputs,
    ) -> io::Result<bool> {
        validated.ready_to_apply(git)?;
        let configs = &validated.apply.configs;
        let rules = configs.rules(run, target, destination);
        if rules.create_only && filesystem.read_path(fs_relative(destination)?)? != Image::Absent {
            return Ok(true);
        }
        let mut desired = BTreeMap::new();
        let mut sources = BTreeMap::new();
        match target.mode.as_str() {
            "copy" | "symlink-each" => {
                let source = target
                    .effective_source()
                    .ok_or_else(|| invalid("copy has no source"))?;
                let staged = match outputs.files.get(source) {
                    Some(staged) => staged.clone(),
                    None => validated.apply.source_path(git.root(), source)?,
                };
                let mut walk = CopyWalk {
                    source,
                    staged: &staged,
                    plan: &validated.apply,
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
                let staged = validated.apply.source_path(git.root(), source)?;
                validated.apply.verify_source(&staged)?;
                verify_link_source(&validated.apply, &staged, source)?;
                desired.insert(
                    destination.to_owned(),
                    Image::Link(link_target(source, destination, rules.relative)?),
                );
            }
            "absent" => {
                desired.insert(destination.to_owned(), Image::Absent);
            }
            "template" | "inline" | "content" => {
                let input = target
                    .effective_source()
                    .and_then(|source| outputs.files.get(source));
                let image = configs.render(
                    run,
                    git,
                    git_dir,
                    target,
                    definition,
                    (&validated.apply, input.map(PathBuf::as_path)),
                )?;
                desired.insert(destination.to_owned(), image);
            }
            _ => return Err(invalid("unrecognized mise dotfile mode")),
        }

        let mut clean = true;
        for (path, mut replacement) in desired {
            if !outputs.still_current(filesystem, dependencies)? {
                return Err(invalid("a dependency changed before dotfile installation"));
            }
            let relative_path = fs_relative(&path)?;
            let current = filesystem.read_path(relative_path)?;
            if rules.create_only && current != Image::Absent {
                continue;
            }
            let known = self.state.known.get(&path);
            let baseline = known
                .map(|known| Image::from_git(git, Some(&known.entry)))
                .transpose()?;
            if let (
                Image::Regular { mode, .. },
                Some(known),
                Image::Regular {
                    mode: current_mode, ..
                },
            ) = (&mut replacement, known, &current)
                && known.mode.is_none()
                && path != destination
            {
                if let Some(Image::Regular { mode: old, .. }) = baseline.as_ref()
                    && (old ^ *mode) & 0o100 != 0
                    && (current_mode ^ *mode) & 0o100 != 0
                {
                    path_issue(
                        receipt,
                        "Kept a copy with unverified deployed permissions",
                        &path,
                        COPY_ACTION,
                    );
                    clean = false;
                    continue;
                }
                *mode = *current_mode;
            }
            let next_baseline = if let (Some(source), Image::Regular { bytes, mode }) =
                (sources.get(&path), &replacement)
                && let Ok(source) = source.strip_prefix(git.root())
                && let Some(entry) = tree.get(source)
            {
                Some(KnownCopy {
                    source: source.into(),
                    entry: entry.clone(),
                    mode: Some(*mode),
                    deployed: Some(Sha256::digest(bytes).into()),
                })
            } else {
                None
            };
            if current != replacement {
                validated.ready_to_apply(git)?;
                let still_observed = observed
                    .get(&path)
                    .map_or(current == Image::Absent, |before| *before == current);
                let manageable = match (&current, known, &baseline) {
                    (Image::Absent, _, _) => true,
                    (_, Some(known), Some(base)) => known.matches(&current, base, known.mode),
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

                filesystem.parent(relative_path, !matches!(replacement, Image::Absent))?;
                let recovery = recovery_parent(&path, git.root(), git_dir)?;
                self.persist(git_dir)?;
                if install(
                    Path::new("/"),
                    relative_path,
                    git_dir,
                    &recovery,
                    (&current, &replacement),
                    Some(WriteEffect::Deploy {
                        target: path.clone(),
                        baseline: next_baseline,
                    }),
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
                    if has_pending(git_dir) {
                        return Err(invalid("dotfile replacement recovery is pending"));
                    }
                    clean = false;
                    continue;
                }
                self.state = Self::load(git_dir)?.state;
            } else if let Some(baseline) = next_baseline {
                self.state.known.insert(path.clone(), baseline);
            } else {
                self.state.known.remove(&path);
            }
            outputs.record(&path, &replacement)?;
        }
        Ok(clean)
    }
}

fn recovery_parent(target: &Path, checkout: &Path, git_dir: &Path) -> io::Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| invalid("dotfile target has no parent"))?
        .canonicalize()?;
    let checkout = checkout.canonicalize()?;
    let git_dir = git_dir.canonicalize()?;
    if parent.starts_with(checkout) {
        // Git metadata is never a publishable path. A target on a nested
        // mount cannot use it to recover and will fail before displacement.
        Ok(git_dir)
    } else {
        Ok(parent)
    }
}

pub(super) fn retire(
    git: &Git<'_>,
    git_dir: &Path,
    retired: &[Retirement],
    receipt: &mut Receipt,
) -> io::Result<bool> {
    let mut clean = true;
    for row in retired {
        let mut source_is_present = false;
        for source in &row.sources {
            source_is_present |= validation::source_exists(git.root(), source)?;
        }
        if source_is_present {
            path_issue(
                receipt,
                "Kept a machine-local declaration whose source still exists",
                &row.path,
                COPY_ACTION,
            );
            clean = false;
            continue;
        }
        if install(
            git.root(),
            &row.path,
            git_dir,
            git_dir,
            (&row.before, &row.after),
            None,
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
    !matches!(
        fs::symlink_metadata(directory.join(PENDING)),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

#[derive(Deserialize, Serialize)]
struct WritePlan {
    anchor: PathBuf,
    path: PathBuf,
    stage: PathBuf,
    #[serde(default)]
    effect: Option<WriteEffect>,
}

#[derive(Deserialize, Serialize)]
enum WriteEffect {
    Capture {
        deployed: PathBuf,
        baseline: Captured,
    },
    Deploy {
        target: PathBuf,
        baseline: Option<KnownCopy>,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
pub(super) enum CaptureFailure {
    Journal,
    Device,
}

#[cfg(test)]
thread_local! {
    pub(super) static CAPTURE_FAILURE: std::cell::Cell<Option<CaptureFailure>> = const { std::cell::Cell::new(None) };
}

fn install(
    anchor: &Path,
    path: &Path,
    git_dir: &Path,
    recovery_parent: &Path,
    images: (&Image, &Image),
    effect: Option<WriteEffect>,
    receipt: &mut Receipt,
) -> io::Result<()> {
    #[cfg(test)]
    let fault = matches!(effect, Some(WriteEffect::Capture { .. }))
        .then(|| CAPTURE_FAILURE.take())
        .flatten();
    let plan = prepare_write(
        anchor,
        path,
        git_dir,
        recovery_parent,
        images,
        effect,
        |record, path| {
            #[cfg(test)]
            if fault == Some(CaptureFailure::Journal) {
                return Err(io::Error::from_raw_os_error(nix::libc::ENOSPC));
            }
            record
                .persist_noclobber(path)
                .map(drop)
                .map_err(io::Error::other)
        },
    )?;
    #[cfg(test)]
    if fault == Some(CaptureFailure::Device) {
        return reconcile_write(
            git_dir,
            &plan,
            receipt,
            |root, path, stage, old, new, before, after| {
                super::guard::replace_at_boundary(
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
        );
    }
    complete_write(git_dir, &plan, receipt)
}

fn prepare_write(
    anchor: &Path,
    path: &Path,
    git_dir: &Path,
    recovery_parent: &Path,
    (before, after): (&Image, &Image),
    effect: Option<WriteEffect>,
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
        effect,
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
    if result.is_ok()
        && let Some(effect) = &plan.effect
    {
        let mut copies = Copies::load(git_dir)?;
        match effect {
            WriteEffect::Capture { deployed, baseline } => {
                copies.state.captured.insert(deployed.clone(), *baseline);
            }
            WriteEffect::Deploy { target, baseline } => {
                if let Some(baseline) = baseline {
                    copies.state.known.insert(target.clone(), baseline.clone());
                } else {
                    copies.state.known.remove(target);
                }
                copies.state.changed.insert(target.clone());
            }
        }
        copies.persist(git_dir)?;
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

fn status_at(run: &Run<'_>, root: &Path, preview: bool) -> io::Result<Vec<Target>> {
    let output = validation::probe(
        run,
        root,
        ["bootstrap", "dotfiles", "status", "--json"],
        preview,
    )?;
    serde_json::from_slice::<Status>(&output)
        .map(|status| status.files)
        .map_err(io::Error::other)
}

fn absolute_target(run: &Run<'_>, path: &Path) -> io::Result<PathBuf> {
    let mut parts = path.components();
    if let Some(std::path::Component::Normal(tilde)) = parts.next()
        && tilde == "~"
    {
        let home = run
            .ctx()
            .env
            .home()
            .ok_or_else(|| invalid("HOME is unavailable"))?;
        let result = home.join(parts.as_path());
        fs_relative(&result)?;
        return Ok(result);
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
    create_only: bool,
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
    staged: &'a Path,
    plan: &'a ApplyPlan,
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
        let staged_source = if top {
            self.staged.to_owned()
        } else {
            self.staged.join(relative)
        };
        let target = if top {
            self.destination.to_owned()
        } else {
            self.destination.join(self.rules.destination(relative))
        };
        if !top && self.claimed.contains(&target) {
            return Ok(());
        }
        self.plan.verify_source(&staged_source)?;
        let md = fs::symlink_metadata(&staged_source)?;
        if md.is_dir() {
            for leaf in fs::read_dir(&staged_source)? {
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
                verify_link_source(self.plan, &staged_source, &source)?;
                Image::Link(link_target(&source, &target, self.rules.relative)?)
            } else {
                read_copy(&staged_source, self.target.permissions.as_deref())?
            };
            self.sources.insert(target.clone(), source);
            self.desired.insert(target, image);
        }
        Ok(())
    }
}

fn verify_link_source(plan: &ApplyPlan, staged: &Path, source: &Path) -> io::Result<()> {
    plan.verify_source(staged)?;
    let staged_dir = fs::symlink_metadata(staged)?.is_dir();
    let live_dir = fs::symlink_metadata(source)?.is_dir();
    if staged_dir && live_dir {
        let names = |path: &Path| {
            fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<BTreeSet<_>>>()
        };
        let expected = names(staged)?;
        if expected == names(source)? {
            for name in expected {
                verify_link_source(plan, &staged.join(&name), &source.join(name))?;
            }
            return Ok(());
        }
    } else if !staged_dir && !live_dir && read_copy(staged, None)?.git_eq(&read_copy(source, None)?)
    {
        return Ok(());
    }
    Err(invalid("link source changed after validation"))
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

#[derive(Clone, Copy)]
struct Declaration<'a> {
    name: &'a str,
    item: &'a Item,
    group: Option<(&'a str, &'a Item)>,
}

impl<'a> Declaration<'a> {
    fn variants(self) -> impl Iterator<Item = &'a toml_edit::Value> {
        self.item
            .get("variants")
            .and_then(Item::as_array)
            .into_iter()
            .flatten()
    }
}

fn declarations_in(doc: &DocumentMut) -> Vec<Declaration<'_>> {
    let mut entries = Vec::new();
    if let Some(dotfiles) = doc.get("dotfiles").and_then(Item::as_table_like) {
        entries.extend(dotfiles.iter().map(|(name, item)| Declaration {
            name,
            item,
            group: None,
        }));
    }
    if let Some(groups) = doc.get("dotfile_groups").and_then(Item::as_table_like) {
        for (group_name, group) in groups.iter() {
            if let Some(dotfiles) = group.get("entries").and_then(Item::as_table_like) {
                entries.extend(dotfiles.iter().map(|(name, item)| Declaration {
                    name,
                    item,
                    group: Some((group_name, group)),
                }));
            }
        }
    }
    entries
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

    fn definition(&self, run: &Run<'_>, target: &Target, path: &Path) -> Option<Declaration<'_>> {
        let matches_target = |name: &str| {
            Path::new(name) == target.target
                || absolute_target(run, Path::new(name)).is_ok_and(|resolved| resolved == path)
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
            .find_map(|(config, doc)| {
                let mut declarations = declarations_in(doc);
                let direct = declarations
                    .iter()
                    .copied()
                    .filter(|decl| {
                        matches_target(decl.name)
                            || decl.variants().any(|variant| {
                                variant
                                    .as_inline_table()
                                    .and_then(|variant| variant.get("target"))
                                    .and_then(toml_edit::Value::as_str)
                                    .is_some_and(matches_target)
                            })
                    })
                    .collect::<Vec<_>>();
                if !direct.is_empty() {
                    return match direct.as_slice() {
                        [decl] => Some(*decl),
                        _ => None,
                    };
                }
                let parent = config.parent()?;
                let source = target.effective_source()?;
                // Native status publishes the selected destination and its
                // origin. A unique source binds a rendered variant back to
                // the declaration containing its guards and dependencies.
                declarations.retain(|decl| {
                    decl.variants().any(|variant| {
                        variant
                            .as_inline_table()
                            .is_some_and(|variant| variant.contains_key("target"))
                    }) && decl
                        .item
                        .get("source")
                        .and_then(Item::as_str)
                        .is_some_and(|declared| {
                            let declared = Path::new(declared);
                            let resolved = absolute_target(run, declared)
                                .unwrap_or_else(|_| parent.join(declared));
                            resolved == source
                        })
                });
                match declarations.as_slice() {
                    [decl] => Some(*decl),
                    _ => None,
                }
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
        let definition = self.definition(run, target, path);
        let group = definition
            .and_then(|definition| definition.group)
            .or_else(|| self.group(run, target, path))
            .map(|(_, item)| item);
        let property = |name| {
            definition
                .and_then(|definition| definition.item.get(name))
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
        let create_only = property("create_if_missing")
            .and_then(Item::as_bool)
            .unwrap_or(false);
        Rules {
            exclude,
            dot_prefix,
            manifest,
            relative,
            create_only,
        }
    }

    fn render(
        &self,
        run: &Run<'_>,
        git: &Git<'_>,
        git_dir: &Path,
        target: &Target,
        item: Option<&Item>,
        (plan, approved_input): (&ApplyPlan, Option<&Path>),
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
                    if let Some(permissions) = &target.permissions {
                        table.insert("permissions", value(permissions.as_str()));
                    }
                    if let Some(source) = target.effective_source() {
                        let source = match approved_input {
                            Some(staged) => staged.to_owned(),
                            None => plan.source_path(git.root(), source)?,
                        };
                        plan.verify_source(&source)?;
                        table.insert(
                            "source",
                            value(
                                source
                                    .to_str()
                                    .ok_or_else(|| invalid("invalid template source path"))?,
                            ),
                        );
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
                Dir::open(&plan.root)?.read_path(Path::new(selector))
                && !root.join(selector).exists()
            {
                write_private(&root.join(selector), &image)?;
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
            .env("DOTS_ROOT", &plan.root)
            .env("MISE_ENV", &plan.selectors)
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

fn invoke(
    run: &Run<'_>,
    overlay: &Path,
    root: &Path,
    selectors: &str,
    expression: &str,
) -> io::Result<ExitStatus> {
    if run.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    use std::ffi::OsStr;
    let mut command = validation::command(
        run,
        overlay,
        [
            "exec",
            "--",
            "/bin/sh",
            "-e",
            "-c",
            "cd \"$1\"; exec /bin/sh -e -c \"$2\"",
            "dots-sync-hook",
        ]
        .map(OsStr::new)
        .into_iter()
        .chain([root.as_os_str(), OsStr::new(expression)]),
        true,
    )?;
    command.env("DOTS_ROOT", root).env("MISE_ENV", selectors);
    Ok(run.probe(command)?.status)
}

fn hook_failed(
    receipt: &mut Receipt,
    diagnostic: &'static str,
    action: &'static str,
    result: io::Result<ExitStatus>,
) -> bool {
    use std::os::unix::process::ExitStatusExt;
    let mut item = crate::receipt::Issue::new(diagnostic, action);
    if let Ok(status) = result {
        if status.success() {
            return false;
        }
        if let Some(code) = status.code() {
            item.diagnostic.push_str(&format!(" (exit {code})"));
        } else if let Some(signal) = status.signal() {
            item.diagnostic.push_str(&format!(" (signal {signal})"));
        }
    }
    receipt.issues.push(item);
    true
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
            let root = temp.path().canonicalize().unwrap().join("checkout");
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
                (
                    &Dir::open(&self.root)?.read_path(Path::new("source"))?,
                    &Dir::open(self._temp.path())?.read_path(Path::new("deployed"))?,
                ),
                None,
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

    #[cfg(target_os = "linux")]
    #[test]
    fn deployed_recovery_keeps_the_editor_inode_with_identical_checkout_backup() {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let fixture = Fixture::new();
        let world = fixture._temp.path().canonicalize().unwrap();
        let destination = world.join("deployed");
        let initial = b"initial\n";
        fs::write(fixture.root.join("source"), initial).unwrap();
        fs::write(&destination, initial).unwrap();
        let mut editor = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&destination)
            .unwrap();
        let identity = |metadata: fs::Metadata| (metadata.dev(), metadata.ino());
        let opened = identity(editor.metadata().unwrap());
        let planned = Image::Regular {
            bytes: b"incoming\n".to_vec(),
            mode: 0o644,
        };
        let mut receipt = fixture.receipt();

        for (root, path, storage) in [
            (
                fixture.root.as_path(),
                Path::new("source"),
                fixture.git.as_path(),
            ),
            (world.as_path(), Path::new("deployed"), world.as_path()),
        ] {
            let before = Dir::open(root).unwrap().read_path(path).unwrap();
            install(
                root,
                path,
                &fixture.git,
                storage,
                (&before, &planned),
                None,
                &mut receipt,
            )
            .unwrap();
        }

        let [checkout, deployed] = receipt.recovery_files.as_slice() else {
            panic!("both original inodes should be recorded");
        };
        assert_eq!(fs::read(checkout).unwrap(), initial);
        assert_eq!(fs::read(deployed).unwrap(), initial);
        assert_ne!(identity(fs::metadata(checkout).unwrap()), opened);
        assert_eq!(identity(fs::metadata(deployed).unwrap()), opened);
        assert_eq!(
            fs::metadata(deployed.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        editor.set_len(0).unwrap();
        editor.seek(SeekFrom::Start(0)).unwrap();
        editor.write_all(b"later editor bytes\n").unwrap();
        assert_eq!(fs::read(deployed).unwrap(), b"later editor bytes\n");
        assert_eq!(fs::read(checkout).unwrap(), initial);
        assert_eq!(fs::read(destination).unwrap(), b"incoming\n");
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
    fn checkout_dotfile_recovery_stays_in_git_metadata_across_resume() {
        let fixture = Fixture::new();
        let source = fixture.root.join("source");
        let stage_parent = recovery_parent(&source, &fixture.root, &fixture.git).unwrap();
        let plan = prepare_write(
            Path::new("/"),
            fs_relative(&source).unwrap(),
            &fixture.git,
            &stage_parent,
            (
                &Dir::open(&fixture.root)
                    .unwrap()
                    .read_path(Path::new("source"))
                    .unwrap(),
                &Image::Regular {
                    bytes: b"replacement".to_vec(),
                    mode: 0o644,
                },
            ),
            None,
            |record, path| {
                record
                    .persist_noclobber(path)
                    .map(drop)
                    .map_err(io::Error::other)
            },
        )
        .unwrap();
        fs::rename(&source, plan.stage.join("previous")).unwrap();
        assert!(plan.stage.starts_with(&fixture.git));
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 1);

        let mut receipt = fixture.receipt();
        recover(&fixture.git, &mut receipt).unwrap();
        assert_eq!(fs::read(source).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 2);
        assert_eq!(receipt.recovery_files, [plan.stage.join("previous")]);
        assert_eq!(
            fs::read(&receipt.recovery_files[0]).unwrap(),
            b"committed source"
        );
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
            (&Image::Absent, &planned),
            None,
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
