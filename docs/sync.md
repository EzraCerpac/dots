# Sync

`dots sync`, and `dots up` before its updates, share a configuration between machines through Git.
This guide covers when sync shares, how a run works, what its receipt means and what to do about each issue.
[The contract](contract.md#dots-sync) is the precise reference.

## Local-only or shared

`dots` decides at each run whether the configuration is shared:

| `dots_source_repo` | `origin` | Mode |
| --- | --- | --- |
| unset | absent | Local-only |
| unset | present | Shared through `origin` |
| set | present and matching | Shared through `origin` |
| set | absent | Diagnostic: the configured remote is missing |
| set | present, not matching | Diagnostic: `origin` is not the configured repository |

A local-only `dots sync` prints a one-line receipt saying how to add a remote, and skips the shared pipeline: it saves no history, captures no copies, and makes no network call, scan or commit.
A checkout or guarded dotfile write that an earlier shared run left pending is still finished first, even without `origin`, and the receipt says what it finished.
When the configuration is local-only and nothing is pending, `dots up` skips its sync stage with one line and no sync receipt, still runs its own checkpoints, and needs no attention for it.
`dots apply` applies declarations locally in either mode.

A configuration that names a remote, or has an `origin`, is never local-only.
When the remote cannot be reached, sync reports it as offline and work waits for the next run.
A reachable `origin` without `dev` or `main` is online, not offline: sync scans every local commit that no published branch contains, creates `dev` with an ordinary push, and treats a missing `main` as empty.
Sync never creates or moves `main`.

### Adding a remote

`dots init` prints the commands for a new, empty remote repository while its seed commit is untouched, pushing exactly that seed to `main` and `dev`.
For any other local-only repository, add the remote and let sync scan and push `dev`:

```sh
git -C "$(dots root)" remote add origin URL
dots sync
```

### Remote URLs and credentials

A repository setting is GitHub `OWNER/NAME`, a GitHub URL, or another Git URL.
A GitHub repository matches an `origin` spelled as `https://github.com/OWNER/NAME`, `ssh://git@github.com/OWNER/NAME` or `git@github.com:OWNER/NAME`, each with or without `.git`.
Any other URL matches only an `origin` with exactly the same string.
`dots` refuses a URL carrying a password or token, a remote-helper form such as `ext::`, and a value starting with `-`.

Credentials come from your normal Git setup: SSH keys, or a configured credential helper.
Sync needs credentials that work without a prompt. Use the action in the sync receipt if authentication fails.
A repository that needs authentication belongs on `https://` or SSH, not `http://`.
Before contacting a remote, `dots` resolves the effective fetch URL and every push URL after Git's URL rewrites on this machine, and stops unless each one matches the expected repository.

Settings history lives in a separate private repository, `dots_state_repo`.
`dots` refuses a history push whose destination is the source repository.

## dev and main

A shared configuration has two branches on `origin`.

- `dev` is what every machine runs.
  Sync commits local edits onto it and pushes them without force.
- `main` changes only through reviewed pull requests.
  Every sync merges it into `dev`, so a merged pull request reaches each machine on its next `dots up`.

The live checkout is a plain Git checkout with `HEAD` on `dev`.
Unpushed work is exactly `origin/dev..dev`.
Machine-local files (`miserc.toml`, `config.local.toml`, `config.host-*.toml`, mise lockfiles, `locks/`, `.setup-state/`) never enter the repository, and sync refuses a merged configuration whose ignore rules would expose one.
Commits pushed straight to `origin/dev` from a task worktree are fine: the next sync merges them.

## Checkout shape

Sync works on one shape only: a plain Git checkout at the repository root, with `HEAD` on `dev` and at least one commit.

- It stops with one line on a detached `HEAD`, another branch, a `.jj` directory, or state from a pre-1.0 sync layout.
- `dots bootstrap` moves a fresh clone that is still on `main`, with no edits, to `dev`.
  Anything else is converted once, by hand, so local edits and unpushed work are kept.
- It skips with one line while a merge, rebase, cherry-pick, revert or bisect is in progress, or while another sync or publish on the same repository runs.
  A lock left by a dead process on this host is reclaimed.
- It skips with one line when Git is older than 2.43.

The live checkout does not support JJ.
When Git `HEAD` follows `dev` and another worktree moves `dev`, JJ re-parents its working copy onto the new commit without changing files and records the difference as a revert, which sync could not tell apart from a real local edit.

## One shared run

1. **Checkpoint.** Saves tracked settings history, refusing any tracked entry that is not `encrypt = true`.
2. **Capture.** When a deployed copy of a dotfile changed and its source still matches the last common version, the deployed bytes become the source.
   When both changed, both stay as they are and sync reports it.
3. **Candidate.** Builds a candidate commit from every tracked or unignored public edit, leaving machine-local files out, without moving `dev` or the real index.
   Existing staging is preserved; a path whose staged and unstaged versions differ stops sync until you reconcile them.
4. **Scan.** Scans the candidate with gitleaks before `dev` or the index advances.
   A flagged file stays on disk and out of the candidate, and the rest is scanned again; only a clean candidate becomes `dev`.
   A rejected candidate never reaches `dev`, the real index or the push, and its edits stay on disk.
   Without a working gitleaks, or when a scan fails, nothing is shared or pushed.
5. **Fetch.** Fetches `dev` and `main`.
   When `origin` cannot be reached, the run ends here: local work stays saved on `dev`, and nothing incoming is applied.
6. **Merge.** Plans the merge of local work with `origin/dev`, then with `origin/main`, without changing the live checkout.
   On a conflicting hunk, `main` wins over `dev` and `dev` wins over local work.
   Binary, file-type and delete conflicts take the published file whole.
   Both parents stay in history, so the losing version is recoverable.
7. **Validate.** Validates the merged configuration with this machine's local files; an invalid result is neither pushed nor applied.
   Local tables that `dots/retired-declarations` retires are validated as already removed; see [the `dots/` reference](../src/init/README.md#retired-dotfiles-and-retired-declarations).
8. **Push.** Before each attempt, checks the outgoing commits, those that no published branch contains, for gitleaks findings and for any machine-local file they ever tracked; either blocks the push, and sync never rewrites the branch to remove it.
   It then pushes the exact validated commit to `refs/heads/dev` at the checked push URL, only to create or fast-forward the branch, and only if the remote `dev` is still the commit it expected or still absent.
   After a rejection it fetches again, and retries up to three times only when `dev` moved and no published history was withdrawn.
   If published `dev` or `main` was removed or rewound, it stops and keeps local work.
   If an attempted push has an uncertain outcome, including one stopped after 60 seconds, sync stops before any retry, checkout or apply; see [Push unconfirmed](#issues-and-what-to-do).
9. **Update.** Only after the remote has confirmed `dev`, updates the checkout and applies dotfiles under the guarded rules below.

Sync never force-pushes, skips validation or writes conflict markers into live files.

### Guarded checkout and copies

- Before writing, sync checks incoming paths for local obstructions, including ignored files, directories and symlinks.
  A blocked path stops the checkout, and unrelated edits can still be shared when the push is confirmed.
- `dots` guards its own checkout and file writes against intervening edits: a file edited during the run is left alone and reported with a recovery action.
- Before a fresh snapshot, sync finishes an interrupted checkout, index installation or `dev` update only while the checkout, index and `dev` still match the recorded recovery state.
  Otherwise it keeps the competing `dev` tip or edits, the private original files and the pending recovery record, and reports the recorded revisions, the recovery paths and how to reconcile before you rerun.
- When sync takes over or displaces an existing copied dotfile, it verifies the displaced file and creates the replacement only at an empty path.
  When another writer claims the path, sync leaves it alone and prints where the moved file is.
- The original inode stays at a printed private path on the same filesystem, so an editor that still has it open can finish saving.
- For a directory copy, sync verifies the bytes and permissions of each leaf it replaces; edited and unmanaged files stay.
  When a leaf's deployed mode cannot be verified, it is updated only if its current mode can stay.
- When recovery files cannot stay on the target's filesystem, sync reports the path and keeps both versions; a checkout whose Git metadata shares the file's filesystem resumes automatic capture.
- Template and inline targets are installed by mise's own apply on the live files.
  mise 2026.11.9 can overwrite a file that another program creates between mise's planning and its installation, losing that file.
- Other targets are applied in dependency order with the usual mise hooks.
  Matching reload commands run once after the writes in that run, and a failure is reported with a next action.
- A validated pre-hook runs in the real configuration checkout, with `DOTS_ROOT` pointing there.
  Its public edits stay in that checkout for the next screened sync.
  An omitted input, a changed selector, target, source path or operation, or a newly introduced hook cannot authorize an additional write.

## The receipt

Every finished shared sync, including the one inside `dots up`, prints a receipt and records it privately for replay.
A clean run prints one line summarizing local work, incoming changes and the push.
Otherwise the receipt leads with each issue and its next action, then that summary, any recovery copies, and up to five paths per group:

- **Shared on dev**: files that differ between the previously published `dev` and the commit the remote confirmed.
- **Saved locally**: files this run committed on this machine's `dev`; after a confirmed push they can also appear under **Shared on dev**.
- **Updated in this checkout**: files that reached the configuration checkout; a later failure to deploy a dotfile is reported separately.
- **Finished pending checkout**: an interrupted update this run finished.

When stdin and stdout are terminals, a standalone sync without `--plain` may show complete small text diffs for up to two paths and twelve displayed lines.
A patch appears only when gitleaks returns a clean JSON report for both file versions and for the displayed text; screening has three seconds.
Piped output, `--plain` output and saved reports carry metadata only, never file contents.
When recording the receipt fails, sync still prints this run's receipt, warns that it could not save it for replay, and exits 0; `dots sync --last` may then show an earlier sync or none.

`dots sync --last` only replays the latest receipt for this checkout from the recorded Git revisions, without the network and without mise.
It lists every path in each group, including when piped.
On a terminal it opens a pager holding up to 1 MiB, with each file's patch only when the same screening is clean; otherwise it lists the file and counts the omitted patches.
Binary, encrypted and unverified changes are always listed as file entries.
Receipts written by releases before 1.0 are ignored.

## Issues and what to do

Each issue names what happened and the next action; start from what the receipt printed.
Issue lines carry `dots`' own diagnostics, never raw Git, mise or gitleaks output, and print paths with control characters escaped.

| Issue | What it means and what to do |
| --- | --- |
| Not shared: a secret was found | gitleaks flagged a new edit, which stays on disk and out of `dev`. Remove the secret, or add a reviewed allowlist entry to `.gitleaks.toml` or `.gitleaksignore` for a false positive, then sync again. |
| Not shared: the scan could not run | gitleaks is missing or failed, so nothing was committed or pushed and your edits stay on disk. Install or fix gitleaks and sync again. |
| Staged and unstaged edits differ | A path has different staged and unstaged versions, so sync cannot tell which one to share. Commit, unstage or discard one with Git, then sync again. |
| Not saved | Another Git command held the index or `dev`, so local edits were not recorded; incoming work may still have arrived. Sync again once that command finishes; remove a lock left by a crashed Git command by hand. |
| Not pushed: an unsafe outgoing commit | An unpushed commit on `dev` contains a finding, or once tracked a machine-local file. Rewrite that commit out of the unpublished part of `dev` by hand (no published branch contains it), then sync again. |
| Not shared: the push was rejected | `origin` refused the conditional push, for its own reason such as credentials or branch protection, or because `dev` kept moving through every retry. Local work stays saved on `dev`; check `origin` and sync again. |
| Push unconfirmed | Sync could not tell whether `origin` accepted the push, for example after an error or the 60-second limit, so it stopped before any retry, checkout or apply and counts nothing as shared. The live and replayed receipts give the candidate commit and the expected old `dev` commit, or `(absent)`. Compare `refs/heads/dev` at `origin`'s push URL with both before running sync again. |
| Published history withdrawn | `dev` or `main` on `origin` was removed or rewound during the run. Local work is kept; review `origin`'s branch history and sync again. |
| Offline | `origin` was unreachable. Local work stays saved on `dev` and nothing incoming was applied; sync again when online. |
| Invalid merged configuration | The merged configuration fails mise's checks with this machine's local files, so nothing was pushed or applied. Fix the error locally or upstream and sync again. |
| Late edit | A file changed while sync was updating files, so sync left it alone. Sync again. |
| Blocked path | Local material sits where an incoming tracked path goes. Move it somewhere safe, sync again, then compare it with what arrived. |
| Checkout pending | A checkout, index or `dev` update was interrupted, or `dev` moved during it. While the recorded state still matches, the next run finishes it first. Otherwise sync keeps the competing tip or edits and the recovery copies, and prints the recorded revisions and recovery paths; reconcile them, then sync again. |
| History not saved | A tracked entry is not `encrypt = true`, or the checkpoint failed. Fix the declaration or the reported mise error and sync again. |
| Copy kept | A deployed copy differs from what sync expected, or another writer claimed it, so sync left it in place and may print where a displaced file is. Compare the copy with its source, put the version you want in the source, then sync again. |
| Apply or reload failed | The configuration advanced but dotfiles were not applied, or a reload command failed. Fix the reported error and run the command sync prints. |
| Retired declaration removed | Sync removed exactly one retired table from a machine-local file and kept a backup at the printed path. Nothing to do unless you had customized that table. |
| Stopped or skipped | The checkout has the wrong shape or a foreign `origin`, a Git operation is in progress, another sync or publish is running, or Git is too old. Fix what the line names and sync again. |

## Exit status

`dots sync` with valid arguments always exits 0, including a missing configuration, a missing mise or gitleaks, a stopped run, a cancelled one and a receipt that could not be saved; the receipt carries the problem.
Invalid sync arguments exit 2. The help flags print usage.
Inside `dots up`, a sync problem makes the sync stage deferred: it is listed under "Needs attention", independent later stages can still run, and on its own it never makes `dots up` exit 1.
Cancellation prevents any further stage from starting.

## Requirements

- Git 2.43 or newer; sync checks this itself and skips on an older Git.
- gitleaks, for every share and push, and for patches in `dots sync --last`.
- Credentials that work without a prompt, as above.
- Git ignore rules that protect machine-local files. `dots init` writes a `.gitignore` and also provides `.gitleaks.toml` to configure the scan.

## Reviewed changes into main

Reviewed changes reach `main` through a pull request from a task branch or a JJ workspace made from `origin/main`, never from the live checkout.
To extract a change made on `dev`, create a task branch from `origin/main`, copy or cherry-pick only that change, and review its diff against `main`.
Do not move `dev` by hand or merge it wholesale into `main`.

`dots publish` is optional and works only with a GitHub repository.
It runs in a colocated JJ workspace of the configuration repository, scans the outgoing commits with gitleaks, asks for confirmation, pushes a bookmark without force and opens or updates the pull request.
It exits 3 when conflicts stop it.
