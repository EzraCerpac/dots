mod checkout;
mod dotfiles;
mod git;
mod guard;
pub mod lock;
mod merge;
mod remote;
mod snapshot;
mod validation;

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::{CheckoutShape, Vars, checkout_shape};
use crate::history;
use crate::receipt::{self, ChangeGroup, Display, Issue, Receipt};
use crate::report::Command;
use crate::run::{self, Outcome, Run, Sink, StageId};
use crate::{Args, Ctx, Fail};

use git::{Git, repository_dirs};
use lock::RepoLock;
use remote::{Fetch, Origin, PushError, RemoteHeads};
use snapshot::{ScanError, Scratch, changed, machine_local_in_history};

const RERUN: &str = "Review the checkout and rerun dots sync.";
const CONFIG: &str = "Check config.toml and the selected profile, then rerun dots sync.";
const REMOTE: &str = "Check origin and dots_source_repo, then rerun dots sync.";
const GIT: &str = "Check the checkout with git status, then rerun dots sync.";
const GUIDE: &str = "Use a plain Git dev checkout as described in the dots sync guide.";

pub struct Shared {
    dir: PathBuf,
    common: PathBuf,
}

pub enum Loaded {
    LocalOnly,
    Problem(run::Issue),
    Ready(Shared),
}

pub fn sync(ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    let execution = Run::begin(ctx, Sink::Report(Command::Sync));
    let stage = execution.start("Sync");
    let display = if ctx.flags.plain {
        Display::Plain
    } else {
        Display::Standalone
    };
    let receipt = run(&execution, stage, load(ctx), display);
    for issue in &receipt.issues {
        execution.issue(
            stage,
            run::Issue::new(&issue.diagnostic, &issue.next_action),
        );
    }
    if let Some((before, after)) = &receipt.adopted_revision {
        execution.change(
            stage,
            run::Change {
                item: "configuration revision".into(),
                before: before.clone(),
                after: after.clone(),
            },
        );
    }
    execution.finish(stage, receipt.stage_outcome);
    let _ = execution.end(0);
    Ok(0)
}

fn shape(git: &Git<'_>) -> Result<CheckoutShape, Fail> {
    checkout_shape(git.root(), |args| {
        git.output(args)
            .map_err(|_| Fail::new(1, "Git checkout could not be read"))
    })
}

fn shape_problem(shape: &CheckoutShape) -> Option<Note> {
    if shape.git_too_old {
        Some(Note(
            "Sync needs Git 2.43 or newer",
            "Upgrade Git and rerun dots sync.",
        ))
    } else if shape.jj_workspace {
        Some(Note("Sync stopped in a JJ workspace", GUIDE))
    } else if shape.legacy_sync {
        Some(Note("Sync found pre-1.0 source state", GUIDE))
    } else if shape.head.as_deref() != Some(OsStr::new("refs/heads/dev")) || !shape.has_commit {
        Some(Note(
            "Sync needs Git HEAD on a dev branch with a commit",
            GUIDE,
        ))
    } else if !shape.operations.is_empty() {
        Some(Note(
            "Sync skipped while a Git operation is in progress",
            "Finish the Git operation and rerun dots sync.",
        ))
    } else {
        None
    }
}

pub fn load(ctx: &Ctx) -> Loaded {
    match load_source(ctx) {
        Ok(Some(shared)) => Loaded::Ready(shared),
        Ok(None) => Loaded::LocalOnly,
        Err(Note(diagnostic, action)) => Loaded::Problem(run::Issue::new(diagnostic, action)),
    }
}

fn load_source(ctx: &Ctx) -> Result<Option<Shared>, Note> {
    let git = Git::new(ctx, ctx.root.path());
    let checkout = shape(&git).map_err(|_| {
        Note(
            "Git is unavailable or the configuration is outside a Git checkout",
            GUIDE,
        )
    })?;
    if let Some(problem) = shape_problem(&checkout) {
        return Err(problem);
    }
    let (dir, common) =
        repository_dirs(&git).or_note("Could not resolve Git checkout directories", GUIDE)?;
    if common != checkout.common_dir {
        return Err(Note(
            "The Git common directory changed during preflight",
            GIT,
        ));
    }
    if checkout::has_pending(&dir) || dotfiles::has_pending(&dir) {
        return Ok(Some(Shared { dir, common }));
    }
    Ok(read_origin(ctx, &git)?.map(|_| Shared { dir, common }))
}

