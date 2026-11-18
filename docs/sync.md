# Sync

`dots sync`, and `dots up` before its updates, share a configuration between machines through Git.
This guide covers when sync shares, how a run works, what its receipt means and what to do about each issue.
[The contract](contract.md#dots-sync) is the precise reference.

> TODO(P-08, P-09): Check the receipt groups and issue descriptions here against the integrated sync and receipt output, then delete this line.

## Local-only or shared

`dots` decides at each run whether the configuration is shared:

| `dots_source_repo` | `origin` | Mode |
| --- | --- | --- |
| unset | absent | Local-only |
| unset | present | Shared through `origin` |
| set | present and matching | Shared through `origin` |
| set | absent | Diagnostic: the configured remote is missing |
| set | present, not matching | Diagnostic: `origin` is not the configured repository |

A local-only `dots sync` prints a one-line receipt saying how to add a remote, and skips its whole pipeline: it saves no history, captures no copies, and makes no network call, scan or commit.
`dots up` skips its sync stage with one line, still runs its own checkpoints, and needs no attention for it.
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
3. **Candidate.** Stages every tracked or unignored public edit as a candidate commit, leaving machine-local files out, without moving `dev` or the index.
4. **Scan.** Scans the candidate with gitleaks before `dev` or the index advances.
   A flagged file stays on disk and out of the candidate, and the rest is scanned again; only a clean candidate becomes `dev`.
   Without a working gitleaks, or when a scan fails, the candidate is dropped, the edits stay on disk, and nothing is shared or pushed.
   A flagged commit already in local history that no published branch contains, or one that ever tracked a machine-local file, blocks the push.
5. **Fetch.** Fetches `dev` and `main`, or continues offline.
6. **Merge.** Merges local work with `origin/dev`, then with `origin/main`, outside the live files.
   On a conflicting hunk, `main` wins over `dev` and `dev` wins over local work.
   Binary, file-type and delete conflicts take the published file whole.
   Both parents stay in history, so the losing version is recoverable.
7. **Validate.** Validates the merged configuration with this machine's local files; an invalid result is neither pushed nor applied.
   Local tables that `dots/retired-declarations` retires are validated as already removed; see [the `dots/` reference](../src/init/README.md#retired-dotfiles-and-retired-declarations).
8. **Push.** Creates or fast-forwards `origin/dev` only, conditional on the remote tip it validated, and retries a lost race up to three times.
   If published history disappears during a retry, sync keeps local work and applies nothing from the withdrawn history.
   A push that takes longer than 60 seconds is stopped.
9. **Update.** Updates the checkout and applies dotfiles under the guarded rules below.

Sync never force-pushes, skips validation, discards uncommitted edits or writes conflict markers into live files.

### Guarded checkout and copies

- Before writing, sync checks incoming paths for local obstructions, including ignored files, directories and symlinks.
  A blocked path stops the checkout, and unrelated edits are still shared.
- A file edited during the run is never overwritten; sync reports it and asks for a rerun.
- An interrupted checkout, index installation or branch update is finished by the next run before it takes a new snapshot.
- A regular copied dotfile is replaced only after moving the current target aside and confirming its bytes are the expected ones, and the replacement is created only at an empty path.
  When another writer claims the path, sync leaves it alone and prints where the moved file is.
- After a replacement, the previous file stays at a printed private path, so an editor that still has it open can finish saving.
- For a directory copy, sync verifies the bytes and permissions of each leaf it replaces; edited and unmanaged files stay.
  When a leaf's deployed mode cannot be verified, it is updated only if its current mode can stay.
- Recovery files stay on the same filesystem as the target.
  When that is impossible, sync reports the path and keeps both versions; a checkout whose Git metadata shares the file's filesystem resumes automatic capture.
- Other targets are applied in dependency order with the usual mise hooks, and matching reload commands run once, after the writes.

## The receipt

Every finished sync, including the one inside `dots up`, prints a receipt and records it privately for replay.
A clean run prints one line summarizing local work, incoming changes and the push.
Otherwise the receipt leads with any issue and its next action, then lists up to five paths per group:

- **Shared**: changes the remote accepted on `dev`, including work saved on an earlier offline run.
- **Saved locally**: edits committed on this machine's `dev` but not pushed.
- **Updated in this checkout**: files that reached the configuration checkout; a later failure to deploy a dotfile is reported separately.
- **Finished after an interruption**: an update the previous run left pending.

For live and replayed patches, gitleaks must clear both file versions and the displayed text.
When stdin and stdout are terminals, standalone sync may show a complete small text diff for up to two paths and twelve displayed lines.
Piped output, `--plain` output and saved reports carry metadata only, never file contents.
When recording the receipt fails, sync still prints it and warns that it could not save it for replay.

`dots sync --last` replays the latest receipt for this checkout from the recorded Git revisions, without the network and without mise.
It shows a patch only when stdin and stdout are terminals and gitleaks returns a clean report for both file versions and for the displayed text; otherwise it lists the file without its patch.
Binary, encrypted and unverified changes are always listed as file entries.
Receipts written by releases before 1.0 are ignored.

## Issues and what to do

Each issue names what happened and the next action; start from what the receipt printed.
`dots --verbose sync` also shows the underlying Git and mise diagnostics.

| Issue | What it means and what to do |
| --- | --- |
| Not shared: a secret was found | gitleaks flagged a new edit, which stays on disk and out of `dev`. Remove the secret, or add a reviewed allowlist entry to `.gitleaks.toml` or `.gitleaksignore` for a false positive, then sync again. |
| Not shared: the scan could not run | gitleaks is missing or failed, so nothing was committed or pushed and your edits stay on disk. Install or fix gitleaks and sync again. |
| Not saved | Another Git command held the index or `dev`, so local edits were not recorded; incoming work may still have arrived. Sync again once that command finishes; remove a lock left by a crashed Git command by hand. |
| Not pushed: an unsafe outgoing commit | An unpushed commit on `dev` contains a finding, or once tracked a machine-local file. Rewrite that commit out of `origin/dev..dev` by hand (it was never published), then sync again. |
| Not pushed: `origin/dev` kept moving | Three pushes in a row lost a race. Nothing to do; the next run retries. |
| Not pushed: the push failed or timed out | Git's own error, such as rejected credentials or a protected branch, or a credential helper waiting for input. Local work is committed on `dev`; fix the cause and sync again. |
| Offline | `origin` was unreachable. Check the receipt for what was saved or applied locally. Retry when online so sync can scan, validate and push eligible work. |
| Invalid merged configuration | The merged configuration fails mise's checks with this machine's local files, so nothing was pushed or applied. Fix the error locally or upstream and sync again. |
| Late edit | A file changed while sync was updating files, so sync left it alone. Sync again. |
| Blocked path | Local material sits where an incoming tracked path goes. Move it somewhere safe, sync again, then compare it with what arrived. |
| Checkout pending | The checkout, index or branch update was interrupted. Fix the reported error and sync again; the next run finishes it first. |
| History not saved | A tracked entry is not `encrypt = true`, or the checkpoint failed. Fix the declaration or the reported mise error and sync again. |
| Copy kept | A deployed copy differs from what sync expected, or another writer claimed it, so sync left it in place and may print where a displaced file is. Compare the copy with its source, put the version you want in the source, then sync again. |
| Apply or reload failed | The configuration advanced but dotfiles were not applied, or a reload command failed. Fix the reported error and run the command sync prints. |
| Retired declaration removed | Sync removed exactly one retired table from a machine-local file and kept a backup at the printed path. Nothing to do unless you had customized that table. |
| Stopped or skipped | The checkout has the wrong shape or a foreign `origin`, a Git operation is in progress, another sync or publish is running, or Git is too old. Fix what the line names and sync again. |

## Exit status

`dots sync` with valid arguments always exits 0, including a missing configuration, a missing mise or gitleaks, a stopped run and a cancelled one; the receipt carries the problem.
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
