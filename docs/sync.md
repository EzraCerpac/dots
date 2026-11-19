# Sync

`dots sync`, and `dots up` before its updates, share a configuration between machines through Git.
The sections below explain when sync shares, the steps of a shared run, how to read its receipt and how to respond to each issue.
For exact rules, see [the contract](contract.md#dots-sync).

## Local-only or shared

Each run works out afresh whether the configuration is shared, from the `dots_source_repo` setting and the checkout's `origin` remote:

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
`dots apply` works locally whichever mode applies.

Once a configuration names a remote or has an `origin`, it is shared, never local-only.
If that remote is unreachable, the receipt says sync is offline, and the work waits for a later run.
Reaching an `origin` that lacks `dev` or `main` still counts as online.
In that case sync scans each local commit missing from every published branch, pushes `dev` into existence without force, and reads an absent `main` as empty.
`main` is never created or moved by sync.

### Adding a remote

As long as its seed commit is untouched, `dots init` prints the commands that connect a new, empty remote repository, and those commands push only that seed, to both `main` and `dev`.
Any other local-only repository just needs the remote; the next sync scans `dev` and pushes it:

```sh
git -C "$(dots root)" remote add origin URL
dots sync
```

### Remote URLs and credentials

The repository can be named as GitHub `OWNER/NAME`, as a GitHub URL, or as any other Git URL.
For a GitHub repository, `origin` may take any of the forms `https://github.com/OWNER/NAME`, `ssh://git@github.com/OWNER/NAME` and `git@github.com:OWNER/NAME`, each with or without `.git`.
A non-GitHub URL has to match `origin` character for character.
`dots` rejects a URL that embeds a password or token, a remote-helper URL such as `ext::`, and any value that begins with `-`.

Sync authenticates the way Git already does on the machine, through SSH keys or a configured credential helper.
Those credentials must succeed without prompting; when authentication fails, follow the action the receipt prints.
Put an authenticated repository behind `https://` or SSH rather than `http://`.
Before any network contact, `dots` applies this machine's Git URL rewrites to find the real fetch URL and every push URL, and stops if any of them points somewhere other than the expected repository.

Settings history is kept apart, in the private `dots_state_repo` repository.
`dots` will not push history to the source repository.

## dev and main

A shared configuration has two branches on `origin`.

- Every machine runs `dev`.
  Sync records local edits as commits on it and pushes them without force.
- Only reviewed pull requests change `main`.
  Each sync merges `main` into `dev`, so once a pull request is merged, every machine picks it up at its next `dots up`.

The live checkout is a plain Git checkout with `HEAD` on `dev`.
Anything in `origin/dev..dev` has not been pushed yet; nothing else counts as unpushed.
Machine-local files never enter the repository: `miserc.toml`, `config.local.toml`, `config.host-*.toml`, mise lockfiles, `locks/` and `.setup-state/`.
Sync refuses a merged configuration whose ignore rules would let any of them in.
Commits pushed straight to `origin/dev` from a task worktree are fine: the next sync merges them.

## Checkout shape

Sync works on one shape only: a plain Git checkout at the repository root, with `HEAD` on `dev` and at least one commit.

- Sync stops with one line when `HEAD` is detached or on another branch, when the root holds a `.jj` directory, and when Git's directory still holds the legacy `dots-dev.json` record described below.
  The `.jj` check is only a refusal: `dots` neither needs nor runs JJ.
- `dots bootstrap` switches a fresh clone from `main` to `dev` when it has no edits.
  Every other checkout gets a one-time conversion by hand, which keeps its edits and unpushed commits.
- Sync skips with one line during a merge, rebase, cherry-pick, revert or bisect, and while another sync holds the lock for the same repository.
  It takes over a lock whose owner died on this host.
- Sync also skips with one line on Git older than 2.43.

### Converting a legacy checkout

Releases up to v0.4.0 kept their sync state in a `dots-dev.json` record inside Git's directory.
Both sync and `dots init` recognize the record and leave it alone: sync stops, and init prints a warning pointing here.
In a linked worktree the record is not under `.git/`, so ask Git where it is; a relative answer is relative to the configuration root:

```sh
git -C "$(dots root)" rev-parse --git-path dots-dev.json
```

Convert the checkout before you run sync again:

1. Save your work first.
   Commit or copy uncommitted edits, and put any commits the remote does not have yet on a branch or in a safe place.
2. Settle the worktree and index.
   Finish or abort any Git operation and any sync an older release left half done, so the worktree and index hold only what you mean to keep.
3. Confirm the shape above: a plain Git checkout at the root, `HEAD` on `dev`, at least one commit and no `.jj` directory.
   `git -C "$(dots root)" symbolic-ref HEAD` should print `refs/heads/dev`.
4. Move the record out of Git's directory into a backup instead of deleting it, then run `dots sync`.

## One shared run

1. **Checkpoint.** Records tracked settings history, and refuses when any tracked entry lacks `encrypt = true`.
2. **Capture.** A deployed dotfile copy that changed becomes the new source, provided the source still matches the last version both sides shared.
   When both sides changed, sync keeps each as it is and reports the clash.
3. **Candidate.** Builds a candidate commit from every tracked or unignored public edit, leaving machine-local files out, without moving `dev` or the real index.
   Your staging stays as you left it.
   When a path bound for the clean candidate has a staged version that differs from its unstaged one, settle the two before that path can be shared.
4. **Scan.** Scans the candidate with gitleaks before `dev` or the index advances.
   A flagged file stays on disk and out of the candidate, and the rest is scanned again; only a clean candidate becomes `dev`.
   A rejected candidate never reaches `dev`, the real index or the push, and its edits stay on disk.
   Without a working gitleaks, or when a scan fails, nothing is shared or pushed.
5. **Fetch.** Fetches `dev` and `main`.
   An unreachable `origin` ends the run at this step, with local work kept on `dev` and nothing incoming applied.
6. **Merge.** Plans the merge of local work with `origin/dev`, then with `origin/main`, while the live checkout stays untouched.
   Where hunks conflict, `main` beats `dev` and `dev` beats local work.
   A binary, file-type or delete conflict takes the published file as a whole.
   History keeps both parents, so the losing side can always be recovered.
7. **Validate.** Checks the merged configuration against this machine's local files; an invalid result is neither pushed nor applied.
   Local tables that `dots/retired-declarations` retires are validated as already removed; see [the `dots/` reference](../src/init/README.md#retired-dotfiles-and-retired-declarations).
8. **Push.** Before each attempt, checks the outgoing commits, those that no published branch contains, for gitleaks findings and for any machine-local file they ever tracked; either blocks the push, and sync never rewrites the branch to remove it.
   It then pushes the exact validated commit to `refs/heads/dev` at the checked push URL, only to create or fast-forward the branch, and only if the remote `dev` is still the commit it expected or still absent.
   After a rejection it fetches again, and retries up to three times only when `dev` moved and no published history was withdrawn.
   If published `dev` or `main` was removed or rewound, it stops and keeps local work.
   If an attempted push has an uncertain outcome, including one stopped after 60 seconds, sync stops before any retry, checkout or apply; see [Push unconfirmed](#issues-and-what-to-do).
9. **Update.** Only after the remote has confirmed `dev`, updates the checkout and applies dotfiles under the guarded rules below.

Sync never force-pushes, skips validation or writes conflict markers into live files.

### Guarded checkout and copies

- Sync checks incoming paths for anything local in the way, ignored files, directories and symlinks included, before it writes.
  A blocked path stops the checkout, and unrelated edits can still be shared when the push is confirmed.
- `dots` guards its own checkout and file writes against intervening edits: a file edited during the run is left alone and reported with a recovery action.
- Before a fresh snapshot, sync finishes an interrupted checkout, index installation or `dev` update only while the checkout, index and `dev` still match the recorded recovery state.
  Otherwise it keeps the competing `dev` tip or edits, the private original files and the pending recovery record, and reports the recorded revisions, the recovery paths and how to reconcile before you rerun.
- What happens when the Git index changed under an interrupted checkout depends on whether that checkout had started writing.
  If it had not, nothing was written, so sync discards the pending record and reports **Not saved**; review your staging and sync again.
  If it had, sync moves the whole journal, original files included, to a private recovery directory and reports that directory with the recorded before and after revisions.
  Keep your edits, reconcile the index and checkout against those revisions and any recovery copies, then sync again.
- When sync takes over or displaces an existing copied dotfile, it verifies the displaced file and creates the replacement only at an empty path.
  When another writer claims the path, sync leaves it alone and prints where the moved file is.
- The original inode stays at a printed private path on the same filesystem, so an editor that still has it open can finish saving.
- A directory copy is assembled privately by mise first.
  To replace an existing file inside it, sync compares that file with the source last deployed at the same path:
  - Matching bytes and permissions mean the file counts as deployed, whoever wrote it, so sync updates it to the new source without listing it as kept.
  - Different bytes or verified permissions, even a difference that appears after the first comparison, leave the file in place, reported as kept.
  - An edit to the moved original that sync notices before installing the replacement goes back to the target when possible.
    When the replacement is already in place, or the original cannot go back, the edited original stays at the printed private recovery path.
  - If the deployed mode cannot be verified, sync updates the file only when its bytes match the last deployed source and its current mode can be kept.

  Local files with no deployed counterpart stay where they are.
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
Its single summary line counts local work, incoming changes and the push.
Issues, each with its next action, are printed above that line.
Below it come any recovery copies, then each path group that has entries; live output shows at most five paths per group.

- **Shared on dev**: files that differ between the previously published `dev` and the commit the remote confirmed.
- **Saved locally**: files this run committed on this machine's `dev`; after a confirmed push they can also appear under **Shared on dev**.
- **Updated in this checkout**: files that reached the configuration checkout; a later failure to deploy a dotfile is reported separately.
- **Finished pending checkout**: an interrupted update this run finished.

For example, this receipt followed an edit to `config.toml` next to a new file holding a secret.
It comes from `dots` 1.0.0-rc.1 at `rust` `9a35df0`, in a disposable home directory with a local bare `origin`:

```text
NOT SHARED: gitleaks flagged a local file; it stays on disk: "leak.toml:2" (generic-api-key)
  Remove or rotate the secret before sharing this path.
dots sync: saved 1 locally · kept 1 files on disk · pulled dev +0, main +0 · pushed dev
Shared on dev
  config.toml
Saved locally
  config.toml
```

`dots sync --last` printed the same receipt under a `Latest sync` heading.

When stdin and stdout are terminals, a standalone sync without `--plain` may show complete small text diffs for up to two paths and twelve displayed lines.
A patch appears only when gitleaks returns a clean JSON report for both file versions and for the displayed text; screening has three seconds.
Piped output, `--plain` output and saved reports carry metadata only, never file contents.
When recording the receipt fails, sync still prints this run's receipt, warns that it could not save it for replay, and exits 0; `dots sync --last` may then show an earlier sync or none.

`dots sync --last` only replays the latest receipt for this checkout from the recorded Git revisions, without the network and without mise.
When piped or run with `--plain`, the replay prints every path in every group.
On a terminal it opens a pager holding up to 1 MiB, with each file's patch only when the same screening is clean; otherwise it lists the file and counts the omitted patches.
Path listings count toward that 1 MiB as well.
Should the pager be missing or fail, the replay writes the metadata to stdout instead.
Binary, encrypted and unverified changes are listed as files, without a patch.
Receipts from v0.4.0 and earlier releases are not replayed.

## Issues and what to do

Every issue says what happened and what to do next, so begin with the receipt's own words.
Issue lines carry `dots`' own diagnostics, never raw Git, mise or gitleaks output, and print paths with control characters escaped.

| Issue | What it means and what to do |
| --- | --- |
| Not shared: a secret was found | gitleaks flagged a new edit. It stays on disk and off `dev`. Take the secret out, or for a false positive add a reviewed allowlist entry to `.gitleaks.toml` or `.gitleaksignore`, then sync again. |
| Not shared: the scan could not run | Nothing was pushed, and your edits are still on disk. When the candidate's scan passed and only the outgoing scan failed, the clean commit already sits on local `dev`. Install or repair gitleaks, then sync again. |
| Staged and unstaged edits differ | A path headed for the clean candidate has a staged version that differs from its unstaged one. Use Git to commit, unstage or discard one of them, then sync again. |
| Not saved | Another Git command held the index or `dev`, so local edits were not recorded; incoming work may still have arrived. Sync again once that command finishes; remove a lock left by a crashed Git command by hand. The same issue appears when the index changed before an interrupted checkout started writing; review your staging and sync again. |
| Not pushed: an unsafe outgoing commit | An unpushed commit on `dev` contains a finding, or once tracked a machine-local file. Rewrite that commit out of the unpublished part of `dev` by hand (no published branch contains it), then sync again. |
| Not shared: the push was rejected | `origin` refused the conditional push, for its own reason such as credentials or branch protection, or because `dev` kept moving through every retry. Local work stays saved on `dev`; check `origin` and sync again. |
| Push unconfirmed | Sync could not tell whether `origin` accepted the push, for example after an error or the 60-second limit, so it stopped before any retry, checkout or apply and counts nothing as shared. The live and replayed receipts give the candidate commit and the expected old `dev` commit, or `(absent)`. Compare `refs/heads/dev` at `origin`'s push URL with both before running sync again. |
| Published history withdrawn | `dev` or `main` on `origin` was removed or rewound during the run. Local work is kept; review `origin`'s branch history and sync again. |
| Offline | `origin` was unreachable. Local work stays saved on `dev` and nothing incoming was applied; sync again when online. |
| Invalid merged configuration | Combined with this machine's local files, the merged configuration fails mise's checks, so nothing was pushed or applied. Fix the error here or upstream, then sync again. |
| Late edit | A file changed while sync was writing, so sync did not touch it. Sync again. |
| Blocked path | Local material sits where an incoming tracked path goes. Move it somewhere safe, sync again, then compare it with what arrived. |
| Checkout pending | A checkout, index or `dev` update was interrupted, or `dev` moved during it. While the recorded state still matches, the next run finishes it first. Otherwise sync keeps the competing tip or edits and the recovery copies, and prints the recorded revisions and recovery paths; reconcile them, then sync again. |
| Guarded checkout parked | The index changed after an interrupted checkout had started writing, so sync moved its journal and original files to the printed private directory and gave the recorded before and after revisions. Keep your edits, reconcile the index and checkout with those revisions and recovery copies, then sync again. |
| History not saved | A tracked entry lacks `encrypt = true`, or the checkpoint failed. Correct the declaration or the mise error shown, then sync again. |
| Copy kept | A deployed copy differs from what sync expected, or another writer claimed it, so sync left it in place and may print where a displaced file is. Compare the copy with its source, put the version you want in the source, then sync again. |
| Apply or reload failed | The configuration moved forward, but dotfiles were not applied or a reload command failed. Fix the error shown and run the command sync prints. |
| Retired declaration removed | Before removing the matching retired tables, sync saved the original machine-local file at the printed recovery path. Nothing is needed unless you had customized that table. |
| Stopped or skipped | The checkout has the wrong shape or a foreign `origin`, a Git operation is under way, another sync holds the lock, or Git is too old. Fix whatever the line names, then sync again. |

## Exit status

`dots sync` with valid arguments always exits 0, including a missing configuration, a missing mise or gitleaks, a stopped run, a cancelled one and a receipt that could not be saved; the receipt carries the problem.
Invalid arguments make it exit 2, and `--help` or `-h` prints its usage.
Within `dots up`, any sync problem defers the sync stage instead.
The stage appears under "Needs attention", later stages that do not depend on it still run, and the deferral alone never makes `dots up` exit 1.
After cancellation, no further stage starts.

## Requirements

- Git 2.43 or later; sync checks the version and skips when it is older.
- gitleaks, for every share and push, and for patches in `dots sync --last`.
- Credentials that succeed without a prompt, as described above.
- Git ignore rules that keep machine-local files out.
  `dots init` writes a `.gitignore`, along with a `.gitleaks.toml` that configures the scan.

## Reviewed changes into main

Reviewed changes reach `main` through a pull request from an ordinary task branch made from `origin/main`, never from the live checkout.
To extract a change made on `dev`, create a task branch from `origin/main`, copy or cherry-pick only that change, and review its diff against `main`.
Do not move `dev` by hand or merge it wholesale into `main`.
Opening and merging that pull request happens in your usual Git and code-review tools; `dots` does not take part.