fn read_origin(ctx: &Ctx, git: &Git<'_>) -> Result<Option<Origin>, Note> {
    if !ctx.root.path().join("config.toml").is_file() {
        return Err(Note(
            "config.toml is missing",
            "Run dots init or set DOTS_ROOT to the configuration checkout.",
        ));
    }
    let vars = Vars::read(&ctx.root).or_note(
        "Could not read config.toml or its repository settings",
        CONFIG,
    )?;
    Origin::load(git, vars.source_repo).or_note(
        "origin is missing, unsafe, or differs from the configured repository",
        REMOTE,
    )
}

pub fn run(execution: &Run<'_>, stage: StageId, loaded: Loaded, display: Display) -> Receipt {
    let ctx = execution.ctx();
    let mut receipt = Receipt::new(Some(ctx.root.path().to_owned()), "not pushed");
    match loaded {
        Loaded::LocalOnly => {
            receipt.local_only = true;
            receipt.outcome = "local-only; add a remote with git remote add origin URL".into();
            receipt.stage_outcome = Outcome::Skipped;
        }
        Loaded::Problem(problem) => {
            receipt.issues.push(Issue {
                diagnostic: problem.diagnostic,
                next_action: problem.next,
            });
            receipt.stage_outcome = Outcome::Deferred;
        }
        Loaded::Ready(shared) => {
            if let Err(Note(reason, fix)) = synchronize(execution, stage, shared, &mut receipt)
                && !execution.cancelled()
            {
                issue(&mut receipt, reason, fix);
            }
        }
    }
    if execution.cancelled() {
        issue(
            &mut receipt,
            "Sync stopped after interruption",
            "Rerun dots sync to finish any pending checkout.",
        );
        receipt.outcome = format!("stopped · {}", receipt.outcome);
        receipt.stage_outcome = Outcome::Cancelled;
    } else if !receipt.issues.is_empty() {
        receipt.stage_outcome = Outcome::Deferred;
    }

    if receipt::emit(ctx, &receipt, display).is_err() {
        let _ = writeln!(
            io::stderr().lock(),
            "dots sync: could not save the sync receipt for replay"
        );
    }
    receipt
}

#[derive(Debug)]
struct Note(&'static str, &'static str);

trait Explanation<T> {
    fn or_note(self, reason: &'static str, fix: &'static str) -> Result<T, Note>;
}

impl<T, E> Explanation<T> for Result<T, E> {
    fn or_note(self, reason: &'static str, fix: &'static str) -> Result<T, Note> {
        self.map_err(|_| Note(reason, fix))
    }
}

fn issue(receipt: &mut Receipt, diagnostic: &'static str, action: &'static str) {
    receipt.issues.push(Issue::new(diagnostic, action));
}

fn path_issue(receipt: &mut Receipt, diagnostic: &'static str, path: &Path, action: &'static str) {
    // Debug path formatting escapes line breaks and control characters.
    let mut item = Issue::new(diagnostic, action);
    item.diagnostic = format!("{diagnostic}: {path:?}");
    receipt.issues.push(item);
}

fn active(run: &Run<'_>) -> Result<(), Note> {
    if run.cancelled() {
        Err(Note(
            "Sync stopped before launching more work",
            "Rerun dots sync.",
        ))
    } else {
        Ok(())
    }
}

