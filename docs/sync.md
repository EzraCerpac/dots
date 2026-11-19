# Sync

`dots sync`, and `dots up` before its updates, share a configuration between machines through Git.
This guide covers how a run works, what its output means and what to do about each attention line.
The code is [`lib/source-repo.mjs`](../lib/source-repo.mjs) (checks, locking, output) and [`lib/source-dev.mjs`](../lib/source-dev.mjs) (the run itself).

## dev and main

The configuration repository has two shared branches on `origin`.

- `dev` is what every machine runs. Sync commits local edits onto it and pushes them without force.
- `main` changes only through reviewed pull requests. Every sync merges it into `dev`, so a merged pull request reaches each machine on its next `dots up`.

The live checkout (`DOTS_ROOT`) is a plain Git checkout with `HEAD` on `dev`.
Unpushed work is exactly `origin/dev..dev`.
Machine-local files (`miserc.toml`, `config.local.toml`, `config.host-*.toml`, mise lockfiles, `locks/`, `.setup-state/`) never enter the repository.

## One run

1. Saves tracked history with `mise bootstrap dotfiles save` after checking encryption.
   Captures edits to regular copied files whose tracked source still matches the last common version.
   It remembers the captured version even if checkout is blocked.
   If both have changed, the source and deployed copy stay in place.
   Capture uses the same guarded replacement as copy installation.
   The previous source stays at the printed private path,
   and an interrupted capture restores a missing source before snapshotting.
2. Commits every tracked or unignored public file onto `dev` as `Save local setup (HOST)`.
   Machine-local files are always left out.
3. Scans that commit with gitleaks. A flagged file stays on disk, unshared; the rest is shared.
   A flagged commit already in `origin/dev..dev` blocks the push. Without gitleaks nothing is shared or pushed.
4. Fetches `dev` and `main`, or continues offline.
5. Merges local work with `origin/dev`, then with `origin/main`, without touching the live files.
   On a conflicting hunk the published side wins: `main` over `dev`, `dev` over local work.
   Binary, file-type and delete conflicts take the published file whole.
   Both parents stay in history, so the losing version remains recoverable.
