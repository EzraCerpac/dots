use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{receipt::Receipt, run::Run};

use super::git::{Entry, Git, Tree, invalid, relative};
use super::guard::{CStrName, Dir, Image, private_dir, write_private};
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
    fn matches(&self, expected: &Self, hook_permissions: bool) -> bool {
        self.target == expected.target
            && self.mode == expected.mode
            && self.source == expected.source
            && self.origin == expected.origin
            && (hook_permissions || self.permissions == expected.permissions)
            && self.omitted == expected.omitted
    }

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
        for target in &targets {
            if matches!(target.mode.as_str(), "track" | "track-local") {
                continue;
            }
            let path = absolute_target(run, &target.target)?;
            if let Ok(image) = filesystem.read_path(fs_relative(&path)?) {
                observed.insert(path, image);
            }
        }
        Ok(Self {
            root,
            selectors,
            targets,
            configs,
            observed,
        })
    }

    pub fn sources(&self) -> impl Iterator<Item = &Path> {
        self.targets
            .iter()
            .filter(|target| !matches!(target.mode.as_str(), "track" | "track-local"))
            .filter_map(Target::effective_source)
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

fn hooks_disabled(run: &Run<'_>) -> bool {
    run.ctx()
        .env
        .get("MISE_NO_HOOKS")
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

fn apply_pre_hooks(run: &Run<'_>, git: &Git<'_>, plan: &ApplyPlan, receipt: &mut Receipt) -> bool {
    if hooks_disabled(run) {
        return true;
    }
    for hook in &plan.configs.pre {
        if hook_failed(
            receipt,
            "mise pre-dotfiles hook failed",
            "Check the pre-dotfiles hook and rerun dots sync.",
            invoke(run, git.root(), &plan.selectors, hook),
        ) {
            return false;
        }
    }
    true
}

fn source_for_apply(
    plan: &ApplyPlan,
    live: &Path,
    tree: &Tree,
    source: &Path,
) -> io::Result<PathBuf> {
    if let Ok(relative) = source.strip_prefix(live) {
        let staged = plan.root.join(relative);
        if !tree.contains_key(relative) && !tree.keys().any(|path| path.starts_with(relative)) {
            return Err(invalid("source was not in the approved tree"));
        }
        if !source.canonicalize()?.starts_with(live) {
            return Err(invalid("source escapes the approved checkout"));
        }
        if fs::symlink_metadata(&staged)?.file_type() != fs::symlink_metadata(source)?.file_type() {
            return Err(invalid("source type changed after validation"));
        }
        if fs::symlink_metadata(&staged)?.is_symlink()
            && fs::read_link(staged)? != fs::read_link(source)?
        {
            return Err(invalid("source link changed after validation"));
        }
    }
    Ok(source.to_owned())
}

#[derive(Clone, Deserialize, Serialize)]
struct KnownCopy {
    source: PathBuf,
    entry: Option<Entry>,
    #[serde(default)]
    revision: Option<String>,
    mode: Option<u32>,
    #[serde(default)]
    deployed: Option<[u8; 32]>,
    #[serde(default)]
    native_epoch: Option<NativeEpoch>,
}

impl KnownCopy {
    fn source_image(&self, git: &Git<'_>, revision: &str) -> io::Result<Image> {
        Image::checkout(
            git,
            self.revision.as_deref().unwrap_or(revision),
            &self.source,
            self.entry.as_ref(),
        )
    }

    fn matches(
        &self,
        observed: &Image,
        base: &Image,
        mode: Option<u32>,
        native: Option<&str>,
    ) -> bool {
        if let Some(native) = native {
            if !matches_native_copy(observed, native, mode) {
                return false;
            }
            // Legacy guarded records do not say when mise last deployed.
            // An extant but undated native record may only corroborate them.
            if self.deployed.is_none() || self.native_epoch.is_some() {
                return true;
            }
        }
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
    written: BTreeSet<PathBuf>,
    deployments: BTreeMap<PathBuf, NativeDeployment>,
}

#[derive(Default, Deserialize, Serialize)]
struct CopyState {
    known: BTreeMap<PathBuf, KnownCopy>,
    captured: BTreeMap<PathBuf, Captured>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct Captured {
    mode: u32,
    digest: [u8; 32],
    #[serde(default)]
    native_epoch: Option<NativeEpoch>,
}

impl Captured {
    fn is_current(self, native: Option<&NativeDeployment>) -> bool {
        !native.is_some_and(|record| record.supersedes(self.native_epoch))
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
enum NativeEpoch {
    Absent,
    Present(DeploymentEpoch),
}

#[derive(Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
struct DeploymentEpoch {
    device: u64,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<fs::Metadata> for DeploymentEpoch {
    fn from(metadata: fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

struct NativeDeployment {
    digest: String,
    epoch: DeploymentEpoch,
}

impl NativeDeployment {
    fn supersedes(&self, earlier: Option<NativeEpoch>) -> bool {
        earlier.is_some_and(|epoch| epoch != NativeEpoch::Present(self.epoch))
    }
}

const STATE: &str = "dots-sync-copies.json";
const COPY_ACTION: &str = "Review the source and deployed copy, then rerun dots sync.";

fn copy_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn native_deployments(run: &Run<'_>) -> io::Result<BTreeMap<PathBuf, NativeDeployment>> {
    let env = &run.ctx().env;
    let state = env
        .get("MISE_STATE_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("XDG_STATE_HOME")
                .filter(|path| !path.is_empty())
                .map(|path| Path::new(path).join("mise"))
        })
        .or_else(|| env.home().map(|home| home.join(".local/state/mise")));
    let Some(state) = state else {
        return Ok(BTreeMap::new());
    };
    let directory = state.join("dotfiles/targets");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let dir = Dir::open(&directory)?;
    let mut deployments = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        if entry.path().extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let (parent, name) = dir.parent(Path::new(&entry.file_name()), false)?;
        let mut file = match parent.open_file(&name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) if error.raw_os_error() == Some(nix::libc::ELOOP) => continue,
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let epoch = DeploymentEpoch::from(metadata);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if DeploymentEpoch::from(file.metadata()?) != epoch {
            return Err(invalid("native deployment record changed while reading"));
        }
        let Ok(doc) = std::str::from_utf8(&bytes)
            .unwrap_or_default()
            .parse::<DocumentMut>()
        else {
            continue;
        };
        if doc.get("version").and_then(Item::as_integer) == Some(1)
            && let (Some(target), Some(digest)) = (
                doc.get("target").and_then(Item::as_str),
                doc.get("content_digest").and_then(Item::as_str),
            )
            && digest.len() == 64
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            deployments.insert(
                PathBuf::from(target),
                NativeDeployment {
                    digest: digest.to_ascii_lowercase(),
                    epoch,
                },
            );
        }
    }
    Ok(deployments)
}

impl Copies {
    fn native_epoch(&self, target: &Path) -> NativeEpoch {
        self.deployments
            .get(target)
            .map_or(NativeEpoch::Absent, |native| {
                NativeEpoch::Present(native.epoch)
            })
    }

    fn native_baseline(&self, target: &Path, known: &KnownCopy) -> Option<&str> {
        // A native record predating a guarded sync write is stale. A later
        // native deployment takes precedence even if it wrote older bytes.
        self.deployments
            .get(target)
            .filter(|native| {
                known.deployed.is_none()
                    || known.native_epoch.is_none()
                    || native.supersedes(known.native_epoch)
            })
            .map(|native| native.digest.as_str())
    }

    fn pending_capture(&self, target: &Path) -> bool {
        self.state
            .captured
            .get(target)
            .is_some_and(|capture| capture.is_current(self.deployments.get(target)))
    }

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
        self.deployments = native_deployments(run)?;
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
            let Some(source) = target.effective_source() else {
                continue;
            };
            let Ok(source) = source.strip_prefix(git.root()) else {
                // A native deployment or identical source is the common
                // version for a copy fed by an earlier target.
                if let Some(Image::Regular { bytes, mode }) = self.observed.get(&destination) {
                    let digest: [u8; 32] = Sha256::digest(bytes).into();
                    let matches = read_copy(source, target.permissions.as_deref())
                        .is_ok_and(|image| image == self.observed[&destination])
                        || self
                            .deployments
                            .get(&destination)
                            .is_some_and(|native| native.digest == copy_digest(bytes));
                    if matches {
                        let native_epoch = Some(self.native_epoch(&destination));
                        self.state.known.entry(destination).or_insert(KnownCopy {
                            source: source.to_owned(),
                            entry: None,
                            revision: None,
                            mode: Some(*mode),
                            deployed: Some(digest),
                            native_epoch,
                        });
                    }
                }
                continue;
            };
            let native = if git.root().join(source).is_dir() {
                Some(stage_tree(run, git_dir, &git.root().join(source), &rules)?)
            } else {
                None
            };
            let revisions = base.iter().filter(|(path, _)| path.starts_with(source));
            for (leaf, entry) in revisions {
                let relative = leaf.strip_prefix(source).expect("prefix was selected");
                let relative = rules.destination(relative);
                if native
                    .as_ref()
                    .is_some_and(|leaves| !leaves.contains_key(&relative))
                {
                    continue;
                }
                let path = destination.join(&relative);
                let record = self
                    .state
                    .known
                    .entry(path.clone())
                    .or_insert_with(|| KnownCopy {
                        source: leaf.clone(),
                        entry: Some(entry.clone()),
                        revision: Some(revision.to_owned()),
                        mode: None,
                        deployed: None,
                        native_epoch: None,
                    })
                    .clone();
                // A different declaration must prove a clean takeover first.
                if record.source != *leaf || !matches!(entry.mode.as_str(), "100644" | "100755") {
                    continue;
                }
                if run.cancelled() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let common = record.source_image(git, revision)?;
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
                let native_digest = if path == destination {
                    self.native_baseline(&path, &record)
                } else {
                    None
                };
                let expected_mode = match (&live, native_digest) {
                    (Image::Regular { mode, .. }, Some(_)) => {
                        Some(copy_mode(*mode, target.permissions.as_deref())?)
                    }
                    _ => record
                        .mode
                        .or_else(|| match native.as_ref()?.get(&relative)? {
                            Image::Regular { mode, .. } => Some(*mode),
                            _ => None,
                        }),
                };
                if record.matches(&deployed, &common, expected_mode, native_digest)
                    || live == deployed
                {
                    continue;
                }
                if matches!(&deployed, Image::Regular { mode, .. } if
                    expected_mode.is_some_and(|expected| expected != *mode))
                {
                    path_issue(
                        receipt,
                        "Kept a locally changed copy mode",
                        &path,
                        COPY_ACTION,
                    );
                    continue;
                }
                let source_unchanged = match native_digest {
                    Some(digest) => {
                        matches_native_copy(&live, digest, None)
                            && matches!((&live, &common),
                                (Image::Regular { mode: live, .. }, Image::Regular { mode: base, .. })
                                    if (live ^ base) & 0o100 == 0)
                    }
                    None => live.git_eq(&common),
                };
                if !source_unchanged {
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
                            native_epoch: Some(self.native_epoch(&path)),
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
        revision: &str,
        tree: &Tree,
    ) -> io::Result<()> {
        if self.state.captured.is_empty() {
            return Ok(());
        }
        for (path, observed) in &self.state.captured {
            if !observed.is_current(self.deployments.get(path)) {
                continue;
            }
            let Some(known) = self.state.known.get_mut(path) else {
                continue;
            };
            let Some(entry) = tree.get(&known.source) else {
                continue;
            };
            let shared = Image::checkout(git, revision, &known.source, Some(entry))?;
            if let Image::Regular { bytes, .. } = &shared
                && <[u8; 32]>::from(Sha256::digest(bytes)) == observed.digest
            {
                known.entry = Some(entry.clone());
                known.revision = Some(revision.to_owned());
                known.mode = Some(observed.mode);
                known.deployed = Some(observed.digest);
                known.native_epoch = observed.native_epoch;
            }
        }
        self.persist(git_dir)
    }

    pub fn reconcile_captures(
        &mut self,
        git: &Git<'_>,
        git_dir: &Path,
        approved: &str,
        merged: &str,
        tree: &Tree,
        receipt: &mut Receipt,
    ) -> io::Result<()> {
        if self.state.captured.is_empty() {
            return Ok(());
        }
        // A saved capture is still a local edit until the adopted checkout
        // contains it. Keep the protection if a published merge replaced it.
        for (path, capture) in std::mem::take(&mut self.state.captured) {
            if !capture.is_current(self.deployments.get(&path)) {
                continue;
            }
            let Some(known) = self.state.known.get(&path) else {
                continue;
            };
            let adopted = Image::checkout(git, merged, &known.source, tree.get(&known.source))?;
            if matches!(&adopted, Image::Regular { bytes, .. }
                if <[u8; 32]>::from(Sha256::digest(bytes)) == capture.digest)
            {
                continue;
            }
            if merged != approved && known.revision.as_deref() == Some(approved) {
                path_issue(
                    receipt,
                    "Conflict with captured copy source",
                    &known.source,
                    "Recover your version from the local merge parent, reconcile both versions, then rerun dots sync.",
                );
            }
            self.state.captured.insert(path, capture);
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
        if validated.apply.targets.is_empty() {
            return Ok(());
        }
        let filesystem = Dir::open(Path::new("/"))?;
        let sealed_sources = validated
            .apply
            .sources()
            .filter_map(|source| {
                let relative = source.strip_prefix(git.root()).ok()?;
                Some((source.to_owned(), relative))
            })
            .filter_map(|(source, relative)| {
                let live = Dir::open(git.root()).ok()?;
                let preview = Dir::open(&validated.apply.root).ok()?;
                let clean = if tree.contains_key(relative) {
                    live.read_path(relative)
                        .ok()?
                        .git_eq(&preview.read_path(relative).ok()?)
                } else {
                    tree.keys()
                        .filter(|path| path.starts_with(relative))
                        .all(|path| {
                            live.read_path(path)
                                .ok()
                                .zip(preview.read_path(path).ok())
                                .is_some_and(|(live, approved)| live.git_eq(&approved))
                        })
                };
                (!clean).then_some(source)
            })
            .collect::<BTreeSet<_>>();
        if !apply_pre_hooks(run, git, &validated.apply, receipt) {
            return Ok(());
        }
        let hook_permissions = !hooks_disabled(run) && !validated.apply.configs.pre.is_empty();
        let active = status_at(run, git.root(), false)?;
        let active_configs = Configs::read_at(run, git.root(), false)?;

        let ApplyPlan {
            selectors,
            targets,
            configs,
            ..
        } = &validated.apply;
        let mut jobs = BTreeMap::new();
        let mut results = BTreeMap::new();
        let mut planned_paths = BTreeSet::new();
        let mut group_members = BTreeMap::<String, Vec<PathBuf>>::new();
        for target in targets {
            if matches!(target.mode.as_str(), "track" | "track-local") {
                continue;
            }
            let path = absolute_target(run, &target.target)?;
            planned_paths.insert(path.clone());
            let declaration = configs.definition(run, target, &path);
            let group = declaration
                .and_then(|decl| decl.group)
                .or_else(|| configs.group(run, target, &path));
            for name in [
                group.map(|(name, _)| name),
                declaration.map(|decl| decl.name),
            ]
            .into_iter()
            .flatten()
            {
                group_members
                    .entry(name.to_owned())
                    .or_default()
                    .push(path.clone());
            }
            let Some(selected) = active
                .iter()
                .find(|selected| selected.matches(target, hook_permissions))
            else {
                path_issue(
                    receipt,
                    "Kept a dotfile whose validated selector changed",
                    &path,
                    COPY_ACTION,
                );
                results.insert(path, false);
                continue;
            };
            let definition = declaration.map(|decl| decl.item.clone());
            if configs.declaration_signature(run, target, &path, hook_permissions)
                != active_configs.declaration_signature(run, selected, &path, hook_permissions)
            {
                path_issue(
                    receipt,
                    "Kept a dotfile whose validated declaration changed",
                    &path,
                    COPY_ACTION,
                );
                results.insert(path, false);
                continue;
            }
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
            jobs.insert(path, (selected, definition, depends));
        }

        let mut observed = validated.apply.observed.clone();
        observed.append(&mut self.observed);
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
        let reload = ReloadQueue::new(run, git.root(), git_dir, selectors, &configs.reload)?;
        let directory_copies = targets
            .iter()
            .filter(|target| target.mode == "copy")
            .filter(|target| target.effective_source().is_some_and(Path::is_dir))
            .filter_map(|target| absolute_target(run, &target.target).ok())
            .collect::<Vec<_>>();
        while !jobs.is_empty() {
            if run.cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let next = jobs
                .iter()
                .find(|(path, (_, _, deps))| {
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
            let (target, definition, depends) = jobs.remove(&path).expect("job was present");
            let waiting_for = dependencies(&depends, &path);
            if waiting_for
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
            let former_directory_leaf = directory_copies.iter().any(|directory| {
                path.starts_with(directory)
                    && directory != &path
                    && self.state.known.keys().any(|leaf| {
                        leaf.starts_with(directory)
                            && (leaf.starts_with(&path) || path.starts_with(leaf))
                    })
            });
            let changed_now = if target.mode == "copy" || former_directory_leaf {
                self.apply_guarded(
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
                    &planned_paths,
                    &sealed_sources,
                    former_directory_leaf,
                )
            } else {
                self.apply_native(
                    run,
                    git,
                    validated,
                    tree,
                    &path,
                    target,
                    &filesystem,
                    &observed,
                    &sealed_sources,
                    reload.as_ref(),
                    receipt,
                )
            };
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

        if let Some(reload) = reload
            && reload
                .finish(
                    run,
                    git.root(),
                    selectors,
                    &configs.reload,
                    &self.written,
                    receipt,
                )
                .is_err()
        {
            issue(
                receipt,
                "Could not match mise dotfile reloads",
                "Check history.reload and run the matching commands.",
            );
        }
        if !hooks_disabled(run) {
            for hook in &configs.post {
                validated.check_overlay_locals()?;
                if hook_failed(
                    receipt,
                    "mise post-dotfiles hook failed",
                    "Check the post-dotfiles hook and rerun dots sync.",
                    invoke(run, git.root(), selectors, hook),
                ) {
                    break;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_native(
        &mut self,
        run: &Run<'_>,
        git: &Git<'_>,
        validated: &Validated,
        tree: &Tree,
        path: &Path,
        target: &Target,
        filesystem: &Dir,
        observed: &BTreeMap<PathBuf, Image>,
        sealed_sources: &BTreeSet<PathBuf>,
        reload: Option<&ReloadQueue>,
        receipt: &mut Receipt,
    ) -> io::Result<bool> {
        if let Some(source) = target.effective_source() {
            if sealed_sources.contains(source) {
                path_issue(
                    receipt,
                    "Kept a dotfile with an omitted live source",
                    path,
                    COPY_ACTION,
                );
                return Ok(false);
            }
            source_for_apply(&validated.apply, git.root(), tree, source)?;
        }
        let current = filesystem.read_path(fs_relative(path)?)?;
        let previous_copy = self.state.known.get(path);
        let absent = target.mode == "absent";
        let copy_is_edited = match previous_copy {
            Some(copy) if current != Image::Absent => {
                let common = copy.source_image(git, validated.revision())?;
                let native = self.native_baseline(path, copy);
                !copy.matches(&current, &common, copy.mode, native)
            }
            None if absent && current != Image::Absent => self
                .deployments
                .get(path)
                .is_some_and(|native| !matches_native_copy(&current, &native.digest, None)),
            _ => false,
        };
        let copy_is_edited =
            copy_is_edited || (current != Image::Absent && self.pending_capture(path));
        if absent && copy_is_edited {
            path_issue(
                receipt,
                "Kept an edited former copy",
                path,
                "Move the deployed file somewhere safe, then rerun dots sync.",
            );
            return Ok(false);
        }
        if observed
            .get(path)
            .map_or(current != Image::Absent, |before| *before != current)
            || copy_is_edited
        {
            path_issue(
                receipt,
                "Kept a changed or unmanaged dotfile",
                path,
                COPY_ACTION,
            );
            return Ok(false);
        }
        validated.check_overlay_locals()?;
        let mut command = validation::command(
            run,
            git.root(),
            [
                "bootstrap",
                "dotfiles",
                "apply",
                "--yes",
                "--",
                utf8(&target.target)?,
            ],
            false,
        )?;
        command
            .env("DOTS_ROOT", git.root())
            .env("MISE_NO_HOOKS", "1")
            .env(
                "MISE_ENV",
                reload.map_or(validated.apply.selectors.as_str(), |queue| {
                    queue.selectors.as_str()
                }),
            );
        if hook_failed(
            receipt,
            "mise dotfile apply failed",
            COPY_ACTION,
            run.probe(command).map(|output| output.status),
        ) {
            return Ok(false);
        }
        if previous_copy.is_some() {
            self.state.known.remove(path);
        }
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_guarded(
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
        planned_paths: &BTreeSet<PathBuf>,
        sealed_sources: &BTreeSet<PathBuf>,
        former_directory_leaf: bool,
    ) -> io::Result<bool> {
        let configs = &validated.apply.configs;
        let rules = configs.rules(run, target, destination);
        if rules.create_only && filesystem.read_path(fs_relative(destination)?)? != Image::Absent {
            return Ok(true);
        }
        let source = target
            .effective_source()
            .map(|source| {
                if sealed_sources.contains(source) {
                    if matches!(target.mode.as_str(), "symlink" | "symlink-each") {
                        return Err(invalid("link source changed after validation"));
                    }
                    Ok(validated
                        .apply
                        .root
                        .join(source.strip_prefix(git.root()).map_err(io::Error::other)?))
                } else {
                    source_for_apply(&validated.apply, git.root(), tree, source)
                }
            })
            .transpose()?;
        if former_directory_leaf {
            if filesystem.read_path(fs_relative(destination)?)? != Image::Absent {
                path_issue(
                    receipt,
                    "Kept a former directory copy",
                    destination,
                    COPY_ACTION,
                );
                return Ok(false);
            }
            if target.mode == "absent" {
                return Ok(true);
            }
            if target.mode != "template"
                || definition
                    .and_then(|item| item.get("remove_empty"))
                    .and_then(Item::as_bool)
                    == Some(true)
            {
                path_issue(
                    receipt,
                    "Kept a former directory copy whose takeover needs manual review",
                    destination,
                    COPY_ACTION,
                );
                return Ok(false);
            }
        }
        let mut desired = BTreeMap::new();
        match target.mode.as_str() {
            "copy" if source.as_ref().is_some_and(|source| source.is_dir()) => {
                let source = source.as_deref().expect("directory source");
                let approved = source
                    .strip_prefix(git.root())
                    .ok()
                    .map(|relative| {
                        stage_tree(run, git_dir, &validated.apply.root.join(relative), &rules)
                    })
                    .transpose()?;
                for (relative, image) in stage_tree(run, git_dir, source, &rules)? {
                    if approved
                        .as_ref()
                        .is_some_and(|leaves| !leaves.contains_key(&relative))
                    {
                        continue;
                    }
                    let path = destination.join(relative);
                    if !planned_paths.contains(&path) {
                        desired.insert(path, image);
                    }
                }
            }
            "copy" => {
                let source = source
                    .as_deref()
                    .ok_or_else(|| invalid("copy has no source"))?;
                desired.insert(
                    destination.to_owned(),
                    read_copy(source, target.permissions.as_deref())?,
                );
            }
            "template" if former_directory_leaf => {
                desired.insert(
                    destination.to_owned(),
                    stage_handoff(
                        run,
                        git_dir,
                        &validated.apply,
                        git.root(),
                        target,
                        definition,
                        source.as_deref(),
                    )?,
                );
            }
            _ => return Err(invalid("unrecognized mise dotfile mode")),
        }

        let mut clean = true;
        for (path, replacement) in desired {
            let relative_path = fs_relative(&path)?;
            let current = filesystem.read_path(relative_path)?;
            if rules.create_only && current != Image::Absent {
                continue;
            }
            let known = self.state.known.get(&path);
            let baseline = known
                .map(|known| known.source_image(git, validated.revision()))
                .transpose()?;
            if let (
                Image::Regular {
                    mode: expected_mode,
                    ..
                },
                Some(known),
                Image::Regular {
                    mode: current_mode, ..
                },
            ) = (&replacement, known, &current)
                && known.mode.is_none()
                && path != destination
                && expected_mode != current_mode
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
            let next_baseline = if let (Some(source), Image::Regular { bytes, mode }) = (
                target.effective_source().filter(|_| target.mode == "copy"),
                &replacement,
            ) && let Some((source, entry)) =
                copy_entry(git, tree, source, destination, &path, &rules)?
            {
                Some(KnownCopy {
                    source,
                    revision: entry.as_ref().map(|_| validated.revision().to_owned()),
                    entry,
                    mode: Some(*mode),
                    deployed: Some(Sha256::digest(bytes).into()),
                    native_epoch: Some(self.native_epoch(&path)),
                })
            } else {
                None
            };
            if current != replacement {
                validated.check_overlay_locals()?;
                let still_observed = observed
                    .get(&path)
                    .map_or(current == Image::Absent, |before| *before == current);
                let manageable = match (&current, known, &baseline) {
                    (Image::Absent, _, _) => true,
                    (_, Some(known), Some(base)) => {
                        let next = next_baseline
                            .as_ref()
                            .filter(|next| path == destination && next.source == known.source);
                        let native = if path == destination {
                            self.native_baseline(&path, known)
                        } else {
                            None
                        };
                        let mode = if native.is_some() && known.native_epoch.is_some() {
                            next.and_then(|next| next.mode).or(known.mode)
                        } else {
                            known.mode
                        };
                        known.matches(&current, base, mode, native)
                    }
                    (Image::Regular { .. }, None, _) => self
                        .deployments
                        .get(&path)
                        .map_or(target.mode == "template", |native| {
                            matches_native_copy(&current, &native.digest, None)
                        }),
                    _ => false,
                };
                if !still_observed
                    || !manageable
                    || (current != Image::Absent && self.pending_capture(&path))
                {
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
                self.written.insert(path.clone());
            } else if let Some(baseline) = next_baseline {
                if self.pending_capture(&path)
                    && same_copy(
                        &current,
                        &baseline.source_image(git, validated.revision())?,
                        baseline.mode,
                    )
                {
                    self.state.captured.remove(&path);
                }
                self.state.known.insert(path.clone(), baseline);
            } else {
                self.state.known.remove(&path);
            }
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
    if *after != Image::Absent {
        fs::hard_link(
            private.path().join("replacement"),
            private.path().join("publish"),
        )?;
    }
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
    reconcile_write(git_dir, plan, receipt, super::guard::replace_consuming)
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
    // Consuming the publication name proves the install even if the live
    // inode has since been replaced by an editor.
    let result =
        if after != Image::Absent && stage.read_path(Path::new("publish"))? == Image::Absent {
            Ok(())
        } else {
            write(
                &root,
                &plan.path,
                &stage,
                &CStrName::new("previous")?,
                &CStrName::new("publish")?,
                &before,
                &after,
            )
        };
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
                copies.state.captured.remove(target);
                if let Some(baseline) = baseline {
                    copies.state.known.insert(target.clone(), baseline.clone());
                } else {
                    copies.state.known.remove(target);
                }
            }
        }
        copies.persist(git_dir)?;
    }
    // Removing the control record before cleaning staging makes either a
    // retry or completed displacement recoverable across interruption.
    fs::remove_file(git_dir.join(PENDING))?;
    File::open(git_dir)?.sync_all()?;
    if fs::symlink_metadata(previous).is_ok() {
        for leaf in ["expected", "replacement", "publish"] {
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

fn matches_native_copy(current: &Image, digest: &str, mode: Option<u32>) -> bool {
    matches!(current, Image::Regular { bytes, mode: actual }
        if mode.is_none_or(|mode| mode == *actual) && copy_digest(bytes) == digest)
}

fn status_at(run: &Run<'_>, root: &Path, preview: bool) -> io::Result<Vec<Target>> {
    let output = validation::probe(run, root, validation::MiseProbe::DotfileStatus, preview)?;
    let mut status: Status = serde_json::from_slice(&output).map_err(io::Error::other)?;
    if let Some(home) = run.ctx().env.home() {
        for row in &mut status.files {
            // Mise abbreviates the top-level source for display. Expand it
            // lexically so the preview prefix can be remapped before comparison.
            if let Some(source) = &mut row.source
                && let Ok(relative) = source.strip_prefix("~")
            {
                *source = home.join(relative);
            }
        }
    }
    Ok(status.files)
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

struct Rules {
    exclude: Vec<String>,
    dot_prefix: bool,
    manifest: bool,
    permissions: Option<String>,
    create_only: bool,
}

impl Rules {
    fn destination(&self, path: &Path) -> PathBuf {
        if !self.dot_prefix {
            return path.to_owned();
        }
        path.components()
            .map(|part| {
                let name = part.as_os_str();
                let bytes = name.as_encoded_bytes();
                if let Some(suffix) = bytes.strip_prefix(b"dot-") {
                    use std::os::unix::ffi::OsStringExt;
                    return std::ffi::OsString::from_vec([b".".as_slice(), suffix].concat());
                }
                name.to_owned()
            })
            .collect()
    }
}

fn copy_entry(
    git: &Git<'_>,
    tree: &Tree,
    source: &Path,
    destination: &Path,
    target: &Path,
    rules: &Rules,
) -> io::Result<Option<(PathBuf, Option<Entry>)>> {
    if let Ok(relative) = source.strip_prefix(git.root()) {
        let leaf = target.strip_prefix(destination).map_err(io::Error::other)?;
        Ok(tree
            .iter()
            .find(|(path, _)| {
                path.strip_prefix(relative)
                    .is_ok_and(|suffix| rules.destination(suffix) == leaf)
            })
            .map(|(path, entry)| (path.clone(), Some(entry.clone()))))
    } else {
        Ok(Some((source.to_owned(), None)))
    }
}

fn staged_files(
    root: &Path,
    relative: &Path,
    files: &mut BTreeMap<PathBuf, Image>,
) -> io::Result<()> {
    let path = root.join(relative);
    if fs::symlink_metadata(&path)?.is_dir() {
        for entry in fs::read_dir(path)? {
            staged_files(root, &relative.join(entry?.file_name()), files)?;
        }
    } else {
        files.insert(relative.to_owned(), Dir::open(root)?.read_path(relative)?);
    }
    Ok(())
}

fn stage_tree(
    run: &Run<'_>,
    git_dir: &Path,
    source: &Path,
    rules: &Rules,
) -> io::Result<BTreeMap<PathBuf, Image>> {
    let stage = private_dir(git_dir, "copy-directory-")?;
    let output = stage.path().join("output");
    let mut definition = Table::new();
    definition.insert("mode", value("copy"));
    definition.insert("source", value(utf8(source)?));
    if rules.dot_prefix {
        definition.insert("dot_prefix", value(true));
    }
    if rules.manifest {
        definition.insert("manifest", value("git"));
    }
    if let Some(permissions) = &rules.permissions {
        definition.insert("permissions", value(permissions.as_str()));
    }
    if !rules.exclude.is_empty() {
        definition.insert(
            "exclude",
            value(
                rules
                    .exclude
                    .iter()
                    .map(String::as_str)
                    .collect::<toml_edit::Array>(),
            ),
        );
    }
    let config = dotfile_config(&output, Item::Table(definition))?;
    isolated_apply(run, stage.path(), &config)?;
    let mut files = BTreeMap::new();
    if output.exists() {
        staged_files(&output, Path::new(""), &mut files)?;
    }
    Ok(files)
}

fn utf8(path: &Path) -> io::Result<&str> {
    path.to_str().ok_or_else(|| invalid("invalid mise path"))
}

fn dotfile_config(path: &Path, definition: Item) -> io::Result<DocumentMut> {
    let mut config = DocumentMut::new();
    let mut dotfiles = Table::new();
    dotfiles.insert(utf8(path)?, definition);
    config.insert("dotfiles", Item::Table(dotfiles));
    Ok(config)
}

fn isolated_apply(run: &Run<'_>, root: &Path, config: &DocumentMut) -> io::Result<()> {
    write_private(
        &root.join("config.toml"),
        &Image::Regular {
            bytes: config.to_string().into_bytes(),
            mode: 0o600,
        },
    )?;
    let command = validation::isolated_apply(run, root)?;
    if !run.probe(command)?.status.success() {
        return Err(invalid("mise could not materialize guarded output"));
    }
    Ok(())
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
    let mode = copy_mode(metadata.permissions().mode(), permissions)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(Image::Regular { bytes, mode })
}

fn copy_mode(source_mode: u32, permissions: Option<&str>) -> io::Result<u32> {
    let mode = match permissions {
        Some(value) => u32::from_str_radix(value, 8).map_err(io::Error::other)?,
        None => source_mode,
    };
    Ok(mode & 0o777)
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

fn omit_permissions(item: &mut Item) {
    if let Some(table) = item.as_table_like_mut() {
        table.remove("permissions");
        if let Some(variants) = table.get_mut("variants").and_then(Item::as_array_mut) {
            for variant in variants
                .iter_mut()
                .filter_map(toml_edit::Value::as_inline_table_mut)
            {
                variant.remove("permissions");
            }
        }
    }
}

impl Configs {
    fn read_at(run: &Run<'_>, root: &Path, preview: bool) -> io::Result<Self> {
        #[derive(Deserialize)]
        struct Config {
            path: PathBuf,
        }
        let bytes = validation::probe(run, root, validation::MiseProbe::ConfigList, preview)?;
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

    fn declaration_signature(
        &self,
        run: &Run<'_>,
        target: &Target,
        path: &Path,
        hook_permissions: bool,
    ) -> (Option<String>, Option<String>) {
        let declaration = self.definition(run, target, path);
        let group = declaration
            .and_then(|declaration| declaration.group)
            .or_else(|| self.group(run, target, path));
        let signature = |item: &Item, group: bool| {
            if !hook_permissions {
                return item.to_string();
            }
            let mut item = item.clone();
            omit_permissions(&mut item);
            if group
                && let Some(entries) = item.get_mut("entries").and_then(Item::as_table_like_mut)
            {
                for (_, entry) in entries.iter_mut() {
                    omit_permissions(entry);
                }
            }
            item.to_string()
        };
        (
            declaration.map(|declaration| signature(declaration.item, false)),
            group.map(|(_, item)| signature(item, true)),
        )
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
        let create_only = property("create_if_missing")
            .and_then(Item::as_bool)
            .unwrap_or(false);
        Rules {
            exclude,
            dot_prefix,
            manifest,
            permissions: target.permissions.clone(),
            create_only,
        }
    }
}

fn stage_handoff(
    run: &Run<'_>,
    git_dir: &Path,
    plan: &ApplyPlan,
    live: &Path,
    target: &Target,
    item: Option<&Item>,
    source: Option<&Path>,
) -> io::Result<Image> {
    let stage = private_dir(git_dir, "copy-handoff-")?;
    let output = stage.path().join("output");
    let mut definition = Table::new();
    if let Some(source) = source {
        definition.insert("mode", value("template"));
        definition.insert("source", value(utf8(source)?));
    } else if let Some(content) = item
        .and_then(|item| item.get("content"))
        .and_then(Item::as_str)
    {
        definition.insert("content", value(content));
    } else {
        return Err(invalid("mise did not identify a template or inline source"));
    }
    if let Some(permissions) = &target.permissions {
        definition.insert("permissions", value(permissions.as_str()));
    }
    let mut config = reload_config(
        &plan.configs.reload,
        &stage.path().join("ignored-reloads"),
        |pattern| Ok(pattern.to_owned()),
    )?;
    let dotfiles = dotfile_config(&output, Item::Table(definition))?;
    config.insert("dotfiles", dotfiles["dotfiles"].clone());
    let profile = stage
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("invalid mise profile"))?;
    let profile_path = plan.root.join(format!("config.{profile}.override.toml"));
    write_private(
        &profile_path,
        &Image::Regular {
            bytes: config.to_string().into_bytes(),
            mode: 0o600,
        },
    )?;
    let mut command = validation::command(
        run,
        &plan.root,
        [
            "bootstrap",
            "dotfiles",
            "apply",
            "--yes",
            "--",
            utf8(&output)?,
        ],
        true,
    )?;
    command
        .env("DOTS_ROOT", live)
        .env("MISE_ENV", format!("{},{profile}", plan.selectors))
        .env("MISE_STATE_DIR", stage.path().join("state"))
        .env("MISE_HISTORY_ENABLED", "0");
    let result = run.probe(command);
    fs::remove_file(profile_path)?;
    if !result?.status.success() {
        return Err(invalid("mise could not stage a dotfile"));
    }
    let output = Dir::open(stage.path())?.read_path(Path::new("output"))?;
    if !matches!(output, Image::Regular { .. }) {
        return Err(invalid("mise did not stage a regular dotfile"));
    }
    Ok(output)
}

struct ReloadQueue {
    _override: tempfile::NamedTempFile,
    stage: tempfile::TempDir,
    selectors: String,
}

impl ReloadQueue {
    fn new(
        run: &Run<'_>,
        live: &Path,
        git_dir: &Path,
        selectors: &str,
        rules: &[(String, String)],
    ) -> io::Result<Option<Self>> {
        if rules.is_empty() {
            return Ok(None);
        }
        let stage = private_dir(git_dir, "copy-reload-")?;
        let mut config = tempfile::Builder::new()
            .prefix("config.host-")
            .suffix(".override.toml")
            .tempfile_in(live)?;
        let profile = config
            .path()
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .and_then(|name| name.strip_prefix("config."))
            .and_then(|name| name.strip_suffix(".override.toml"))
            .ok_or_else(|| invalid("invalid mise reload profile"))?;
        let selectors = if selectors.is_empty() {
            profile.to_owned()
        } else {
            format!("{selectors},{profile}")
        };
        let doc = reload_config(rules, &stage.path().join("matches"), |pattern| {
            Ok(pattern.to_owned())
        })?;
        config.write_all(doc.to_string().as_bytes())?;
        let mut command = validation::command(run, live, ["config", "ls", "--json"], false)?;
        command
            .env("MISE_ENV", &selectors)
            .env("MISE_NO_HOOKS", "1");
        let output = run.probe(command)?;
        #[derive(Deserialize)]
        struct LoadedConfig {
            path: PathBuf,
        }
        if !output.status.success()
            || !serde_json::from_slice::<Vec<LoadedConfig>>(&output.stdout)
                .map_err(io::Error::other)?
                .iter()
                .any(|loaded| loaded.path == config.path())
        {
            return Err(invalid("mise did not load the temporary reload profile"));
        }
        Ok(Some(Self {
            _override: config,
            stage,
            selectors,
        }))
    }

    fn finish(
        self,
        run: &Run<'_>,
        live: &Path,
        selectors: &str,
        rules: &[(String, String)],
        paths: &BTreeSet<PathBuf>,
        receipt: &mut Receipt,
    ) -> io::Result<()> {
        let marker = self.stage.path().join("matches");
        if !paths.is_empty() {
            let root = self.stage.path().join("paths");
            let mut config = reload_config(rules, &marker, |pattern| {
                let pattern = absolute_target(run, Path::new(pattern))?;
                Ok(utf8(&root.join(fs_relative(&pattern)?))?.to_owned())
            })?;
            let mut files = Table::new();
            for path in paths {
                let mut definition = Table::new();
                definition.insert("content", value(""));
                files.insert(
                    utf8(&root.join(fs_relative(path)?))?,
                    Item::Table(definition),
                );
            }
            config.insert("dotfiles", Item::Table(files));
            isolated_apply(run, self.stage.path(), &config)?;
        }
        let ids = match fs::read_to_string(marker) {
            Ok(ids) => ids,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let ids = ids
            .lines()
            .filter_map(|id| id.parse::<usize>().ok())
            .collect::<BTreeSet<_>>();
        let mut ran = BTreeSet::new();
        for (id, (_, command)) in rules.iter().enumerate() {
            if ids.contains(&id) && ran.insert(command) {
                hook_failed(
                    receipt,
                    "mise dotfile reload failed",
                    "Check the matching history.reload command, then rerun dots sync.",
                    invoke(run, live, selectors, command),
                );
            }
        }
        Ok(())
    }
}

fn reload_config(
    rules: &[(String, String)],
    marker: &Path,
    pattern_key: impl Fn(&str) -> io::Result<String>,
) -> io::Result<DocumentMut> {
    let mut reload = Table::new();
    let marker = utf8(marker)?.replace(char::from(39), "'\\''");
    for (id, (pattern, _)) in rules.iter().enumerate() {
        let key = pattern_key(pattern)?;
        let mut command = reload
            .get(&key)
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_owned();
        command.push_str(&format!("printf '%s\\n' {id} >> '{marker}'; "));
        reload.insert(&key, value(command));
    }
    let mut history = Table::new();
    history.insert("reload", Item::Table(reload));
    let mut config = DocumentMut::new();
    config.insert("history", Item::Table(history));
    Ok(config)
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

fn invoke(run: &Run<'_>, root: &Path, selectors: &str, expression: &str) -> io::Result<ExitStatus> {
    if run.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    use std::ffi::OsStr;
    let mut command = validation::command(
        run,
        root,
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
        false,
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
    fn a_late_atomic_save_keeps_the_new_bytes_after_completed_install() {
        let fixture = Fixture::new();
        let plan = fixture.stage();
        let stage = Dir::open(&plan.stage).unwrap();
        let root = Dir::open(&plan.anchor).unwrap();
        let previous = stage.read_path(Path::new("expected")).unwrap();
        let replacement = stage.read_path(Path::new("replacement")).unwrap();
        super::super::guard::replace_consuming(
            &root,
            &plan.path,
            &stage,
            &CStrName::new("previous").unwrap(),
            &CStrName::new("publish").unwrap(),
            &previous,
            &replacement,
        )
        .unwrap();
        let prepared = fixture.root.join("editor-save");
        fs::write(&prepared, b"late atomic save").unwrap();
        fs::rename(prepared, plan.anchor.join(&plan.path)).unwrap();
        let mut receipt = fixture.receipt();
        recover(&fixture.git, &mut receipt).unwrap();
        assert_eq!(
            fs::read(plan.anchor.join(&plan.path)).unwrap(),
            b"late atomic save"
        );
        let [backup] = receipt.recovery_files.as_slice() else {
            panic!("expected the old inode");
        };
        assert_eq!(fs::read(backup).unwrap(), b"committed source");
        assert!(!has_pending(&fixture.git));
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