fn synchronize(
    run: &Run<'_>,
    stage: StageId,
    shared: Shared,
    receipt: &mut Receipt,
) -> Result<(), Note> {
    active(run)?;
    let ctx = run.ctx();
    let git = Git::supervised(run);
    let _lock = RepoLock::try_acquire(&shared.common)
        .or_note("Could not open the source operation lock", GIT)?
        .ok_or(Note(
            "Sync skipped while another sync or publish is running",
            "Wait for it to finish, then rerun dots sync.",
        ))?;
    let checked_shape = shape(&git).or_note("Could not recheck the Git checkout", GIT)?;
    if let Some(problem) = shape_problem(&checked_shape) {
        return Err(problem);
    }
    if checked_shape.common_dir != shared.common {
        return Err(Note(
            "The Git common directory changed during preflight",
            GIT,
        ));
    }
    let root = git
        .root()
        .canonicalize()
        .or_note("Could not verify the source checkout directory", GIT)?;
    for metadata in [&shared.dir, &shared.common] {
        if metadata.starts_with(&root) && !metadata.starts_with(root.join(".git")) {
            return Err(Note(
                "Git metadata is inside a publishable checkout path",
                "Move Git metadata outside the worktree or into .git, then rerun dots sync.",
            ));
        }
    }
    dense_index(&git)?;
    if let Some(recovery) = finish_pending(run, &git, &shared.dir, receipt)? {
        let paths = recovery.paths.clone();
        adoption(receipt, &recovery);
        if !paths.is_empty() {
            receipt.recovered.push(ChangeGroup::new(
                Some(recovery.before),
                Some(recovery.after),
                paths,
            ));
        }
    }

    active(run)?;
    dotfiles::recover(&shared.dir, receipt).or_note(
        "A guarded dotfile write needs recovery",
        "Review the recovery path and rerun dots sync.",
    )?;
    let Some(origin) = read_origin(ctx, &git)? else {
        if receipt.recovered.is_empty() && receipt.recovery_files.is_empty() {
            receipt.local_only = true;
            receipt.outcome = "local-only; add a remote with git remote add origin URL".into();
        } else {
            receipt.outcome =
                "finished pending writes; add a remote with git remote add origin URL".into();
        }
        receipt.stage_outcome = Outcome::Skipped;
        return Ok(());
    };
    crate::config::selection::read(&ctx.root, &ctx.env).or_note(
        "The profile and machine selection could not be read",
        "Run dots profile --profile PROFILE --machine-id ID to repair the selection.",
    )?;
    crate::process::Mise::new(ctx).or_note(
        "mise is missing or this machine's profile could not be read",
        CONFIG,
    )?;
    if let Err(problem) = history::checkpoint(run, stage, ctx) {
        // Do not relay a history backend tail or command output.
        let _ = problem;
        return Err(Note(
            "Could not save encrypted tracked history",
            "Check the history configuration and run dots backup before retrying sync.",
        ));
    }

    let before = git
        .id(["rev-parse", "--verify", "HEAD^{commit}"])
        .or_note("Could not verify the dev revision", GIT)?;
    let base = git
        .tree(&before)
        .or_note("Could not read the dev tree", GIT)?;
    let mut index =
        checkout::index_snapshot(&shared.dir).or_note("Could not read the Git index", GIT)?;
    let scratch = Scratch::new(&git, &shared.dir, &shared.common).or_note(
        "Could not create a private sync workspace",
        "Check free space and permissions, then rerun dots sync.",
    )?;
    let mut copies =
        dotfiles::Copies::load(&shared.dir).or_note("Could not read guarded copy state", RERUN)?;
    if let Some(previous) = checkout::previous_index_base(
        &git,
        &index,
        &scratch.dir.path().join("reflog.index"),
        &before,
    )
    .or_note(
        "Could not verify the index against the current dev ref",
        GIT,
    )? {
        dense_index(&git)?;
        let old = git
            .tree(&previous)
            .or_note("Could not verify the preceding dev tree", GIT)?;
        let blocked = checkout::preflight(&git, &previous, &old, &base)
            .or_note("Could not reconcile a ref-only update of dev", GIT)?;
        if !blocked.is_empty() {
            for checkout::Blocked(path) in blocked {
                path_issue(
                    receipt,
                    "Kept an edit after dev moved outside sync",
                    &path,
                    GIT,
                );
            }
            return Ok(());
        }
        copies
            .seed(run, &scratch, &old)
            .or_note("Could not preserve the previous copy declarations", RERUN)?;
        copies
            .persist(&shared.dir)
            .or_note("Could not save the previous copy baselines", RERUN)?;
        checkout::begin(&git, &shared.dir, &index, &previous, &before, true)
            .or_note("Could not prepare the checkout after dev moved", GIT)?;
        let installed = finish_pending(run, &git, &shared.dir, receipt)?.ok_or(Note(
            "The checkout could not be reconciled after dev moved",
            GIT,
        ))?;
        index = installed.index_after;
        receipt.recovered.push(ChangeGroup::new(
            Some(previous),
            Some(before.clone()),
            installed.paths,
        ));
    }
    copies
        .seed(run, &scratch, &base)
        .or_note("Could not verify the previous dotfile declarations", RERUN)?;
    dense_index(&git)?;
    copies
        .capture(run, &git, &shared.dir, &base, receipt)
        .or_note(
            "Could not inspect deployed copies or record their recovery state",
            RERUN,
        )?;

    active(run)?;
    let unscanned = scratch.capture(&before).or_note(
        "Could not snapshot local edits",
        "Check Git identity, repository state and free space; your edits remain on disk.",
    )?;
    let candidate_tree = scratch
        .git
        .tree(&unscanned)
        .or_note("Could not verify the local snapshot", GIT)?;
    let local_paths = changed(&base, &candidate_tree);
    receipt.kept = local_paths.len();
    if !checkout::preserves_staged(
        &scratch.git,
        &scratch.dir.path().join("user.index"),
        &index,
        &before,
        &unscanned,
    )
    .or_note("Could not verify the staged local edits", GIT)?
    {
        return Err(Note(
            "A path has different staged and unstaged edits",
            "Save or reconcile the two versions with Git, then rerun dots sync.",
        ));
    }
    let fetch = Fetch::new(
        &git,
        scratch
            .nonce()
            .or_note("Could not reserve temporary fetch refs", GIT)?,
    );

    let approved = match scan_candidate(run, &scratch, &before, &unscanned, &local_paths, receipt) {
        Ok(approved) => approved,
        Err(error) => {
            if !run.cancelled()
                && let Ok(heads) = fetch.heads(run, &origin)
            {
                incoming(receipt, &git, &heads, &before);
            }
            return Err(scan_note(error));
        }
    };
    let local_tree = scratch
        .git
        .tree(&approved)
        .or_note("Could not verify clean local changes", GIT)?;
    let saved_paths = changed(&base, &local_tree);
    receipt.kept = local_paths.len().saturating_sub(saved_paths.len());
    if approved != before {
        active(run)?;
        dense_index(&git)?;
        scratch
            .promote(&approved, &before)
            .or_note("Could not retain the clean local commit", GIT)?;
        checkout::begin(&git, &shared.dir, &index, &before, &approved, false).or_note(
            "Could not prepare the local index update; edits remain on disk",
            GIT,
        )?;
        let installed = finish_pending(run, &git, &shared.dir, receipt)?
            .ok_or(Note("The local checkout update was not recorded", GIT))?;
        adoption(receipt, &installed);
        index = installed.index_after;
        receipt.saved.push(ChangeGroup::new(
            Some(before.clone()),
            Some(approved.clone()),
            saved_paths,
        ));
        receipt.outcome = "local work saved, not pushed".into();
    }

    copies
        .confirm_capture(&git, &shared.dir, &local_tree)
        .or_note("Could not record the saved copy baseline", RERUN)?;

    let Ok(mut heads) = fetch.heads(run, &origin) else {
        return Err(Note(
            "origin is offline or its refs could not be fetched",
            "Check the repository and your Git credentials, then rerun dots sync.",
        ));
    };
    incoming(receipt, &git, &heads, &before);

    for attempt in 0..=3 {
        active(run)?;
        let published: Vec<_> = heads.published().collect();
        if machine_local_in_history(&git, &approved, &published)
            .or_note("Could not verify unpublished Git history", GIT)?
        {
            return Err(Note(
                "NOT SHARED: unpublished history tracked a machine-local file",
                "Review local history without publishing it, then rerun dots sync.",
            ));
        }
        if !scratch
            .scan(run, &approved, &published)
            .map_err(scan_note)?
            .is_empty()
        {
            return Err(Note(
                "NOT SHARED: gitleaks flagged unpublished history",
                "Review the unpublished commits and remove or rotate any secret before sharing.",
            ));
        }

        active(run)?;
        let dev = merge::with_published(&scratch, &approved, heads.dev(), "dev").or_note(
            "Could not merge published dev in the private workspace",
            GIT,
        )?;
        let merged = merge::with_published(&scratch, &dev, heads.main(), "main").or_note(
            "Could not merge published main in the private workspace",
            GIT,
        )?;
        if merged != approved && !published.contains(&merged.as_str()) {
            let mut excluded = published.clone();
            excluded.push(&approved);
            if !scratch
                .scan(run, &merged, &excluded)
                .map_err(scan_note)?
                .is_empty()
            {
                return Err(Note(
                    "NOT SHARED: gitleaks flagged the merged configuration",
                    "Review the merged revisions before sharing.",
                ));
            }
        }
        let merged_tree = scratch
            .git
            .tree(&merged)
            .or_note("Could not verify the merged tree", GIT)?;
        let validated = match validation::validate(run, &scratch, &merged_tree) {
            Ok(validated) => validated,
            Err(error) => {
                let fix = "Check the merged mise configuration and machine-local ignore rules, then rerun dots sync.";
                if let Some(location) = error
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<validation::ConfigLocation>())
                {
                    let mut issue = Issue::new("Merged configuration failed validation", fix);
                    issue.diagnostic =
                        format!("Merged configuration failed validation at {location}");
                    receipt.issues.push(issue);
                    return Ok(());
                }
                return Err(Note(
                    "Merged configuration failed validation; nothing pushed or applied",
                    fix,
                ));
            }
        };

        if !validated
            .still_current(git.root())
            .or_note("Could not recheck machine-local files", RERUN)?
        {
            return Err(Note("Machine-local files changed during validation", RERUN));
        }

        let blocked = checkout::preflight(&scratch.git, &approved, &local_tree, &merged_tree)
            .or_note("Could not verify incoming checkout paths", RERUN)?;
        let sharing = if heads.dev() == Some(merged.as_str()) {
            None
        } else {
            let published_tree = match heads.dev() {
                Some(head) => git
                    .tree(head)
                    .or_note("Could not verify the published dev tree", GIT)?,
                None => git::Tree::new(),
            };
            Some(ChangeGroup::new(
                heads.dev().map(str::to_owned),
                Some(merged.clone()),
                changed(&published_tree, &merged_tree),
            ))
        };
        active(run)?;
        scratch
            .promote(&merged, &approved)
            .or_note("Could not retain the validated merged revision", GIT)?;
        let pushed = match remote::push(&git, run, &origin, heads.dev(), &merged) {
            Ok(()) => true,
            Err(PushError::Rejected) => false,
            Err(PushError::NotAttempted) => {
                return Err(Note(
                    "NOT SHARED: the conditional dev push could not be started",
                    "Check the verified revision and origin settings, then rerun dots sync.",
                ));
            }
            Err(PushError::Unconfirmed) => {
                receipt.outcome = if receipt.saved.is_empty() {
                    "dev push unconfirmed".into()
                } else {
                    "local work saved · dev push unconfirmed".into()
                };
                return Err(Note(
                    "Could not verify whether origin accepted the dev push",
                    "Check origin's dev revision before retrying dots sync.",
                ));
            }
        };
        if pushed {
            receipt.outcome = if let Some(sharing) = sharing {
                receipt.shared.push(sharing);
                "pushed dev".into()
            } else {
                "dev up to date".into()
            };
            if !blocked.is_empty() {
                for checkout::Blocked(path) in blocked {
                    path_issue(
                        receipt,
                        "Kept checkout because this incoming path is obstructed or edited",
                        &path,
                        RERUN,
                    );
                }
                return Ok(());
            }

            active(run)?;
            if merged != approved {
                dense_index(&git)?;
                checkout::begin(&git, &shared.dir, &index, &approved, &merged, true)
                    .or_note("Could not prepare guarded checkout", GIT)?;
                let installed = finish_pending(run, &git, &shared.dir, receipt)?
                    .ok_or(Note("The merged checkout update was not recorded", GIT))?;
                adoption(receipt, &installed);
                if !installed.paths.is_empty() {
                    receipt.updated.push(ChangeGroup::new(
                        Some(installed.before),
                        Some(installed.after),
                        installed.paths,
                    ));
                }
            }
            active(run)?;
            if !validated
                .still_current(git.root())
                .or_note("Could not recheck machine-local files before apply", RERUN)?
            {
                return Err(Note(
                    "Machine-local files changed after validation; dotfiles were kept",
                    RERUN,
                ));
            }
            if dotfiles::retire(&git, &shared.dir, &validated.retired, receipt)
                .or_note("Could not update retired machine-local declarations", RERUN)?
            {
                copies
                    .apply(run, &git, &shared.dir, &merged_tree, &validated, receipt)
                    .or_note("Could not safely apply the validated dotfiles", RERUN)?;
            }
            return Ok(());
        }
        active(run)?;
        if attempt == 3 {
            break;
        }
        let refreshed = fetch.heads(run, &origin).or_note(
            "Could not verify dev after the rejected push",
            "Check origin and the Git credential helper, then rerun dots sync.",
        )?;
        if refreshed
            .withdraws(&git, &heads)
            .or_note("Could not verify refreshed published history", GIT)?
        {
            return Err(Note(
                "Published dev or main was removed or rewound during sync",
                "Review origin's branch history and rerun dots sync.",
            ));
        }
        if !refreshed.same_incoming(&heads) {
            incoming(receipt, &git, &refreshed, &before);
        }
        if refreshed.dev() == heads.dev() {
            break;
        }
        heads = refreshed;
    }
    Err(Note(
        "NOT SHARED: the conditional dev push was rejected",
        "Check origin, credentials and remote branch policies, then rerun dots sync.",
    ))
}