6. Validates the merged configuration with this machine's local files; an invalid result is neither pushed nor applied.
   Local `[dotfiles.NAME]` tables that the merged `dots/retired-declarations` retires are validated as already removed (see [the `dots/` reference](../init/README.md#retired-dotfiles-and-retired-declarations)).
7. Only fast-forwards `origin/dev`.
   Git checks that the remote tip still matches the one used to validate the update.
   Sync merges and retries up to three times if another machine advanced the branch.
   If published history disappears during a retry, sync keeps local work,
   skips the withdrawn checkout and dotfile changes, and asks you to inspect the remote.
   A push that fails for any other reason is not retried, and one that takes longer than 60 seconds is stopped; Git never prompts for credentials here.
   Stopping it stops Git, not a hook or credential helper Git started, so a push may still land later; the next sync reconciles it.
   Diagnostics include the failed operation and recovery action. URLs have credentials removed.
8. Checks incoming tracked paths for local obstructions, including ignored files, directories and symlinks.
   If a path is blocked, unrelated edits can still be shared.
   The two-tree checkout also refuses to overwrite a file edited during the run.
   When it retires tables, it moves only while the local files are still the ones it validated.
   If checkout, index installation or the branch update is interrupted, the next run finishes that checkout before taking a new snapshot.
   Once the files, index and branch agree, sync removes the retired tables and applies dotfiles.
   Sync installs regular copies by moving the actual target aside before checking it and creating its replacement only at an empty path.
   If a writer claims that path, sync leaves it alone and prints the recovery path for any file it moved aside.
   A successful replacement keeps the previous inode at the printed private path so an editor that still has it open can finish saving it.
   For a directory copy, mise stages the desired tree privately.
   When sync would replace an existing file in a directory copy, it compares the file with the source previously deployed at that path.
   If both bytes and permissions match, sync treats the file as deployed regardless of who created it and updates it to the new source without reporting it as kept.
   If the bytes or verified permissions differ at the target, including a change detected after the initial comparison, sync leaves the file there and reports it as kept.
   If an editor writes to the original file after sync has moved it aside, the edit survives at the printed private recovery path even when the replacement is already at the target.
   When the deployed mode cannot be verified, sync updates the file only if its bytes match the previously deployed source and its current mode can stay.
   Unmatched local files stay in place.
   Mise runs the usual hooks and applies the other targets in dependency order.
   A target that requires a retained copy waits for reconciliation.
   Matching `[history.reload]` commands run once after the writes
   and before the post-dotfiles hook.
   If an incoming declaration replaces that copy, move the local file somewhere safe before applying the replacement.

Capture and guarded installation within the worktree keep recovery files in
that worktree's Git directory.
If Git metadata and the file are on different filesystems,
sync reports the affected path and preserves both versions.
Use a checkout with Git metadata on the same filesystem to resume automatic capture.

Commits pushed straight to `origin/dev` from a task worktree are fine: the next sync merges them.
A `dev` moved with plumbing in another worktree of the live checkout is merged too, not reverted.

## The outcome block

A clean run fits on one line.
When files change, the receipt leads with any issue and its next action,
then lists up to five paths per group.

- **Shared on dev** covers the change accepted by the remote,
  including work saved on an earlier offline run.
- **Saved locally** covers edits recorded on this machine's `dev`.
- **Updated in this checkout** covers files that reached the configuration checkout.
  A later failure to deploy a dotfile is reported separately.
- **Finished pending checkout** covers an update completed after an interruption.

In a terminal, standalone sync can show a complete small text diff
for up to two paths and twelve displayed lines across the run.
Use `dots sync --last` to inspect the latest finished sync for this checkout,
including the sync at the beginning of `dots up`.
Search with `/` in the pager and press `q` to return.
Replay reads the recorded Git revisions locally.
It spends up to three seconds screening patches and retains up to 1 MiB for the pager.
The pager lists every recorded file and counts omitted patches.
Piped output and saved reports contain metadata only.
Binary, encrypted and unverified changes remain visible as file entries.
Showing a patch requires Gitleaks to return a clean JSON report.
Versions that omit the report on a clean scan show the file list only.

### Verbose diagnostics

`dots --verbose sync` includes the Git summary and the underlying diagnostics.

| Part | Values |
| --- | --- |
| Local work | `shared N files` (pushed), `already shared N files` (another push included them), `saved N files locally` (committed, not pushed), `kept N files on disk` (not committed; the files are untouched), `no local changes` |
| Incoming | `pulled dev +N, main +M` (applied), `fetched dev +N, main +M (not applied)`, `offline` |
| Push | `pushed COMMIT`, `nothing to push`, `not pushed` |

Indented lines follow.
The updater records each issue with its diagnostic and recovery action.
Git and mise diagnostics appear under the relevant issue.
Hook failures identify the failed step and keep patch contents out of the report.
Lower-case lines are information.

| Line | Meaning and what to do |
| --- | --- |
| `CONFLICT kept published: PATH  (yours: git show COMMIT:PATH)` | Both sides changed the same hunk; the published version is on disk. If yours mattered, recover it with the printed command, edit the file and sync again. `(yours: deleted in COMMIT)` means you had deleted it. |
| `NOT SHARED (gitleaks RULE): PATH:LINE` | gitleaks flagged a new edit. The file stays on disk and out of `dev`. Remove the secret, or add a reviewed allowlist entry to `.gitleaks.toml` or `.gitleaksignore` for a false positive. |
| `NOT SHARED: REASON; local edits kept on disk, nothing pushed` | The scan could not run (for example gitleaks is not installed). Install or fix gitleaks and sync again. |
| `NOT SAVED: REASON; local edits kept on disk, rerun` | Another Git command held the index or `dev`, so the run left both alone and did not record local edits; incoming work may still be pulled. Run sync again once that command finishes; remove a lock left by a crashed Git command by hand. |
| `NOT PUSHED (gitleaks RULE): commit COMMIT PATH:LINE; remove it from local history` | An unpushed commit on `dev` contains a finding, so nothing is pushed. Rewrite that commit out of `origin/dev..dev` by hand (it was never published), then sync again. |
| `NOT PUSHED: commit COMMIT changes private setup file PATH; remove it from local history` | An unpushed commit on `dev` tracked a machine-local file, even if a later commit removed it, so nothing is pushed. Rewrite that commit out of `origin/dev..dev` by hand, then sync again. |
| `NOT PUSHED: REASON` | The scan of unpushed commits could not run. Fix gitleaks and sync again. |
| `NOT PUSHED: origin/dev kept moving; retried next run` | Three pushes in a row lost a race. Nothing to do; the next run retries. |
| `NOT PUSHED: git push failed: REASON` | The push failed while `origin/dev` stayed where it was: `REASON` is Git's own error, for example rejected credentials or a protected branch. Fix that and sync again; local work is committed on `dev`. |
| `NOT PUSHED: git push timed out after 60s; check the Git credential helper for origin, then rerun` | The push hung, usually on a credential helper waiting for input it cannot get. Check that `git push origin dev` works in the live checkout without a prompt, then sync again. |
| `OFFLINE: N commits unpushed, retried next run` | `origin` was unreachable. Local work is committed and applied; the next run pushes it. |
| `INVALID merged config: ERROR; nothing pushed or applied` | The merged configuration fails mise's checks with this machine's local files. Fix the error (locally or upstream) and sync again; local work is already committed on `dev`. |
| `LATE EDIT: PATH not updated; rerun` | A file changed while the run was updating files, so the run left it alone. Run sync again. |
| `BLOCKED PATH: PATH obstructs an incoming tracked path; move it aside, rerun sync, then reconcile it` | An addition from Git would overwrite local material. Move the printed path somewhere safe, sync again, then compare it with what arrived. |
| `NOT APPLIED: the Git index is locked (PATH); ...` | Run sync again after the other Git operation finishes. If the printed lock was left by an interrupted run, remove that lock and sync again. |
| `NOT APPLIED: FILE changed during sync; files not updated, rerun` | A machine-local config file changed after the merged configuration retiring a declaration was validated against it, so the run left the files and `dev` alone; anything it pushed arrives on the next run. Run sync again. |
| `NOT RETIRED: local declaration [TABLE] in FILE, which changed during sync; rerun, or remove the table by hand` | `FILE` changed between the file update and the removal, so the run left it alone and the table may name a source that is gone. Run sync again; if it then reports `INVALID merged config`, remove the table from `FILE` by hand. |
| `NOT APPLIED: ERROR; checkout pending, rerun sync to finish it` | Checkout, index installation or the branch update was interrupted. Fix the reported error and run sync again. If a path differs from both versions, move it aside first and reconcile it with what arrives. |
| `SAVE REFUSED: history entry PATH in FILE is not encrypted; ...; nothing saved` | A tracked file is not declared with `encrypt = true` (or the tracked-path inventory could not be read), so nothing was saved. Fix the declaration and sync again. |
| `SAVE FAILED: mise bootstrap dotfiles save (exit N); tracked history not saved` | The history checkpoint failed. Fix mise's error below the line and sync again. |
| `COPY KEPT: TARGET: REASON; compare it with its source and reconcile the two files, then rerun` | The deployed copy still holds different bytes and sync left it in place. Compare the files and put the version you want in the source. Bring the target into agreement, then run sync. |
| `COPY KEPT: TARGET: REASON; recover the previous file from PATH; ...` | Sync kept a displaced file after an edit during installation or after another writer claimed the target. The file at `PATH` and the source are available for reconciliation. |
| `CAPTURE FAILED: ERROR; fix the error and rerun sync` | Sync could not check or capture the copy. Fix the reported problem and rerun sync. |
| `RELOAD FAILED: ERROR` | Dotfiles were written and a reload could not be selected or run. Check the matching `[history.reload]` rule and retry its command. |
| `APPLY FAILED: mise bootstrap dotfiles apply (exit N); fix the error below, then run dots apply` | The configuration advanced but dotfiles were not applied. Fix mise's error below the line and run the printed command. Sync asks you to rerun sync itself when it skipped a copy or disabled history during apply. `timed out` means apply and its hooks ran longer than 60 seconds. |
| `RETIRED local declaration [TABLE] from FILE (source SOURCE no longer exists); deployed files left in place, backup PATH` | The merged configuration retires this table and the files it pointed into are gone, so the run removed exactly that table from the machine-local `FILE`; the deployed file and app data are untouched, and `PATH` holds the file as it was. Nothing to do; restore from `PATH` if you had customized the table. A manager from before this line existed cannot do this: it reports `INVALID merged config` instead, so remove the table by hand once there. |
| `changed on disk: STAT` | Files the run updated. |
| `dev moved outside sync (now COMMIT), or the index was staged to match COMMIT; ...` | The Git index matched an older commit, so local files were treated as based on it and merged onto `dev`. Nothing on disk is lost, but a revert staged by hand is undone; commit such a revert instead. |

A run can also end with a single line instead:

| Line | Meaning and what to do |
| --- | --- |
| `dots sync: stopped · PROBLEM. Sync needs a plain Git checkout on dev; ...` | The checkout is missing, has a foreign `origin`, or has the wrong shape (see [checkout shape](#checkout-shape)). |
| `dots sync: stopped · missing setting: add dots_source_repo to [vars] in FILE` | `config.toml` does not name the repository, so sync cannot check `origin`. Add `dots_source_repo = "OWNER/NAME"` under `[vars]` and sync again. |
| `dots sync: skipped · a Git operation is in progress (NAME); ...` | Finish or abort the merge, rebase, cherry-pick, revert or bisect, then sync again. |
| `dots sync: skipped · another source operation is running ...` | Another sync or publish holds the lock. A lock left by a dead process on this host is reclaimed on the next run. |
| `dots sync: skipped · GIT cannot merge-tree -X; install Git 2.43 or newer` | See [requirements](#requirements). |
| `dots sync: failed · ERROR` | An unexpected error. Read it, fix the cause and sync again. |

A normal sync exits zero and reports its outcome in the receipt.
The updater continues with its remaining stages after a sync issue.

## Requirements

- Git 2.43 or newer. Sync merges with `git merge-tree --write-tree -X theirs`; `merge-tree` gained `--write-tree` in 2.38 and `-X` only in 2.43.0 (its release notes: "git merge-tree learned to take strategy backend specific options via the -X option"). Git 2.39.5 rejects `-X` with `unknown switch`; Git 2.56 accepts it. Sync checks this itself and skips with a message on an older Git.
- gitleaks, for every share and push.
- `origin` pointing at the GitHub repository named in `[vars] dots_source_repo`, with `main` and `dev` branches.
- A `.gitignore` that ignores the machine-local files and a `.gitleaks.toml`; `dots init` writes both. Sync refuses a merged configuration whose ignore rules would expose a local file.

## Checkout shape

Sync works on one shape only: a plain Git checkout at the repository root, with `HEAD` on `dev` and at least one commit.
It stops on a detached `HEAD`, another branch, a JJ workspace (`.jj`) or state from an older sync (`.git/dots-dev.json`).
`dots bootstrap` moves a fresh clone that is still on `main`, with no edits, to `dev`.
Anything else is converted once, by hand, so that local edits and unpushed work are kept; the configuration repository may document the exact steps for its machines.

The live checkout does not support JJ.
When Git `HEAD` follows `dev` and another worktree moves `dev`, JJ re-parents its working copy onto the new commit without changing files and records the difference as a revert, which sync could not tell apart from a real local edit.

## Pull requests into main

Reviewed changes reach `main` through a pull request from a task branch or a JJ workspace, never from the live checkout.
To extract a change made on `dev`, create a task branch from `origin/main`, copy or cherry-pick only that change, and review its diff against `main`.
Do not move `dev` by hand or merge it wholesale into `main`.
`dots publish` runs in a colocated JJ workspace of the configuration repository: it scans the outgoing commits with gitleaks, asks for confirmation, pushes a bookmark and opens or updates the pull request.
No command force-pushes, rebases a task, or writes conflict markers into live files.