fn dense_index(git: &Git<'_>) -> Result<(), Note> {
    git.ensure_dense_index().or_note(
        "Sync kept the checkout because the full Git index could not be verified",
        "Disable sparse checkout and clear skip-worktree entries, then rerun dots sync.",
    )
}

fn scan_candidate(
    run: &Run<'_>,
    scratch: &Scratch<'_>,
    before: &str,
    first: &str,
    paths: &[PathBuf],
    receipt: &mut Receipt,
) -> Result<String, ScanError> {
    let mut candidate = first.to_owned();
    let mut rejected = std::collections::BTreeSet::new();
    loop {
        let found = scratch.scan(run, &candidate, &[before])?;
        if found.is_empty() {
            return Ok(candidate);
        }
        if found.keys().any(|path| !paths.contains(path)) {
            return Err(ScanError::Failed);
        }
        let tree = scratch
            .git
            .tree(&candidate)
            .map_err(|_| ScanError::Failed)?;
        let count = rejected.len();
        for (path, found) in found {
            if rejected.insert(path.clone()) {
                const DIAGNOSTIC: &str =
                    "NOT SHARED: gitleaks flagged a local file; it stays on disk";
                let mut location = path.into_os_string();
                if let Some(line) = found.line
                    && let Some(entry) = tree.get(Path::new(&location))
                    && matches!(entry.mode.as_str(), "100644" | "100755")
                    && scratch
                        .git
                        .get(["cat-file", "blob", &entry.oid])
                        .is_ok_and(|blob| {
                            blob.split_inclusive(|&byte| byte == b'\n')
                                .nth(line - 1)
                                .is_some()
                        })
                {
                    location.push(format!(":{line}"));
                }
                let mut reported = Issue::new(
                    DIAGNOSTIC,
                    "Remove or rotate the secret before sharing this path.",
                );
                reported.diagnostic = format!("{DIAGNOSTIC}: {location:?}");
                if let Some(rule) = found.rule {
                    use std::fmt::Write as _;
                    let _ = write!(&mut reported.diagnostic, " ({rule})");
                }
                receipt.issues.push(reported);
            }
        }
        if rejected.len() == count {
            return Err(ScanError::Failed);
        }
        candidate = scratch
            .omit(before, &candidate, &rejected)
            .map_err(|_| ScanError::Failed)?;
    }
}

fn scan_note(error: ScanError) -> Note {
    match error {
        ScanError::Missing => Note(
            "NOT SHARED: gitleaks not found; local edits kept on disk, nothing pushed",
            "Install gitleaks and rerun dots sync.",
        ),
        ScanError::Failed => Note(
            "NOT SHARED: the gitleaks scan failed; nothing pushed",
            "Check gitleaks and its rules, then rerun dots sync.",
        ),
    }
}

fn incoming(receipt: &mut Receipt, git: &Git<'_>, heads: &RemoteHeads, before: &str) {
    let count = |tip: Option<&str>| {
        tip.and_then(|tip| git.text(["rev-list", "--count", tip, "--not", before]).ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };
    receipt.pulled_dev = count(heads.dev());
    receipt.pulled_main = count(heads.main());
}

fn adoption(receipt: &mut Receipt, installed: &checkout::Installed) {
    if installed.before != installed.after {
        let entry = receipt
            .adopted_revision
            .get_or_insert_with(|| (installed.before.clone(), installed.after.clone()));
        entry.1.clone_from(&installed.after);
    }
}

fn finish_pending(
    run: &Run<'_>,
    git: &Git<'_>,
    dir: &Path,
    receipt: &mut Receipt,
) -> Result<Option<checkout::Installed>, Note> {
    match checkout::finish(git, dir, || run.cancelled()) {
        Ok(Some((installed, backups))) => {
            receipt.recovery_files.extend(backups);
            Ok(Some(installed))
        }
        Ok(None) => Ok(None),
        Err(error) => {
            receipt.recovery_files.extend(error.backups);
            if let Some(path) = error.path {
                path_issue(
                    receipt,
                    "Kept a late edit during guarded checkout",
                    &path,
                    RERUN,
                );
            }
            Err(Note(
                "A guarded checkout is pending; sync kept your files",
                "Resolve the obstruction and rerun dots sync to finish the pending checkout.",
            ))
        }
    }
}
