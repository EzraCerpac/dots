# dots 1.0 contract

Status: Frozen for 1.0. Changes go through the coordinator as amendments to this document.
Amended 2026-11-18: shell completion, removal targets, the output of individual commands, cancellation, shared agent skill directory creation, the 1.0 guarantee for the running `dots` on children's `PATH`, file adoption and removal, software version syntax, status program versions, history enrollment and restore, retired declarations, and publish validation.
Amended 2026-11-18 for the terminal output redesign: `dots up` writes ordinary scrollback, hooks and recipes run in the foreground, `dots reports` prints a list, and bare `dots` prints help.
Amended 2026-11-18: restore undo checkpoints, package pass scope, and publish origin and `dev` ancestry checks.
Amended 2026-11-18: report listing and exit status, the init seed commit message, GitHub app ownership, pull request bases, update output modes, scan policy and archive coverage, lockfile protection, and local versus published settings history.

This document is the reference for what `dots` 1.0 does as seen from outside: commands, files, settings, environment, output and exit codes.
Every statement describes 1.0.
**New in 1.0** marks behaviour that 0.3.5 does not have.
**Changed from 0.3.5** marks behaviour that differs from 0.3.5 and says how.
**Changed from 0.4.0** marks behaviour that 0.4.0 ships and 1.0 changes, and says how.
**New in 0.4.0** and **Changed in 0.4.0** mark such a difference that 0.4.0 already ships; the other labels mark changes that arrive with 1.0.
Where 0.3.5 files are cited, as `bin/dots:233`, they are historical evidence for such a label, not a specification.
[Internal and subject to change](#internal-and-subject-to-change) lists what an implementation may choose freely.
Messages quoted in this document show the content a line carries; their exact wording is not part of the contract unless stated.
Examples use generic names: profiles `laptop` and `server`, a machine `my-laptop`, repositories `OWNER/NAME`.

## Platforms and programs

`dots` runs on macOS and Linux.
Linux under WSL is supported; native Windows is not.
`dots` is one self-contained executable.
**Changed from 0.3.5:** it needs no Bash, Node.js, uv or jq, and no support directory next to it.

| Program | Needed by |
| --- | --- |
| mise 2026.11.8 or newer | Every command that reads the configuration through mise |
| Git | `init`, `sync`, the sync stage of `up`, `backup`, `restore`, `bootstrap`, `publish`, `add FILE` (**New in 1.0**); sync needs Git 2.43 or newer |
| gitleaks | Sharing in `sync` and `up`; `publish`; showing patches in `sync --last` |
| age, age-keygen | `bundle`, a bootstrap that uses a bundle, history enrollment, `backup`, `restore` |
| ssh | `remote` |
| `gh` | `publish`; the optional remote offer in `init` |
| JJ | `publish` |
| Homebrew | Declarations that select `brew:` or `brew-cask:`, and the `dots brew` helpers, on macOS |
| apt, dnf, pacman, apk, mas | Declarations that select that manager |

Native package managers stay external programs that mise and `dots` call.
A configuration's hooks, commands and recipes need whatever their own interpreters are, for example Bash for a Bash script; `dots` does not.
A command checks only the programs that the requested action uses.
A missing program fails that action with one line naming the program and the action, and exit status 1.
A missing mise fails with exit status 2.
A missing optional program never fails an inspection command or a stage that has nothing to do.

## Configuration root

The configuration is a mise config directory.
`dots` selects it in this order:

1. `DOTS_ROOT`, when set.
2. `MISE_CONFIG_DIR`, when set.
3. `${XDG_CONFIG_HOME:-$HOME/.config}/mise`.

`dots` resolves the result to its physical path and exports it to every child as `DOTS_ROOT`.
A command that needs the configuration stops with exit status 2 and a line naming the path when `config.toml` does not exist there.
The exceptions are bare `dots`, `init`, `help`, `--version`, `completions`, `bundle`, `brew`, `install-release-app`, `status` (which prints its program list first), and `sync` (which always exits 0).
Every other built-in command, helper subcommand and configuration command needs the configuration.
mise children run rooted in the configuration, with `MISE_CONFIG_DIR` set to the root.
**Changed from 0.3.5:** every command except `dots bootstrap persist-selected` ignores the caller's `MISE_ENV`, so a project's environment selector never leaks in; the machine's own selection comes from its persisted files.

### Files in the root

| Path | Purpose | In the repository |
| --- | --- | --- |
| `config.toml` | Base mise configuration, always loaded; its `[vars]` holds the `dots` settings | yes |
| `config.PROFILE.toml` | Additions for one profile | yes |
| `config.host-ID.toml` | Optional additions for one machine, loaded with its `host-ID` environment | no |
| `dots/` | Hooks, commands, installer exceptions, retired dotfiles and retired declarations | yes |
| `dotfiles/` | Dotfile sources; `dots add FILE` puts sources here | yes |
| `.gitleaks.toml`, `.gitleaksignore` | Secret-scan rules for `sync` and `publish` | yes |
| `.gitignore` | Ignores the machine-local files below | yes |
| `encrypted/bootstrap.PROFILE.json.age` | That profile's encrypted bootstrap bundle | yes |
| `tests/run` | The configuration's own tests | yes |
| `miserc.toml` | This machine's mise environment selector, mode 0600 | no |
| `config.local.toml` | This machine's profile, machine ID, history and identity settings, mode 0600 | no |
| `mise.lock`, `mise.*.lock`, `locks/`, `.setup-state/` | Files mise and older setups write per machine | no |

`dots sync` never shares a machine-local file, and refuses a merged configuration whose ignore rules would expose one.
Root `mise.lock` and `mise.*.lock` files always stay out of sharing.
For lockfiles, incoming ignore rules need only protect those present at the root.
Incoming rules must still protect future `config.host-*.toml` selectors.

### Settings in `[vars]`

mise `[vars]` values are strings, so a list is one string of space-separated words.
`dots` reads these settings from `ROOT/config.toml` only:

| Setting | Meaning |
| --- | --- |
| `dots_profiles` | Required. The profiles this configuration selects, for example `"laptop server"` |
| `dots_source_repo` | Optional. Pins the configuration's own repository; see [Remotes](#remotes) |
| `dots_state_repo` | Optional. The private repository for settings history; see [Settings history](#settings-history) |
| `dots_self_updating_casks` | Optional. Casks that `dots up` only installs when missing |
| `dots_publish_on_behalf_of` | Optional. A person's name; `dots publish` then notes that a model wrote the pull request text |

**Changed from 0.3.5:** both repository settings accept a Git URL; 0.3.5 accepted only `OWNER/NAME`.
**Changed from 0.3.5:** `dots_atuin_username` is not read.
A profile name matches `^[a-z0-9][a-z0-9-]*$` and is not `base`, `local` or a name starting with `host-`.
`dots` refuses any profile not listed in `dots_profiles`, in commands, hook suffixes and installer exception rows alike.
A missing, empty or invalid `dots_profiles` stops the command with exit status 2 and a line naming the setting and the file.

## Profiles and machine identity

A profile is a mise environment: `config.laptop.toml` applies when the machine selects `laptop`.
A machine ID matches `^[a-z0-9][a-z0-9-]*$`.
The machine's own mise environment is `host-ID`; it loads `config.host-ID.toml` and names the machine's stream in settings history.
A machine persists its selection in two ignored files, both mode 0600:

- `miserc.toml` selects `PROFILE` and `host-ID`; other selectors already there are kept after them.
- `config.local.toml` records the profile as `env.SETUP_PROFILE`, the machine ID as `env.SETUP_MACHINE_ID`, and `settings.history.sync = "manual"`; with a history origin it also records that origin and the branch `host-ID`.

Both files are validated with mise before either changes, and each is replaced atomically.
A `miserc.toml` that selects two profiles or two `host-` IDs is ambiguous, and so is a pair of files that name a different profile or machine ID, for example after an interrupted write.
`dots` never chooses between them: every command that reads the selection stops with exit status 2 and names `dots profile --profile PROFILE --machine-id ID` as the fix.
That command is the one exception: it replaces every profile and `host-` selector with the given pair, keeps the other selectors, and rewrites both files' profile and machine ID, under the same validation and atomic replacement.
Commands take the profile and machine ID from `SETUP_PROFILE` and `SETUP_MACHINE_ID` in the environment when set, else from the persisted selection.

## First run

| Behaviour | 0.3.5 | 1.0 |
| --- | --- | --- |
| Configuration files | `dots init` writes `[vars] dots_profiles`, the `dots/` layout, `.gitleaks.toml` and `.gitignore`, never overwriting (`tasks/init:57-107`) | The same files from templates embedded in the executable, never overwriting |
| Profile and machine | A separate `dots internal profile` command (`README.md:99-103`) | `dots init` selects them; machine ID from the hostname |
| Git repository | Made by hand with `main` and `dev` | `dots init` creates it with one seed commit on `main` and `dev`, `HEAD` on `dev` |
| Remote | Made by hand | Optional; `init` offers to create one in a terminal with `gh`, else prints two commands |
| Remote kinds | GitHub only (`lib/source-repo.mjs:30-48`) | GitHub, or any Git URL |
| `dots_source_repo` | Required by `sync`, the sync stage of `up`, `publish` and `remote` | Optional; `origin` alone suffices |
| `dots up` without a remote | The sync stage stops with "missing setting: add dots_source_repo" | The sync stage is skipped with one line |
| `dots sync` without a remote | Exits 0 with a stop line | Exits 0 with a one-line receipt saying how to add a remote |
| `dots add` | Software only | Software, or an existing file |
| `dots remove ./FILE` | Treated as a software name that matches no declaration | Restores an adopted file |
| A missing program | Often the child's own error, such as a failed `mise exec` | One line naming the program and the action |
| `dots status` | No program list (`tasks/status:15-45`) | Lists versions, reports unreadable optional versions as `version unavailable`, and lists missing programs with the commands that need them |
| Internal commands in the quickstart | Required | None |

The local quickstart needs only mise and Git installed:

```sh
dots init
dots up
```

On a fresh configuration, `dots up` skips the checkpoint because nothing is tracked, skips the sync stage with one line because there is no remote, and finds no packages, exceptions or hooks.
When its ordinary update stages succeed, including the standalone mise update, which needs the network, it exits 0 with nothing under "Needs attention".
The skipped sync stage makes no network call and is never a reason for attention or failure; a failure of another stage is reported and makes `dots up` exit 1 as on any machine.

### `dots init [DIR]`

`DIR` defaults to the configuration root, and `init` creates it when missing.
Each output line names an item and whether `init` created, added or kept it.
Running `init` again changes nothing it made before.
Every command `init` prints names `DIR` quoted for the shell.
When `DIR` is not the configuration root that `dots` would select, `init` says so and prints each later `dots` command with `DOTS_ROOT` set, for example `DOTS_ROOT='/path/to/dotfiles' dots up`.

Git shape comes first.
`init` inspects the repository around `DIR` before it writes anything:

- `DIR` inside another repository, but not its root: `init` stops with exit status 2 and writes nothing.
- `DIR` is the root of a repository with commits, or with a non-empty index: an existing repository; see step 6 below.
- `DIR` is no repository, or the root of one with no commits and an empty index: a new repository.

Then, in order:

1. **Files.** Without `config.toml`, `init` writes one with `[vars] dots_profiles = "default"` and commented hints for the optional settings.
   With a `config.toml` that lacks `dots_profiles`, it adds `dots_profiles = "default"` and keeps every other byte.
   It creates `dots/` with `hooks/update.d/`, `hooks/bootstrap.d/`, `commands/` and `exceptions/`, and writes `dots/README.md`, `dots/exceptions.tsv`, `dots/retired-dotfiles`, `dots/retired-declarations`, `.gitleaks.toml` and `.gitignore`, each only when absent.
2. **Profile.** A valid persisted profile is kept.
   Otherwise `init` selects `default` when `dots_profiles` lists it, else the first listed profile.
3. **Machine ID.** A valid persisted machine ID is kept.
   Otherwise `init` takes the hostname up to its first `.`, lowercases it, replaces each run of characters outside `a-z0-9` with one `-`, and trims `-` from both ends.
4. When the hostname is unavailable, the result is empty, or the persisted selection is ambiguous, `init` stops with exit status 2.
   It keeps the files it wrote and prints `dots profile --profile PROFILE --machine-id ID` and `dots init DIR`, both aimed at `DIR`; the second run finishes the remaining steps.
5. **Selection.** `init` persists the profile and machine ID as `dots profile` does.
6. **Repository.** For a new repository, `init` creates the seed commit described below.
   For an existing repository, `init` commits nothing, stages nothing, keeps every branch and all history, and reports each problem that sync would stop on (see [Checkout shape](#checkout-shape)), with its fix.
   When `HEAD` is on `main` with commits and no uncommitted changes, and neither a local `dev` nor an `origin/dev` exists, the fix it prints is `git -C DIR switch -c dev`; otherwise it prints no branch command.
7. **Remote.** See below.

`init` exits 0 when every step it attempted succeeded, apart from the optional remote step.

The seed commit:

- It is a root commit, with no parents.
- It contains exactly the files at embedded template paths that are regular files, not ignored by the repository or the user, whose bytes and file mode equal the embedded template, whichever run of `init` wrote them.
- Every other file stays out of it and on disk as it is, including a `config.toml` that differs from its template because it existed before `init` or was edited since.
- Without a configured Git identity, `init` supplies a synthetic author for that one commit only, on the command line; it never writes Git configuration.
- `main` and `dev` both point at it, and `HEAD` is on `dev`.
- Its commit message is exactly `Initialize dots configuration` followed by a single LF.

The seed is **pristine** while all of these hold:

- `main` and `dev` exist and point at the same commit, which has no parents, so no unrelated commit is part of it.
- Its tree contains only zero or more embedded template files, each at its template path with the template's bytes and file mode; any subset of the templates, including none, qualifies.
- `HEAD` is on `dev`, there is no `origin`, and nothing has been committed since.
- **New in 1.0:** the root commit message must match the seed message byte for byte for a scan-free seed push, automatic or printed.
  A different message makes the seed ineligible, and `init` never prints the rejected message.

The remote step:

1. `init` offers to create a remote only in the run that created the seed, and only when the seed is pristine, stdin and stdout are terminals, `gh` is on `PATH`, and `gh` reports a signed-in account.
2. The offer is a private GitHub repository named `dotfiles`; the default answer is no.
3. On yes, `init` creates the repository, adds it as `origin`, and pushes the verified seed without force, as below.
4. When any part fails, `init` keeps the local configuration, reports what did and did not happen, and prints only the remaining commands; when `origin` was already added, that is only the push of the verified seed.
   It still exits 0.
5. A noninteractive run never creates a remote, and a later run never asks again.
6. In every run where the seed is still pristine and no remote was created, `init` prints the two commands for a new, empty remote repository:

   ```sh
   git -C DIR remote add origin URL
   git -C DIR push origin SEED_OID:refs/heads/main SEED_OID:refs/heads/dev
   ```

   Every seed push, automatic or printed, uses the full commit ID of the seed, captured after verifying that it is pristine, as each source, and fully qualified branch names as destinations.
   A printed command shows that ID literally in place of `SEED_OID`, never a branch name that could move later.
7. It says that these publish only the seed, and that later changes are shared with `dots sync`, which scans them first.
8. For any other repository without `origin`, it prints only `git -C DIR remote add origin URL`, followed by `dots sync` on `dev`, which scans every local commit before it pushes `dev`.
   It never prints a push for such a repository, so a new remote is never seeded with unscanned files, and `main` on the remote stays as the user leaves it.

### Remotes

`dots` decides the source mode at each run:

| `dots_source_repo` | `origin` | Mode |
| --- | --- | --- |
| unset | absent | Local-only |
| unset | present | Shared through `origin` |
| set | present and matching | Shared through `origin` |
| set | absent | Diagnostic: the configured remote is missing |
| set | present, not matching | Diagnostic: `origin` is not the configured repository |

In local-only mode, the sync stage of `dots up` is skipped with one line, and `dots sync` prints a one-line receipt saying how to add a remote, then exits 0.
Local-only `dots sync` skips its whole pipeline: it saves no history, captures no copies, makes no network call, scan or commit, and applies nothing.
`dots apply` applies declarations locally, and `dots up` still runs its own checkpoints.
A configuration that names a remote, or has an `origin`, is never local-only: an unreachable remote is reported as offline and work waits for the next run.
A reachable `origin` without `dev` or `main` is online, not offline.
Sync then scans every local commit that no published branch contains, creates `dev` with an ordinary push, and treats a missing `main` as empty; sync never creates or moves `main`.

A repository setting is GitHub `OWNER/NAME`, a GitHub URL, or another Git URL.

- `OWNER/NAME` matches `^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$`.
- A GitHub URL is any of `https://github.com/OWNER/NAME`, `ssh://git@github.com/OWNER/NAME` and `git@github.com:OWNER/NAME`, each with or without `.git`; as a setting it means the same as `OWNER/NAME`.
- A GitHub repository matches an `origin` spelled as any of those six URLs.
  **Changed from 0.3.5:** the source repository accepted five `origin` spellings, without `ssh://git@github.com/OWNER/NAME`.
- Any other Git URL matches only an `origin` with exactly the same string.
  The comparison is literal: `dots` does not recognize another spelling or alias of the same repository.
- Accepted URL forms are `ssh://`, scp-like `[user@]host:path`, `git://`, `http://`, `https://` and `file://`.
- `dots` refuses a URL that carries a password or token, a `TRANSPORT::ADDRESS` remote-helper form such as `ext::`, and a value starting with `-`.
- Credentials come from the user's normal Git setup: SSH keys, or a configured credential helper.
  `dots` adds no protection to Git's own transport, so a repository that needs authentication belongs on `https://` or SSH, not `http://`.

`dots` passes a URL to Git as one argument and never through a shell.
Before it fetches, clones, adopts or pushes, `dots` resolves the effective fetch URL and every effective push URL after Git's URL rewrites on this machine, and stops before contacting the remote unless each one matches the expected repository.
For the source repository that is `dots_source_repo` when set, else the `origin` URL as configured; for history it is `dots_state_repo`.
No URL that `dots` prints, logs or stores carries credentials.
The source and state repositories are independent, and must not resolve to the same repository; `dots` refuses a history push whose destination is the source repository.
For URLs other than GitHub's, that check is the same literal comparison, so an alias of the source repository is not detected.
A local-only configuration may keep settings history in a state repository.
`dots publish` works only with a GitHub repository.

## Commands

### Global syntax

```text
dots [--plain | --verbose] COMMAND [ARGS...]
```

`--plain` prints concise progress lines.
`--verbose` shows full child output.
`--plain` with `--verbose` exits 2.
**Changed from 0.4.0:** `--no-wait` is gone and exits 2 as an unknown option.
For commands other than `dots up`, these flags neither wrap the output nor wait for the user, as [Output and reports](#output-and-reports) describes.
These flags apply only to built-in commands; before a configuration command they exit 2.
Without a terminal on stdout, output is plain by default.
`dots COMMAND -h` and `dots COMMAND --help` print a built-in command's usage and exit 0.
Configuration commands, `dots test` and `dots mise` pass every argument, `--help` included, to their child unchanged.
**Changed from 0.3.5:** `dots test --help` reaches `tests/run`.
An unknown command or option exits 2 with one line pointing to `dots help`.

### Effects

| Mark | The command can change |
| --- | --- |
| R/O | Nothing below |
| M | The machine: installed software, files in `HOME`, services |
| C | The configuration checkout (files, index, local branches) or its machine-local files |
| R | A remote repository: a push, a new repository or a pull request |
| H | Another host, over SSH |
| D | Whatever the child it runs changes |

A mark in parentheses applies only under the condition given.
Every invocation may also refresh the agent skill links ([Agent skill](#agent-skill)), except `dots completions` and shell completion.
Commands marked "report" save a private report.
Read-only commands never install tools.

### Inventory

| Command | Effects | Notes |
| --- | --- | --- |
| `dots` | R/O | **Changed from 0.4.0:** the help text and exit 0, in a terminal too |
| `dots help`, `-h`, `--help` | R/O | Built-in commands, the helpers, then the configuration's commands |
| `dots --version` | R/O | Version and install method |
| `dots reports [COMMAND]` | R/O | **Changed from 0.4.0:** prints the same newest-first listing to a terminal, a pipe or a redirected file. Exits 0 for a valid listing and 2 for an invalid filter or usage |
| `dots completions bash\|zsh\|fish` | R/O | **New in 0.4.0.** Prints a completion script |
| `dots status` | R/O, report | Programs, profile, source, packages, tools, dotfiles, history, exceptions |
| `dots plan` | R/O, report | mise's bootstrap plan for this profile and machine; **New in 1.0:** the report |
| `dots sync --last` | R/O | Replays the latest sync receipt |
| `dots up [--mas]`, `dots update` | M, C, (R: pushes `dev` when shared), (D: hooks and recipes), report | [`dots up`](#dots-up) |
| `dots up --last` | R/O | **Changed from 0.4.0:** prints the last `dots up` report and exits with its recorded status |
| `dots sync` | C, M, (R: pushes `dev` when shared), report | [`dots sync`](#dots-sync) |
| `dots apply [TARGET...]` | M, report | Applies declared dotfiles and templates |
| `dots test [ARGS...]` | D, report | Runs `tests/run` |
| `dots add [SELECTOR] [--asset GLOB] SOFTWARE...` | M, C, report | Installs, then records |
| `dots add [SELECTOR] FILE...` | M, C, report | **New in 1.0.** Adopts a dotfile |
| `dots remove`, `dots uninstall` `[SELECTOR] [--dry-run] [--keep-installed] TARGET...` | M, C, report | `--dry-run`: R/O; `--keep-installed`: C only |
| `dots init [DIR]` | C, (R: an accepted offer creates a GitHub repository and pushes the seed) | [`dots init`](#dots-init-dir) |
| `dots bootstrap [OPTIONS] [MISE-ARGS...]` | M, C, (D: mise hooks, bootstrap hooks, recipes), report | [`dots bootstrap`](#dots-bootstrap) |
| `dots bootstrap persist-selected` | C | Callback for a `pre-dotfiles` hook |
| `dots bootstrap complete` | M, C, D | Callback for the final `bootstrap` task |
| `dots remote HOST --profile P [OPTIONS]` | H, report | `--dry-run`: R/O |
| `dots bundle create OPTIONS` | M, (C when the output is inside the configuration), report | Writes one encrypted file |
| `dots bundle check OPTIONS` | R/O, report | Decrypts and validates in memory |
| `dots backup` | M (history store), R (state repository), report | [Settings history](#settings-history) |
| `dots restore [--initialize-history]` | M (history store, tracked files), C (machine-local files), report | Fetches; never pushes |
| `dots publish [--all \| --bookmark NAME]` | C (JJ bookmarks), R (push and pull request), report | [`dots publish`](#dots-publish) |
| `dots NAME [ARGS...]` | D | A configuration command |
| `dots root` | R/O | **New in 1.0.** Prints the configuration root |
| `dots profile` | R/O | **New in 1.0.** Prints the selected profile |
| `dots profile --profile P --machine-id ID [--origin URL]` | C | Persists a profile and machine |
| `dots mise ARGS...` | D | **New in 1.0.** mise rooted in the configuration |
| `dots retire-dotfiles` | M | **New in 1.0.** Deletes retired dangling links |
| `dots exceptions` | M, D | Installs missing installer exceptions |
| `dots enroll-history OPTIONS` | M (file modes), C (machine-local files) | Prepares encrypted history |
| `dots install-release-app OWNER/REPO` | M (`/Applications`) | **New in 1.0** as a public name; mise post-install callback |
| `dots brew path`, `version`, `admin-available`, `hold-formula`, `hold-cask` | R/O | **New in 1.0** as commands |
| `dots brew defer-for-admin TOKEN` | R/O | **New in 1.0.** Exits 3 |
| `dots brew record ...` | Appends to the `DOTS_CHANGES` file | **New in 1.0** |
| `dots brew update-cask ...`, `update-formula ...` | M (that one Homebrew package), appends to `DOTS_CHANGES` | **New in 1.0** as commands |

`update` is an alias of `up`, and `uninstall` of `remove`.
Built-in names win over configuration commands of the same name.
**Changed from 0.3.5:** `root`, `profile`, `mise`, `retire-dotfiles`, `exceptions`, `enroll-history`, `install-release-app` and `brew` are new built-in names, so a configuration command with one of these names is no longer reachable.
**Changed in 0.4.0:** `completions` is a built-in name, with the same effect.
**Changed from 0.4.0:** `menu` is no longer a built-in name, and no alias is reserved for it.
An executable configuration command named `menu` runs and completes under the ordinary rules of [Configuration commands](#configuration-commands).
Without one, `dots menu` is an unknown command and exits 2.

### Exit status

| Status | Meaning |
| --- | --- |
| 0 | Success, including a run whose only problems are deferred or skipped stages |
| 1 | A failure, including a missing program for the requested action |
| 2 | A usage or configuration error: unknown command or option, invalid arguments, missing configuration, invalid `[vars]`, missing mise |
| 3 | Deferred: `dots publish` stopped by conflicts; `dots enroll-history` with no tracked file for this machine, or with no recipient other than this machine's own key; `dots brew defer-for-admin`; a deferral in `dots brew update-cask` |
| 4 | Skipped: a `dots brew` install, update or hold command on a system other than macOS |

Configuration commands, `dots test` and `dots mise` exit with their child's status.
`dots up` and `dots bootstrap` exit 0 when every stage completed, was skipped or was deferred, and 1 when any stage failed.
`dots sync` with valid arguments exits 0 every time, including a missing configuration, a missing mise or gitleaks, and a stopped run; its receipt carries the problem.
`dots sync` with invalid arguments exits 2.
Ctrl-C cancels the entire process group of every active stage, such as an App Store stage running alongside others with `--mas`.
Once a cancellation begins, no stage starts, whatever its place in the order.
Ctrl-C still cancels while descendants a stage owns drain its output after its leading process has exited.
`dots` waits for this cleanup to finish before it saves the report or exits.
A cancellation is not a stage failure, so the rule above for `dots up` and `dots bootstrap` does not apply to it.
A cancellation that `dots` handles for a command it supervises is recorded as cancelled, and the command exits with the first status that applies:

1. `dots sync` exits 0, and its receipt records the stopped run.
2. When the interrupted foreground child exits with its own nonzero status, `dots` exits with that status, unchanged; cleaning up other active stages does not replace it.
3. Otherwise `dots` exits 130, including when the leading process returns 0 while cleanup is under way.

**Changed in 0.4.0:** `dots` handles the interrupt of a command it supervises, waits for cleanup, and then exits 130.
0.4.0 exits 130 whatever the child returns, even 0, and for a supervised `dots sync` too; 1.0 keeps the child's own nonzero status and the sync status 0 above.
When signalling is refused, `dots` warns on stderr that the command's processes may still be running.
A cancelled `dots remote` says on stderr and in its report that the remote host's state is unknown and should be inspected before retrying.
**Changed from 0.3.5:** `dots bundle` usage errors exit 2, not 1.

### Hook and recipe outcomes

| Status | Outcome |
| --- | --- |
| 0 | completed |
| 3 | deferred: needs attention without failing the run |
| 4 | skipped |
| other | failed |

**Changed from 0.3.5:** installer exception recipes follow this table; 0.3.5 treated every nonzero recipe status as a failure.

### Output and reports

Every problem of `dots up` is one issue, listed under "Needs attention" in its terminal output, in `--plain` output and in the final report.
Each issue shows the command to run next and, when output was captured, the last lines of the error and the location of the step's full log.
Reports and logs are private to the user and live under `${XDG_STATE_HOME:-~/.local/state}/dots/`.
Recent completed reports are kept for `dots reports` and `--last`.
`dots help` and bare `dots` both list the configuration's commands.
**Changed from 0.4.0:** the `A` key on a finished issue no longer offers a handoff to a coding agent, and `DOTS_AGENT_COMMAND` is not read.

**Changed from 0.4.0:** `dots` has no full-screen interface.
By default in an interactive terminal, the full `dots up`, also run as `dots update`, writes to the terminal's ordinary scrollback:

- a line when each step starts;
- the output of a foreground step, which owns the terminal while it runs;
- one line with each step's result;
- a final outcome line, such as `dots up · Complete` or `dots up · Failed (exit 1)`;
- "Needs attention", with each issue, its next action and, when its output was captured, its log location;
- `Details and logs · dots up --last`.

It uses no alternate screen, navigation keys, spinner, minimum terminal size or cursor motion across child output.
A step whose output is recorded privately may update its own progress line once, and finishes that line before any child can print or prompt.
As in 0.4.0, in concise output, including headless runs and `--plain`, hooks and recipes read the caller's stdin and `dots` captures their stdout and stderr in private logs that feed failure tails.
`--verbose` mirrors the captured output and keeps the same logs and failure tails.

`dots reports`, `dots up --last`, `dots plan`, the sync receipt and the `dots sync --last` pager keep their own displays, and the rest of this paragraph does not apply to them.
Every other individual command that `dots` supervises, `dots status` included, writes directly to the terminal's scrollback.
Its child runs in the caller's foreground, so a native prompt such as `sudo` or `ssh` reads the terminal as usual.
It reads the caller's stdin, which may be redirected, and its stdout and stderr reach the caller's separately and byte for byte, even when the reader is slow.
For these commands, `dots` adds no banner, footer or second copy of a diagnostic around that output.
When such a command is marked "report", its report keeps its stages' outcomes and its exit status, but not the child's raw output or a log path that does not exist.

A report for a failure or an issue that needs attention gives the command to rerun; a report of a success need not.
`dots sync` may log its backends' output privately, and shows file contents only as [Receipt](#receipt) allows.
When any report cannot be written or finished, `dots` says so on stderr, naming the command, and still exits with the command's own result.

During interactive scrollback, hooks, recipes and steps that `dots` knows are interactive, such as its own `sudo` authorization, run in the foreground and may prompt.
**Changed from 0.4.0:** in this mode, a foreground step gets the caller's stdin, stdout, stderr and controlling terminal from its start, with no handoff.
Only one step at a time writes to the terminal, and `dots` never reads a reply to a prompt.
`dots` does not detect prompts.
In interactive scrollback, `dots` records the other stages' output privately for logs and issue tails.
A failed foreground step in interactive scrollback reports its outcome, exit status and next action, and says `Output was shown on the terminal` in place of a tail and log location.
`dots` restores the caller's terminal after a normal exit, a cancellation or an unwinding panic.
A `SIGKILL` or an abort gives it no chance to do so.

## Command reference

### `dots --version`

It prints the version and how `dots` was installed: `homebrew`, `mise` or `cargo` when the executable's location shows it, else `other`.
**Changed from 0.3.5:** `checkout` and `archive` are gone.

### `dots status`

It prints the program list first, then the version, the agent skill links, the selected profile and machine ID, the source mode and repository, the declared package state, the tool state, the dotfile and history state, and the installer exceptions.
Without a configuration it prints the program list and exits 2.
With a configuration but no usable mise it prints the program list, the version and the skill links, then exits 2.
Otherwise it exits 1 when a section fails, and 0 when every section succeeded or only needs attention.
A missing optional program and a source diagnostic do not fail it.
**New in 1.0:** the program list gives a found optional program the informational label `version unavailable` when its version cannot be read.
**Changed in 0.4.0:** it names the host and profile, prints each item once under its group with its status and summary, and puts any action the item supplies underneath, including for an item that is ok, such as a pending sync.
Each of these fields is shown as one line without control characters.
A selected profile that `dots_profiles` does not list gives one diagnostic on stderr and exit status 2, and the report keeps that diagnosis and its next action as a startup issue.
**Changed from 0.3.5:** the program list is new, and the Atuin section is gone.

### `dots plan`

It prints mise's bootstrap plan for the selected profile and machine, without installing anything.
**New in 1.0:** it saves a report; 0.4.0 prints the plan directly and saves none.

### `dots apply [TARGET...]`

It applies the declared dotfiles and templates, or only the named targets, through mise.

### `dots test [ARGS...]`

It runs `ROOT/tests/run ARGS...` in the configuration's mise environment.
**Changed from 0.3.5:** `tests/run` is executed directly and must be executable; 0.3.5 ran it with `bash`.

### `dots up`

`--mas` includes Mac App Store updates; `--last` prints the last saved `dots up` report and exits with the status it records; both together exit 2.
The stages run in this order:

1. **Pre-update checkpoint.** Saves tracked settings, after checking that every tracked entry is encrypted.
   With nothing tracked, it is skipped without attention.
   When it fails, no update starts and `dots up` exits 1.
2. **Sync**, as `dots sync`.
   In local-only mode it is skipped with one line and needs no attention.
   A sync problem makes the stage deferred: it is listed under "Needs attention", later stages still run, and on its own it never makes `dots up` exit 1.
3. **Reload.** Later stages use the configuration as sync left it.
4. **Packages.** Each available package manager with declarations for this profile and machine gets its own pass.
   The apply pass honours explicit `state = "absent"` declarations, even when they are the only declarations for that manager.
   Native mise apply determines the outcome, including when the manager cannot remove a package.
   Each operation keeps declared package pins and exclusions such as holds.
   A manager-only pass can run when every declaration for that manager is eligible for the operation.
   A pass with no eligible work is skipped.
   If eligible work remains and the pass cannot select it while preserving pins and exclusions, the manager stage fails before changing packages and reports why.
   Other independent stages continue under the usual `dots up` rules.
   Eligible missing casks declared present are installed.
   Eligible installed casks declared present are upgraded unless their metadata marks them auto-updating or `dots_self_updating_casks` lists them.
5. **Tools.** Installs newly declared mise tools, then upgrades declared tools; a tool pinned to a revision keeps it, and replaced versions stay installed.
6. **Installer exceptions.** Each selected recipe; with `install-only`, the recipe gets `--install-only`.
7. **Update hooks**, in name order.
8. **Standalone mise.** Updates mise itself, unless a package manager installed it.
9. **Manager.** When the configuration declares `dots` and a stage installed another version, it reports `dots X -> Y, used from the next run`.
10. **Post-update checkpoint.** As stage 1.

**Changed from 0.3.5:** the pre-update checkpoint is the first stage; 0.3.5 ran it after sync and reload (`tasks/update:186-216,346`).
With `--mas` on macOS and declarations that select `mas`, App Store updates run alongside stages 5 to 7, and stage 8 waits for them.
Without cached administrator access, a noninteractive run defers the App Store stage.
An interactive run asks for administrator access in the foreground, when it is needed, before the App Store stage starts.
**Changed from 0.4.0:** the App Store worker runs detached from the terminal and noninteractively, and its output goes to its log.
`dots up` reports its result at the next stage boundary.
When its administrator access expires, the stage is deferred with a next action and never prompts again while another step owns the terminal.
Homebrew keeps the old version of anything `dots up` upgrades until the user's next cleanup.
The run finishes on the `dots` version it started with.
A failing stage does not stop later ones; Ctrl-C does, as [Exit status](#exit-status) describes.
A configuration that becomes invalid during the run stops it with exit status 1.
`dots up` never prunes undeclared packages or old versions, restarts services or updates project dependencies.
Its sync stage pushes `dev` like `dots sync`; `dots up` never pushes `main` or opens a pull request.
Package, tool and standalone mise changes come from comparing state before and after their stage; hooks and recipes report theirs through `DOTS_CHANGES`; the sync stage reports the configuration revision it adopted.

### `dots sync`

`dots sync` takes no arguments except `--last`.

#### Checkout shape

- The checkout is a plain Git checkout at the repository root, with `HEAD` on `dev` and at least one commit.
- Sync stops with one line on a detached `HEAD`, another branch, a `.jj` directory, or state from a pre-1.0 sync layout.
- `dots bootstrap` moves a fresh clone that is still on `main`, with no edits, to `dev`; anything else is converted by hand.
- Sync skips with one line while a merge, rebase, cherry-pick, revert or bisect is in progress, or while another sync or publish on the same repository runs.
- A lock left by a dead process on this host is reclaimed.
- Sync skips with one line when Git is older than 2.43.

#### One run

1. Saves tracked history, refusing any tracked entry that is not `encrypt = true`.
2. Captures edits to copied dotfiles: when the deployed copy changed and its source still matches the last common version, the deployed bytes become the source.
   When both the copy and the source changed, both stay as they are and sync reports it.
3. Stages every tracked or unignored public edit as a candidate commit, leaving machine-local files out, without moving `dev` or the index.
4. Scans the candidate with gitleaks before `dev` or the index advances.
   As in 0.4.0, sync uses the checkout's live gitleaks rules and ignore entries, so a local exception governs the scan of the change that adds it.
   The native Git scan covers outgoing commit diffs, including merge diffs against their first parent.
   A flagged file stays on disk and out of the candidate, and the remaining candidate is scanned again; only a clean candidate becomes `dev`.
   Without a working gitleaks, or when a scan fails, the candidate is dropped, the edits stay on disk, and nothing is shared or pushed.
   A flagged commit already in local history that no published branch contains, or one that ever tracked a machine-local file, blocks the push; sync never rewrites the branch to remove it.
5. Fetches `dev` and `main`, or continues offline.
6. Merges local work with `origin/dev`, then with `origin/main`, outside the live files.
   On a conflicting hunk, `main` wins over `dev` and `dev` wins over local work.
   Binary, file-type and delete conflicts take the published file whole.
   Both parents stay in history, so the losing version is recoverable.
7. Validates the merged configuration with this machine's local files; an invalid result is neither pushed nor applied.
8. Pushes by creating or fast-forwarding `origin/dev` only, conditional on the remote tip it validated, and retries a lost race up to three times.
   If published history disappears during a retry, sync keeps local work and applies nothing from the withdrawn history.
   A push that takes longer than 60 seconds is stopped, and Git never prompts for credentials.
9. Updates the checkout and applies dotfiles under the guarded rules below.

#### Guarded checkout and copies

- Before writing, sync checks incoming paths for local obstructions, including ignored files, directories and symlinks; a blocked path stops the checkout, and unrelated edits are still shared.
- A file edited during the run is never overwritten; sync reports it and asks for a rerun.
- An interrupted checkout, index installation or branch update is finished by the next run before it takes a new snapshot.
- A regular copied dotfile is replaced only after moving the current target aside and confirming the bytes are the expected ones; the replacement is created only at an empty path.
- When another writer claims the path, sync leaves it alone and prints where the moved file is.
- After a replacement, the previous file stays at a printed private path, so an editor that still has it open can finish saving.
- For a directory copy, sync verifies the bytes and permissions of each leaf it replaces; edited and unmanaged files stay.
- When the deployed mode of a leaf cannot be verified, the leaf is updated only if its current mode can stay.
- Recovery files stay on the same filesystem as the target; when that is impossible, sync reports the path and keeps both versions.
- Other targets are applied in dependency order with the usual mise hooks, and matching reload commands run once, after the writes.

Sync never force-pushes, skips validation, discards uncommitted edits or writes conflict markers into live files.
Each issue carries a diagnostic and a recovery action.

#### Receipt

- Each finished sync, including the one inside `dots up`, prints a receipt and records it for replay with private permissions.
- When recording fails, sync still prints this run's receipt, warns that it could not save it for replay, and exits 0; the run's outcome does not change, and `sync --last` may show an earlier sync or none.
- A clean run prints one line, which summarizes local work, incoming changes and the push.
- Otherwise the receipt leads with any issue and its next action, then lists up to five paths per group: shared, saved locally, updated in this checkout, finished after an interruption.
- When stdin and stdout are terminals, standalone sync may show a complete small text diff for up to two paths and twelve displayed lines.
- Piped output, `--plain` output and saved reports contain metadata only.
- A missing gitleaks gives a receipt such as `dots sync: kept 2 files on disk · pulled dev +1, main +0 · not pushed`, led by the issue `NOT SHARED: gitleaks not found; local edits kept on disk, nothing pushed` and the action to install it.

`dots sync --last` replays the latest receipt for this checkout from the recorded Git revisions, without the network and without mise.
It spends up to three seconds screening patches and keeps up to 1 MiB for the pager.
A patch is shown only when gitleaks returns a clean JSON report for both file versions and for the displayed text; otherwise the file is listed without its patch.
Binary, encrypted and unverified changes are listed as file entries.
**Changed from 0.3.5:** receipts written by any earlier release are ignored, and `sync --last` then says no receipt exists.

### Requests to `dots add` and `dots remove`

Each argument after the options is one request.
A request is a **file** when it starts with `/`, `~/`, `./` or `../`.
Every other request is **software**: a GitHub URL, a name with a backend prefix such as `npm:prettier`, or a bare name.
A bare name is software even when it starts with a dot, so adopt `~/.zshrc` from `HOME` as `./.zshrc`.
A relative file request is resolved against the caller's directory.
The selectors are `--base` (`config.toml`), `--profile P` (`config.P.toml`) and `--path FILE`; at most one may be given.
Without a selector, `dots add` writes to the selected profile's file, and stops with exit status 2 when no profile is selected.
A selected file that does not exist, such as a new `config.PROFILE.toml`, is created only once a request succeeds; a failed request leaves no new or empty file.
`--asset` or `--keep-installed` with a file request exits 2.
For a file request, `dots add` and `dots remove` accept only a selected file that is `ROOT/config.toml` or `ROOT/config.PROFILE.toml` for a profile in `dots_profiles`, after resolving `--path` and any symlinks.
Any other selected file, including one a symlink leads out of that set, is refused with exit status 2 before anything changes, so every adoption can be reversed through the selector that made it.
For a software request, `--path FILE` keeps its usual meaning.

### `dots add SOFTWARE...`

It installs first and records only what installed.
A bare tool name uses mise's preferred backend and defaults to `latest`.
A backend prefix such as `npm:` or `cargo:` is recorded as a mise tool.
A mise tool request accepts `TOOL@VERSION` and supported inline tool options in brackets, such as `TOOL@VERSION[KEY=VALUE]`.
Existing inline tool options are retained unless explicitly changed.
In `npm:@scope/name`, the leading `@` is part of the package name.
`brew:` prefers a formula and selects a cask only when the formula is confirmed absent; `brew-cask:` always selects a cask.
New Homebrew declarations are restricted to macOS.
`apt:`, `dnf:`, `pacman:`, `apk:` and `mas:` select those managers.
For `brew:`, `brew-cask:` and `mas:`, `@` is part of the name.
For `apt:`, `dnf:`, `pacman:` and `apk:`, `NAME@VERSION` supplies the version for a new declaration.
A GitHub URL with a prebuilt command-line release uses mise's GitHub backend.
A macOS app in a ZIP or tar release is recorded with a post-install hook that runs `dots install-release-app OWNER/REPO`.
DMG and pkg releases, and repositories without a release, are refused; no script from the repository runs.
`--asset GLOB` names one release asset and needs exactly one GitHub URL.
A package that an installer exception holds is refused before anything installs.
On an install failure, `dots add` shows the declaration and asks whether to record it for a later retry; the default is no, and a noninteractive run never records it.
With several requests, earlier successes stay recorded and the command stops at the first failure.

### `dots add FILE...`

**New in 1.0.**
For a file request, `REL` is the target's path relative to `HOME`, the source is `ROOT/dotfiles/REL`, and the declaration is `"~/REL" = { source = "dotfiles/REL", mode = "symlink" }` under `[dotfiles]` in the selected file.
`dots add FILE` requires Git to check ignore rules, including for a root without a repository.
A root without a Git repository stays uninitialized.
If Git is missing, it prints one diagnostic naming Git and the action and exits 1.

`dots add` first checks for an exact earlier adoption: the target is a symlink to that source, the source is a regular file, and the selected file holds that declaration.
Such a request reports the file as already adopted, changes nothing and succeeds.

Otherwise it refuses, with exit status 2 and nothing changed:

- a target that is not a regular file, or whose path after resolution is outside `HOME` or inside the configuration root or `dots`' own state;
- a target with a symlink among its parent directories below `HOME`;
- a source path whose parent directories below `ROOT` include a symlink or a non-directory, or that the configuration's Git ignore rules would exclude;
- a source path that already exists, or that any configuration file in `ROOT` already uses, for any profile;
- a target that any configuration file in `ROOT` already declares;
- two requests in one invocation that resolve to the same target.

Then it copies the target's bytes and mode to the source, adds the declaration while keeping every other byte, comment and order of the file, and replaces the target with a symlink to the source.
If the target changes while this happens, or any step fails, `dots add` puts back the original target, removes the source and declaration it added, never overwrites a file it did not create, and exits 1.
A rerun after an interruption either finishes the adoption or reports exactly what is left to reconcile.
Afterwards the target is a link, and `dots status` lists it.
The new source reaches other machines with the next sync, which scans it first.

### `dots remove TARGET...`

For software targets it searches every active configuration file, including the machine-local ones, and a selector narrows the search.
A software target is named by its exact declaration (`brew-cask:firefox`), by a short name that identifies exactly one piece of software, or by a GitHub URL.
`mise:NAME` selects a mise tool when another manager also has `NAME`.
When another active file still declares the software, only the selected declaration goes and the software stays.
mise tools are removed with `mise unuse` and then pruned only for that tool, which keeps versions other projects use.
Native packages are removed with one targeted uninstall after checking dependents, never with zap, purge or force.
GitHub apps that pass the [ownership checks](#dots-install-release-app-ownerrepo) are moved to the Trash and their release installation is removed.
When a removal cannot be confirmed safe, the command stops; when a step fails, it restores the declarations and any staged app.
Settings and app data stay where they are.
`--keep-installed` deletes only the declaration.
`--dry-run` prints the plan and changes nothing.

**Changed in 0.4.0:** a short name from any declaration or alias of the software matches it, not only one from the first; the bundle name of an app `dots` owns matches as before.
Names count pieces of software, not declarations or spellings: the same software declared in several files, or under several spellings, counts once.
A software target that matches nothing prints one diagnostic on stderr and exits 1 with nothing changed.
The diagnostic suggests a name to use only when exactly one piece of software is a close match and an installer exception does not hold it; between several it never guesses.
When the app ownership records cannot be read, a full removal suggests no GitHub app, while `--keep-installed` still may.
An exact name shared by several pieces of software lists their qualified names, `mise:NAME` for a mise tool, as alternatives.

**New in 0.4.0:** shell completion after `dots remove` offers the software it could remove:

- Tools under `[tools]`, packages under `[bootstrap.packages]` for a supported manager, and GitHub apps with a valid local ownership record.
- Without a selector, it reads the active mise files that lie physically in the selected root, including machine-local ones, and ignores the caller's `MISE_ENV`.
- With `--base`, `--profile` or `--path`, it reads only that file, without discovering active files; a relative `--path` is resolved against the caller's directory.
- An app whose ownership record outlived its declaration, as after `--keep-installed`, is offered without a selector, and with one only when the selected file declares it.
- Each spelling of a declaration gives case-insensitive names: the name without its prefix, including any tap or repository path, and its last path component; an owned app adds its bundle name without `.app`.
- A name shared by several pieces of software, counted as above, is replaced by their qualified names, `mise:NAME` for a mise tool.
- A typed prefix also matches qualified names: after `npm:`, completion may offer `npm:@scope/tool` even when no other software shares its short name.
- Every qualified name offered is one that removal accepts, such as an exact declaration or `mise:NAME`.
- Targets already on the command line, targets an installer exception holds under the same rule as removal, and names that are not valid targets are left out.
- Where the shell shows descriptions, each candidate has one saying what kind of software it is.

**New in 1.0:** without a selector, `dots remove FILE` searches all configuration files in `ROOT`, regardless of the active profile or machine.
A selector limits which file can supply the declaration to the base file or a listed profile's file, as described in [Requests to `dots add` and `dots remove`](#requests-to-dots-add-and-dots-remove).
With or without a selector, uniqueness is checked across all configuration files in `ROOT`.
A file target is an adopted dotfile when all of these hold, with paths compared after normalization:

- the target is a symlink whose destination is exactly `ROOT/dotfiles/REL`, where `REL` is the target's path relative to `HOME`;
- the source is a regular file, and no parent directory below `ROOT` is a symlink or a non-directory;
- exactly one declaration in all of `ROOT`'s configuration files, for any profile or machine, is `"~/REL" = { source = "dotfiles/REL", mode = "symlink" }`, in the selected file when a selector is given;
- no other declaration names that target or uses that source.

For such a target, `dots remove` replaces the link with a regular file holding the source's current bytes and mode, then removes the declaration and the source.
Edits made through the link are therefore kept.
It refuses, with exit status 2 and nothing changed, any other file target, and any target whose uniqueness it cannot prove, for example because a configuration file does not parse.
If the link, the source or the configuration file changes while this happens, or a step fails, it stops with exit status 1.
It then puts back the link, the source with its mode, and the configuration file only where nothing changed them since; a later edit is kept, and `dots` never overwrites a file or an edit it did not make.
A rerun after an interruption either finishes the removal or reports exactly what is left to reconcile.
No record of earlier adoptions is needed.

### `dots bootstrap`

```text
dots bootstrap [--bundle FILE] [--identity KEY] [--restore ID] [MISE-BOOTSTRAP-ARGS...]
```

It runs mise's native bootstrap for the whole declared setup with `--skip-dirty`, with `DOTS_ROOT` exported.
On mise 2026.11.9, a dotfile target that already held other bytes made the native bootstrap exit 1 despite `--skip-dirty`, so not every collision is skipped.
`dots bootstrap` then reports the native failure, exits 1, and leaves the target's bytes as they are.
When the native bootstrap stops before its final task, `dots bootstrap complete` does not run, and `dots bootstrap` says so instead of running it separately.
Relative `--bundle` and `--identity` paths are resolved against the caller's directory.
`--identity` names the external age identity for both the bundle and recovery.
`--restore ID` restores that machine's settings history; `ID` must be a valid machine ID, and it cannot be combined with `--only`.
With `--restore`, `dots` skips mise's services step in the native bootstrap, so no history watcher runs before the restore; the completion stage restores first, and enrollment then activates the watcher.

A configuration connects bootstrap to `dots` in two places:

```toml
# config.laptop.toml
[bootstrap.hooks]
pre-dotfiles = 'SETUP_PROFILE=laptop "$(command -v mise || echo "$HOME/.local/bin/mise")" x github:EzraCerpac/dots -- dots bootstrap persist-selected'

# config.toml
[tools]
"github:EzraCerpac/dots" = "1.0.0"

[tasks.bootstrap]
description = "Finish the dots bootstrap"
run = 'dots bootstrap complete'
raw = true
```

On mise 2026.11.9, with only `/usr/bin:/bin` on `PATH`, a native bootstrap ran the `pre-dotfiles` hook before dotfiles were applied and without `mise` on `PATH`, then applied dotfiles, ran the `post-dotfiles` hook, ran the tools step, and ran the `bootstrap` task last.
So the `pre-dotfiles` hook locates the standalone mise itself and runs `dots` through `mise x github:EzraCerpac/dots`, which uses the version that `[tools]` in `config.toml` pins, here 1.0.0.
On mise 2026.11.9 on macOS, with an empty temporary `HOME` and data, state and cache directories, and `"github:EzraCerpac/dots" = "0.3.5"` in `[tools]`, a `pre-dotfiles` hook running `mise x github:EzraCerpac/dots -- dots --version` through the standalone mise fetched and installed the pinned 0.3.5 before it existed and printed its version; dotfiles were then applied, the tools step found `dots` already installed, and the bootstrap exited 0.
That verifies resolving the pin and launching it from the hook, not the 1.0 callback itself.

`dots bootstrap persist-selected` persists the profile and machine ID as `dots profile` does.
It keeps an existing persisted profile and machine ID.
Without one, it takes the profile from `SETUP_PROFILE`, else from a single profile named in the caller's `MISE_ENV`, and the machine ID from `SETUP_MACHINE_ID`, else from a persisted `host-` selector, else `auto-` followed by a random identifier.
With `--restore ID` in effect, the machine ID is `ID`, and a different persisted ID stops it with exit status 2.
It records `SETUP_HISTORY_ORIGIN` as the history origin when set and preserves any conflicting recorded history origin.
It never runs a full bootstrap.

`dots bootstrap complete` runs these stages, printing one result line per stage and a final count:

1. **Private inputs.** Selects, decrypts and validates a bundle as in [Bootstrap bundle](#bootstrap-bundle), and remembers the identity path in `config.local.toml`.
2. **Source repository.** Moves a fresh clone that is still on `main`, with no edits, to `dev`.
3. **Restore private settings**, only with `--restore`.
   Restore requires the selected recovery identity to already exist.
   The history origin is the one recorded in `config.local.toml` when it matches `dots_state_repo`; otherwise it comes from `dots_state_repo`, where `OWNER/NAME` becomes `https://github.com/OWNER/NAME` and a URL is used as written.
   Before restoring, bootstrap records that origin, the branch `host-ID` and the recovery identity in `config.local.toml`.
   A recorded origin that does not match `dots_state_repo`, or a recorded recovery identity other than the one in use, stops the stage with nothing changed; a recorded value is never replaced by a default.
   It then restores as `dots restore --initialize-history`, with the recovery identity and the user's normal Git credentials.
4. **Installer exceptions**, as `dots exceptions`.
5. **Bootstrap hooks**, in name order, each its own stage.
6. **Encrypted local history**, when `dots_state_repo` or `SETUP_HISTORY_ORIGIN` is set, as described in [Enrollment](#enrollment).

**Changed from 0.3.5:** 0.4.0 always listed the history stage, and 1.0 lists it only when the state repository or history origin is set.
`SETUP_HISTORY_ORIGIN` alone lists the stage, independently of the source repository's origin.
It exits 1 when any stage failed, and 0 otherwise.
**Changed from 0.3.5:** the Atuin, Tailscale and private-file stages are gone, and so is the `GH_TOKEN` injection from a bundle value.

A restore needs the user's normal Git authentication for the state repository to work before bootstrap starts: SSH keys, or a configured credential helper.
A bootstrap hook runs after the restore stage, so it cannot supply authentication to it.

### `dots remote`

```text
dots remote HOST --profile P [--dry-run] [--yes] [--identity-file FILE] [--port N] [--connect-timeout S] [--github-relay-repo R] [--github-relay-read-only]
```

It runs mise's remote bootstrap on `HOST` over SSH, installing mise there, adopting the source repository and selecting profile `P`.
`P` must be one of `dots_profiles`.
The source is `dots_source_repo`, else the `origin` URL; without either it exits 2.
**Changed from 0.3.5:** the source may be a Git URL.
`mise bootstrap remote --help` on 2026.11.9 accepts a URL for `--adopt`; a real remote enrollment from a URL is unverified.

As in 0.4.0, `dots remote` leaves the target's history settings alone.
If history is enabled on the target, mise stores tracked files without encryption in plaintext in the target's local history.
Declaring the file with `encrypt = true` or disabling history on the target avoids that plaintext storage.
The new [backup publication audit](#dots-backup) checks outgoing history separately and honours mise's `allow_plaintext_history` opt-in.

### `dots bundle`

```text
dots bundle create --input PRIVATE.json --output BUNDLE.age --recipient age1... [--recipient age1...]...
dots bundle check --bundle BUNDLE.age --identity KEY
```

`create` validates the input, encrypts it with age to every recipient, and creates the output exclusively with mode 0600, refusing a path that exists.
It writes no plaintext anywhere.
`check` decrypts and validates in memory.
**Changed from 0.3.5:** `--from-live` is gone.

### `dots publish`

```text
dots publish [--all | --bookmark NAME]
```

It runs in a colocated JJ workspace of the configuration repository, never in the live checkout.
**Changed from 0.3.5:** invalid settings in the base file's `[vars]` make `dots publish` exit 2.
The raw `origin` URLs in the live checkout and in a separate colocated JJ clone must both match the expected GitHub repository, using any of the six spellings in [Remotes](#remotes).
When `dots_source_repo` is set, it supplies the expected repository.
When `dots_source_repo` is unset, the live checkout's raw GitHub `origin` supplies the expected repository.
With valid settings, `dots publish` exits 1 before looking for JJ, `gh` or gitleaks when the source is outside GitHub or either raw `origin` is absent, malformed, credential-bearing or mismatched.
The raw checks must pass before URL rewrites are considered.
Before contacting the remote, the workspace's effective fetch URL and every effective push URL must also match the expected repository.
A rejected origin or effective URL makes `dots publish` exit 1 with one line naming the GitHub requirement or repository mismatch and omitting the rejected URL.
By default it publishes exactly one unpublished working change directly above `main@origin`.
`--bookmark NAME` publishes an existing private `wip/...` bookmark.
`--all` publishes the current stack, offering to merge `main@origin` into it first.
It fails with exit status 1 before any push or pull request when `main` does not exist on the remote.
In every mode, if the workspace's local `dev` tip is an ancestor of the target (`@` or the requested private bookmark) and is outside the ancestry of `main@origin`, publication stops with exit status 1 before a push or pull request.
The diagnostic tells the user to extract a focused change onto a branch based on `main`.
It scans the outgoing commits with gitleaks, asks for confirmation, pushes a bookmark without force, and opens or updates the pull request.
**Changed from 0.4.0:** publish takes its gitleaks policy only from the approved tree and screens the full terminal preview before displaying it.
**New in 1.0:** publish uses gitleaks' native archive traversal one level deep.
An explicitly reported unsupported-format or depth-limit skip stops publication before confirmation or push.
A successful scan with no reported skip proceeds to normal confirmation.
As in 0.4.0, new pull requests target `main`.
When exactly one pull request matches the head branch from the same repository and that pull request is open, `dots publish` edits its title and body and keeps its existing base, including a base other than `main`.
The scan still uses `main@origin` as its base.
When `dots_publish_on_behalf_of` names a person, the pull request text notes that a model, named by `DOTS_PUBLISH_MODEL` when set, wrote it on that person's behalf.
It exits 3 when conflicts stop it, and 1 on other failures.

### `dots reports [COMMAND]`

**Changed from 0.4.0:** it prints recent completed reports, newest first, optionally only one command's.
Each line gives the command, time, outcome, exit status and the report's location.
It prints the same listing to a terminal, a pipe or a redirected file.
A valid listing exits 0, and an invalid command filter or usage exits 2.

### `dots completions bash|zsh|fish`

**New in 0.4.0.**
It takes exactly one argument and prints that shell's completion script on stdout, and nothing else, with exit status 0.
It runs before any setup and without mise, and does not refresh the agent skill links.
A missing or unknown shell, or another argument, prints usage on stderr and exits 2.
From 1.0, `-h` and `--help` follow [Global syntax](#global-syntax); 0.4.0 treats them as an unknown shell.

Homebrew installs and upgrades the scripts in its standard completion locations, as `dots` for Bash, `_dots` for Zsh and `dots.fish` for Fish.
With mise, a release binary, Cargo, or a Homebrew shell that does not load completions by itself, the user adds one line to the shell's interactive startup:

| Shell | Startup file | Line to add |
| --- | --- | --- |
| Fish | `~/.config/fish/config.fish` | `dots completions fish \| source` |
| Zsh | `~/.zshrc`, after `compinit` | `source <(dots completions zsh)` |
| Bash | `~/.bashrc` | `eval "$(dots completions bash)"` |

For a Bash login shell, the login profile sources `~/.bashrc` or holds the loader itself.
Each new shell then completes with the active release.
`dots` never edits a startup file, adds a setting or copies a script for the user to maintain.

Completing a word is local, quiet and bounded, and works offline.
It never prompts, runs no hook and no configuration command, evaluates no environment, installs or updates nothing, and writes no persistent state, cache, log or skill link.
Commands and options are offered before any setup and without mise.
A dynamic input that is unavailable, such as an unreadable file or a mise that does not answer in time, is skipped; completion never guesses which configuration files are active.

Completion draws candidates only from these sources, and does not promise one for every argument:

- Commands: the public built-in commands, the aliases `update` and `uninstall`, and each configuration command that [Configuration commands](#configuration-commands) would run, only where that command could start; a configuration command's arguments are not completed.
- Global flags: `--plain` or `--verbose`, before the command and after `up` or `update`; `-h`, `--help` and `--version` only as the first word.
- A command's options, with short forms such as `-n`, only where it accepts them: never one that conflicts with an option already given, a second copy of an option that may appear only once, or any option after `--` where the command accepts `--`.
  A repeatable option, such as `dots bundle create --recipient` or `dots bootstrap --env`, `--only` and `--skip`, stays on offer as the grammar allows.
- An option's value, inline as `--option=VALUE` or as a comma-separated list, only where the option accepts that form.
- Profile names, for `--profile` and `dots bootstrap --env` or `-E`, from `dots_profiles` in `ROOT/config.toml`.
- `bash`, `fish` and `zsh` for `dots completions`; `create` and `check` for `dots bundle`; the commands that save reports for `dots reports`, which from 1.0 include `dots plan`.
- `python`, `node` and `shell` for `dots test`, as hints for a conventional `tests/run`; any other argument still passes through.
- For `dots add`, prefixes only, with no registry search: the native managers `apk:`, `apt:`, `brew:`, `brew-cask:`, `dnf:`, `mas:` and `pacman:`, the mise backends `aqua:`, `asdf:`, `cargo:`, `conda:`, `dotnet:`, `gem:`, `github:`, `go:`, `npm:`, `pipx:`, `spm:`, `ubi:` and `vfox:`, and `https://github.com/`.
- For `dots remove`, the targets that [`dots remove`](#dots-remove-target) describes.
- For `dots apply`, `--force` (`-f`), `--dry-run` (`-n`), `--yes` (`-y`), `--prune` and `--prompt-secrets`.
- For `dots bootstrap`, its own options and the native bootstrap options it passes on: `--from`, `--adopt`, `--from-dir`, `--config-dir`, `--replace-history`, `--take-remote-all`, `--dry-run` (`-n`), `--yes` (`-y`), `--skip-dirty`, `--force-dotfiles`, `--prune-dotfiles`, `--only`, `--skip`, `--prompt-secrets`, `--prompt-vars`, `--save-env`, `--env` (`-E`), `--update` and `--upgrade`.
  `--only` and `--skip` exclude each other, and so do `--only` and `--restore`.
  `--only` and `--skip` take part names: `plugins`, `packages`, `accounts`, `files`, `services`, `firewall`, `compose`, `repos`, `dotfiles`, `mise-shell-activate`, `macos-defaults`, `settings`, `macos-launchd-agents`, `linux-systemd-units`, `user`, `tools`, `task` and `final-hook`; `--restore` takes a machine ID.
- Local files for `--path`, `--bundle`, `--identity`, `--identity-file`, `--input` and `--output`; directories for `dots init` and `--from-dir`; nothing for free text, such as `--asset`, `--bookmark`, `--restore`, a port, a repository, and `--config-dir`, which is relative to the checkout, not to the caller's directory.

Each script keeps its shell's usual behaviour: quoting and escapes, completion at a cursor with text after it, a prefix such as `brew:` left open for typing, and the shell's own path completion, including names with spaces or tabs, a literal `=` and names that look like options.
An unquoted `~/` at the start of an earlier word means the home directory, as when the shell runs the command; a quoted or escaped `~/` is literal.

## The `dots/` directory

Every part is optional.

| Path | Purpose |
| --- | --- |
| `hooks/update.d/` | Executables `dots up` runs after packages, tools and installer exceptions |
| `hooks/bootstrap.d/` | Executables `dots bootstrap` runs after installer exceptions |
| `commands/NAME` | An executable run as `dots NAME [ARGS...]` |
| `exceptions.tsv`, `exceptions/NAME` | Apps mise cannot install, each with its own recipe |
| `retired-dotfiles` | Removed dotfile targets whose dangling links may be deleted |
| `retired-declarations` | Stale declarations sync removes from machines' local files |

### Environment of children

**New in 1.0:** `dots` puts its running executable first on `PATH` in the environment it hands each child it starts directly, even when the user started `dots` by an absolute path.
Every hook, recipe, configuration command and `dots test` child, and every other launch that `dots` itself makes for `dots bootstrap`, starts with that executable first on `PATH`, after any configuration environment is applied, so `dots` there is the same executable at the same version.
`dots` arranges this without a setting.
In 0.4.0, a configuration command and the shell fallback for `dots test` could both run another `dots` from the caller's `PATH`.
For programs started by an explicit `dots mise exec` or `dots mise run`, see [`dots mise`](#dots-mise-args).
As in 0.4.0, mise's native bootstrap uses the configuration's pinned `dots` for its callbacks, as [`dots bootstrap`](#dots-bootstrap) describes.
Every hook, recipe, configuration command, `dots test` child and `dots mise` child gets `DOTS_ROOT` and `DOTS_PROFILE`.
**Changed from 0.3.5:** `DOTS_HOME` is not set.

### Hooks

A hook is a regular executable file whose name matches `^[A-Za-z0-9][A-Za-z0-9._-]*(@[A-Za-z0-9._-]+)?$`; other names are ignored.
Hooks run in byte order of their names, from the configuration root, in the configuration's mise environment, each as its own reported stage.
A failed hook does not stop later ones.
The stage label is the name without its `@PROFILE` suffix, a leading number followed by `-` or `_`, and a final extension, with dashes and underscores as spaces: `20-Login-shell.sh` reports as `Login shell`.
A name ending in `@PROFILE` applies only to that profile; other profiles do not list it.
A suffix that names a profile outside `dots_profiles` fails the listing before any hook in that directory runs.
A hook can prompt, as [Output and reports](#output-and-reports) describes.

Update hooks also get:

- `DOTS_TOOLS_OK`: `1` when installing newly declared tools succeeded in this run, else `0`.
- `DOTS_TOOLS_UPGRADED`: `1` when that succeeded and upgrading declared tools succeeded too, else `0`.
- `DOTS_CHANGES`: a file; each `ITEM<TAB>BEFORE<TAB>AFTER` line appended to it appears as a change in the hook's report, with `not installed` as `BEFORE` for a new installation.

Bootstrap hooks also get `DOTS_PRIVATE_READY`, `DOTS_RESTORED` and `DOTS_BUNDLE_FD`; see [Bootstrap bundle](#bootstrap-bundle).

### Configuration commands

`dots NAME ARGS...` runs `commands/NAME` when `NAME` is not a built-in, matches `^[A-Za-z0-9][A-Za-z0-9._-]*$`, and is a regular executable file.
The arguments reach it unchanged, and it runs in the caller's directory.
`DOTS_PROFILE` keeps a value the caller set; otherwise it is the persisted profile, or empty.
It runs before mise loads the configuration: no sync, tool installs or package updates.

### Installer exceptions

`exceptions.tsv` has one row per app, tab-separated: the profile (`base` for every profile), the recipe name, and optional comma-separated flags.
Empty lines and lines starting with `#` are ignored.
A row with a fourth column, an unknown profile, an invalid name or an unknown flag stops the reading command with exit status 2.
The recipe is the executable `exceptions/NAME`, with `NAME` matching `^[A-Za-z0-9][A-Za-z0-9._-]*$`; a selected row without an executable recipe is an error.
`install-only` makes `dots up` pass `--install-only`, so the recipe only installs a missing app.
`hold=MANAGER:NAME` records that the recipe pins that package, so `dots add` refuses it; the value matches `^[a-z][a-z0-9-]*:[A-Za-z0-9@._/+-]+$`, and a request matches after dropping any tap prefix or `@version`.
Bootstrap always passes `--install-only`.
Under `dots up`, a recipe also gets `DOTS_CHANGES`.
A recipe updates exactly its own app with its own package-manager command, never a whole-environment update.
A recipe can prompt, as [Output and reports](#output-and-reports) describes.

### Retired dotfiles and retired declarations

`retired-dotfiles` lists one `~/PATH` per line; empty lines and `#` lines are ignored.
`dots retire-dotfiles` deletes each listed target only while it is a symlink into the configuration root, by its logical or physical path, whose destination no longer exists.
It ignores, with a warning, an entry that does not start with `~/` or that contains `..`, and reports each link it removes.
Call it from a mise `post-dotfiles` hook.

`retired-declarations` rows are `dotfiles.NAME<TAB>SOURCE`, with `SOURCE` a plain relative path without backslashes, `.`, `..` or empty parts.
Invalid rows are ignored individually.
Before validating a merged configuration, sync validates it with each machine-local file's matching tables taken out.
A table goes only when it is exactly `[dotfiles.NAME]`, its decoded `source` string equals `SOURCE` without path normalization, nothing else defines it, and `SOURCE` does not exist in the merged configuration.
**New in 1.0:** the source string can use any valid TOML quoting, including triple quotes that 0.4.0 left in place.
The machine-local file changes only after the merged files are on disk, and only while it is still the file sync validated; the rest of it, its mode and the deployed file stay, and sync keeps a backup and prints where.
A rendered template is a regular file and is never deleted.

## Helper subcommands

**New in 1.0.**
These replace the shell libraries that configurations sourced from the 0.3.5 manager directory, so a hook or recipe can be written in any language and tested against these commands.

### `dots root`

It prints the physical configuration root.

### `dots profile`

Without options it prints the selected profile; with none selected it exits 2 with one line saying how to select one.
`dots profile --profile P --machine-id ID [--origin URL]` validates `P` and `ID`, persists them as in [Profiles and machine identity](#profiles-and-machine-identity), and confirms the selection.

### `dots mise ARGS...`

It runs mise rooted in the configuration with this machine's `PROFILE,host-ID` environment, without the caller's `MISE_ENV`, and with auto-install off.
`ARGS` are passed as an argument array and never evaluated by a shell.
**New in 1.0:** `dots mise` is a public command; 0.4.0 had no such built-in.
For an explicit `exec` or `run`, mise receives those arguments and the active configuration unchanged and applies its own `PATH` rules inside the resulting command or task.
Configured tool directories and `[env] _.path` can precede the inherited `PATH`, so a bare `dots` there resolves according to the configuration.
On macOS, for mise's package bootstrap and status, it exposes the configuration's already-installed Ruby so Homebrew tap evaluation does not go through a shim; it never installs Ruby.
It exits with mise's status.

### `dots exceptions`

It runs every selected installer exception with `--install-only`, naming each before it runs and each failure after.
It exits 1 when any recipe failed.

### `dots enroll-history`

```text
dots enroll-history --profile P --machine-id ID --identity PATH [--origin URL] [--recipient age1...]...
```

See [Enrollment](#enrollment).

### `dots install-release-app OWNER/REPO`

mise runs it as the post-install hook of a GitHub release that `dots add` recorded as a macOS app.
It needs exactly one app bundle in the installed release, and replaces the app in `/Applications` only when `dots` installed that app from this repository before.
As in 0.4.0, a valid `{repo, path, bundle_id}` ownership record and a matching app are enough to establish ownership for an upgrade or removal to the Trash, subject to the existing permission, path, marker and bundle checks.
`dots` refuses mismatches and ambiguous rename candidates.
A matching legacy record does not require the user to move the app aside first.
**New in 1.0:** `dots` records stronger app identity only after it installs or replaces the app itself.
It fails on systems other than macOS.
The hook that `dots add` writes runs `dots` by a path that survives upgrades: a Homebrew install runs the prefix's `bin/dots`; a mise install runs `dots` through `mise x github:EzraCerpac/dots`, finding mise through `DOTS_MISE_BIN`, `PATH` or `~/.local/bin/mise`; any other install runs the executable's absolute path.

### `dots brew`

Each subcommand works on one named package and never runs a global Homebrew update, autoremove or install cleanup.

- `path` prints the Homebrew executable: the first of `brew` on `PATH`, `/opt/homebrew/bin/brew` and `/usr/local/bin/brew` whose prefix is `/opt/homebrew` or `/usr/local`; with none it exits 1.
- `version formula|cask TOKEN` prints the installed version; when it is unknown it exits 1.
- `admin-available` exits 0 when the user is root, when stdin and stdout are terminals, or when cached `sudo` access exists, and 1 otherwise.
- `defer-for-admin TOKEN` prints that `TOKEN` needs interactive administrator access, with `sudo -v && dots up` as the next step, and exits 3.
- `record formula|cask TOKEN installed|updated [BEFORE]` appends `brew-KIND:TOKEN<TAB>BEFORE<TAB>AFTER` to `DOTS_CHANGES` when that is set and the package is installed; `installed` writes `not installed` as `BEFORE`, and `updated` writes only when `BEFORE` and the installed version are both known and differ.
- `update-cask [--auto-updates] [--admin] TOKEN [PROCESS] [--install-only]` installs a missing cask, or upgrades an outdated one unless `--install-only`.
  `--auto-updates` also upgrades casks that update themselves.
  With `PROCESS`, it defers while a process with exactly that name runs.
  With `--admin`, it defers when `admin-available` would fail.
  A deferral prints the reason and exits 3.
  It records each verified installation or upgrade as `record` does.
- `update-formula TOKEN [--install-only]` does the same for one formula, without the process and administrator checks.
- `hold-formula TOKEN VERSION [REASON]` succeeds when exactly that version is installed, and otherwise prints a refusal on stderr and exits 1.
- `hold-cask TOKEN VERSION APP_PATH [REASON]` does the same, reading the version from the app bundle at `APP_PATH`.
- Hold commands never install, upgrade or remove anything.
- On a system other than macOS, `update-cask`, `update-formula`, `hold-formula` and `hold-cask` print that the package is macOS-only and exit 4.

**Changed from 0.3.5:** 0.3.5 returned 0 for a deferral, sending a notice to the dashboard, and returned 0 with a "Deferred" line on systems other than macOS (`lib/brew-exception.sh:65-75,105-108,218-221`).

### Replacing 0.3.5 interfaces

| 0.3.5 | 1.0 |
| --- | --- |
| `source "$DOTS_HOME/lib/brew-exception.sh"` and its functions | `dots brew` subcommands, one per function |
| `dots_up_emit_notice` from a recipe | A message on the recipe's output and exit status 3 |
| `source "$DOTS_HOME/lib/setup-common.sh"`, `setup_init`, `setup_mise` | `dots root`, `dots profile`, `dots mise` |
| `setup_error` | A message on stderr |
| `quiet_probe` | Capture the child's output in the script's own language and print it on failure |
| `bash "$DOTS_HOME/lib/retire-dotfiles.sh" "$DOTS_ROOT"` | `dots retire-dotfiles` |
| `dots internal home` | `dots root` for the configuration; nothing for the manager |
| `dots internal persist-selected`, `complete` | `dots bootstrap persist-selected`, `dots bootstrap complete` |
| `dots internal profile`, `enroll-history`, `exceptions` | `dots profile`, `dots enroll-history`, `dots exceptions` |
| `dots internal update`, `status`, `backup`, `restore` | `dots up`, `dots status`, `dots backup`, `dots restore` |
| `dots internal github-install-app` | `dots install-release-app` |
| `dots internal commands`, `profiles`, `sync-present` | Dropped |

`dots internal` exits 2 as an unknown command.

## Bootstrap bundle

A bundle is age-encrypted UTF-8 JSON:

```json
{ "version": 1, "secrets": { "KEY": "VALUE" }, "recipients": ["age1..."] }
```

- `version` is the number 1.
- `secrets` is an object whose keys are strings and whose values are nonempty strings without NUL.
- `recipients` is optional; each entry matches `^age1[0-9a-z]+$`, and duplicates are dropped.
- Any other top-level field is rejected.

**Changed from 0.3.5:** any secret key is accepted; 0.3.5 accepted only a fixed list, so every bundle it wrote still validates.

Selection:

- The bundle is `--bundle FILE` when given, else `encrypted/bootstrap.PROFILE.json.age` when it exists.
- A bundle is requested when that profile file exists, or when a bundle or bundle identity was given now or remembered from an earlier run.
- The bundle identity is `--identity KEY`, else the remembered bundle identity, else, for a requested bundle, the recovery identity.
- The recovery identity is `--identity KEY`, else `SETUP_AGE_IDENTITY`, else the one recorded in `config.local.toml`, else `~/.config/age/keys.txt`.
- The remembered bundle identity supplies only bundle decryption.
- A requested bundle whose file or identity is missing makes the private inputs stage deferred, with `dots bootstrap --bundle FILE --identity FILE` as the next step.

**New in 1.0:** bundle values reach bootstrap hooks through `DOTS_BUNDLE_FD`.

- `dots` gives each bootstrap hook it launches `DOTS_BUNDLE_FD`, the number of an inherited read-only file descriptor.
- Each hook gets its own descriptor, readable from the start, holding the UTF-8 JSON `secrets` object.
- The object is `{}` when no bundle was requested, and `{}` when private recovery is not ready.
- Private recovery is ready when any requested bundle decrypted and validated, and any requested restore succeeded.
- `DOTS_PRIVATE_READY` is `1` when private recovery is ready, else `0`, so a hook can tell "nothing requested" (`{}` and `1`) from "requested and failed" (`{}` and `0`).
- `DOTS_RESTORED` is `1` when this run restored settings history, else `0`.
- Values arrive intact, including newlines.
- `dots` writes no value to a plaintext file, an environment variable or an argument, and prints none.
- No other stage or command that `dots` launches gets the descriptor; what the hook's own children inherit is the hook's responsibility.
- The descriptor leaves the hook's prompt channel available, so it can still prompt as [Output and reports](#output-and-reports) describes.
- The hook is responsible for what it prints, where it writes values, preserving files that already exist, and closing the descriptor before starting long-lived children.

Example: a hook that only checks that a value is present, and prints nothing secret.

```python
#!/usr/bin/env python3
import json, os, sys

if os.environ["DOTS_PRIVATE_READY"] != "1":
    print("private inputs not ready; rerun dots bootstrap after recovery")
    sys.exit(3)
with os.fdopen(int(os.environ["DOTS_BUNDLE_FD"]), encoding="utf-8") as stream:
    secrets = json.load(stream)
if "example_token" not in secrets:
    print("example_token not in the bundle; skipping")
    sys.exit(4)
print("example_token present")
```

`dots` has no built-in behaviour for any app: writing app files, credential helpers and service enrollment from bundle values belong to the configuration's hooks, and no value becomes a credential implicitly.

## Settings history

`dots backup` and `dots restore` keep app-written files in mise's encrypted dotfile history, in the private `dots_state_repo`, on one `host-ID` branch per machine.
Each such file is a native track entry with one `host-ID` profile variant per machine that keeps it:

```toml
# config.laptop.toml
[dotfiles]
"~/.config/app/state.json" = { mode = "track", encrypt = true, variants = [{ profile = "host-my-laptop" }] }
```

The variant names that machine's stream in history, so another machine never applies it.
A variant that has history must never be renamed or removed.
When `dots` checks declarations before saving a checkpoint, every spelling of the tracked entry must declare `encrypt = true`.
Native mise can still save plaintext locally, including on a target bootstrapped by [`dots remote`](#dots-remote).
Tracked files end at mode 0600.
`dots_state_repo` is required for backup, restore and enrollment, and follows [Remotes](#remotes).

### Enrollment

`dots enroll-history`, and stage 6 of `dots bootstrap complete`, prepare a machine for encrypted history:

- Once its prerequisites hold, stage 6 of `dots bootstrap complete` alone creates a missing machine identity at `~/.config/age/keys.txt`, using exclusive creation with mode 0600 and mode 0700 for a new parent directory.
- Standalone enrollment uses an existing identity to prepare machine-local configuration, public recipients and tracked file permissions.
- A missing identity makes standalone `dots enroll-history` or `dots restore` exit 2.
- **New in 1.0:** the diagnostic names the missing identity path and tells the user how to supply or generate it.
- Another machine's private key, and the recovery identity's private key, are never copied.
- Bootstrap enrollment adds this machine's public key, the recovery identity's public key and the bundle's recipients only to `config.local.toml`.
- **Changed from 0.3.5:** 0.4.0 used the bundle identity to derive the recovery recipient when the bundle file existed, while 1.0 uses the recovery identity's public key.
- A recipient refresh retains valid extra public recipients already recorded in `config.local.toml`.
- Bootstrap enrolls only once private recovery is ready and the recovery identity file exists; otherwise the stage is deferred.
- **New in 1.0:** once private recovery is ready and the recovery identity exists, a missing `dots_state_repo` fails the stage and bootstrap exits 1.
- **New in 1.0:** bootstrap fails enrollment on a conflicting recorded history origin or recovery identity and leaves the recorded value in place.
- Enrollment needs at least one recipient other than this machine's own public key: a recovery identity with a different key, a bundle recipient or a `--recipient`.
  The default recovery identity is the machine identity's own path, so it alone never qualifies.
  Without such a recipient, enrollment exits 3 and the bootstrap stage is deferred, naming `--identity` or `SETUP_AGE_IDENTITY` as the way to supply a separate recovery identity; the machine's own key is never described as a recovery backup.
- The identity is a readable regular file outside the configuration root, named by an absolute path or one starting with `~/`.
- Every tracked file this machine keeps is set to mode 0600; a tracked path that is a symlink or lies outside `HOME` is refused.
- When no tracked file names this machine's `host-ID` profile yet, enrollment exits 3 and the bootstrap stage is deferred.
- Bootstrap then records the history origin and sets history sync to `manual`.
- Only bootstrap enrollment activates mise's history watcher service and checks that it is running.
- A watcher that is not running after that leaves the stage deferred, with `dots backup` as the way to save.
- Enrollment saves no checkpoint, creates no repository and uploads nothing; publication happens only through `dots backup`.

### `dots backup`

It takes no arguments.
It stops with exit status 2 at the first failed check:

1. History sync is `manual`.
2. The machine ID is set, and the branch is `host-ID`.
3. `dots_state_repo` is valid.
4. Every tracked entry active on this machine is encrypted and active only through this machine's `host-ID` variant.
   Entries whose variants name only other machines are not active here, are never saved here, and are not checked.
5. mise's effective history origin branch is `host-ID`.
6. Every effective push URL of the history store, after Git's URL rewrites, matches `dots_state_repo` and is not the source repository.

Then it saves a checkpoint locally.
**New in 1.0:** before publishing, `dots backup` audits the actual tracked payloads in every reachable local checkpoint the state repository lacks.
Unless mise's `allow_plaintext_history` is enabled, a plaintext payload makes backup exit nonzero with a diagnostic naming the file and checkpoint, before it syncs or sends any history.
Only native `mise bootstrap dotfiles sync` publishes to the state repository, and `dots backup` reports successful publication.

### `dots restore [--initialize-history]`

- It needs a history origin in machine-local configuration that matches `dots_state_repo`, and checks the effective fetch URL as in [Remotes](#remotes) before contacting it.
- It leaves the source checkout and inactive or unencrypted targets untouched.
- Its first restore preflight checks the user's mise history watcher process and its launchd or systemd service, and refuses to proceed when the watcher runs or its state cannot be confirmed.
- **Changed from 0.3.5:** when the watcher check and a pending-marker or local-store check would both fail, 1.0 reports the watcher failure first.
  Version 0.4.0 checked the marker and store first.
- Restore never activates, stops or restarts the watcher.
- It requires an existing recovery identity and never generates one.
- Restore uses only fetch-side remote traffic. It never runs `dots backup`, pushes or publishes during restore.
- As in 0.4.0, native mise applies the fetched history to the actual targets and may save local undo checkpoints of their live contents before overwriting them.
  The checkpoints are encrypted to the configured recipients.
  A later explicit `dots backup` publishes them in the history branch's ancestry.
- On an overlapping conflict, restore keeps the live file and exits 1 with a diagnostic saying that both sides changed the same lines.
- A new store is staged and validated as a bare clone before it is installed.
  On first restore, mise applies the fetched remote state as the initial checkpoint, using its native conflict checks.
- An interrupted first restore resumes on the next run instead of treating the new store as established.
- A completed restore leaves no staging clone or pending marker.
- An existing unconnected store is replaced only with `--initialize-history`.
  The old store moves to a private sibling directory and is kept.
  If preparing the replacement fails, the old store stays in place.
- It refreshes this machine's identity and recipients, sets the active encrypted tracked files for this machine to mode 0600, and sets history sync to `manual`.
- It needs Git, Git authentication for the state repository, and the age identity to be present, and installs nothing.

## Environment variables

Read by `dots`:

| Variable | Meaning |
| --- | --- |
| `DOTS_ROOT` | Selects the configuration |
| `MISE_CONFIG_DIR`, `XDG_CONFIG_HOME` | Fallbacks for the configuration root |
| `DOTS_MISE_BIN` | The mise executable; else `~/.local/bin/mise`, else `mise` on `PATH` |
| `DOTS_SKILL_LINKS` | `0` turns agent skill linking off |
| `DOTS_PUBLISH_MODEL` | Model named in the `dots publish` note |
| `DOTS_PROFILE` | Kept for configuration commands when already set |
| `SETUP_PROFILE`, `SETUP_MACHINE_ID` | Override the persisted profile and machine ID |
| `SETUP_HISTORY_ORIGIN`, `SETUP_AGE_IDENTITY` | History origin and recovery identity for bootstrap |
| `MISE_ENV` | Read only by `dots bootstrap persist-selected` |
| `XDG_STATE_HOME` | Base of `dots` reports and logs |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | Where agents keep their configuration |

A `DOTS_MISE_BIN` that is set but not executable is a configuration error, exit status 2.
**Changed from 0.3.5:** the 0.3.5 dispatcher often fell back silently in that case (`bin/dots:231-232`), while its sync code already refused it (`lib/source-repo.mjs:151-155`).
Set by `dots` for children: `DOTS_ROOT`, `DOTS_PROFILE` and the hook variables above; mise children also get `MISE_CONFIG_DIR`, and `dots up` sets `HOMEBREW_NO_INSTALL_CLEANUP=1`.
Variables that are flags take exactly `0` or `1`.

## Agent skill

The agent skill is embedded in the executable.
On every invocation except `dots completions` and shell completion, `dots` keeps a copy of it in storage `dots` owns and links that copy for the agents in use:

| Link | Made when |
| --- | --- |
| `${CLAUDE_CONFIG_DIR:-~/.claude}/skills/dots` | That agent directory exists |
| `~/.agents/skills/dots` | `~/.agents` or `${CODEX_HOME:-~/.codex}` exists |

`dots` creates or replaces only links it made that still point where it left them, and only when the link's directory stays inside the agent's directory.
A directory, a file or any other link at those paths belongs to the user; `dots` leaves it alone and says so once.
`dots` never creates either agent's configuration root, `${CLAUDE_CONFIG_DIR:-~/.claude}` or `${CODEX_HOME:-~/.codex}`.
When the Codex configuration root exists, `dots` may create `~/.agents/skills` even if `~/.agents` is missing, provided the link's parent resolves inside `~/.agents`.
`dots status` shows each link.
`DOTS_SKILL_LINKS=0` turns linking off.

## mise behaviour this contract relies on

The supported minimum stays mise 2026.11.8; the probes below ran on 2026.11.9, and 2026.11.8 was not exercised separately.
Run on mise 2026.11.9 with an isolated configuration:
`mise config get`; `mise -E default,host-example exec`; `mise x -- COMMAND` without a tool; `mise bootstrap dotfiles apply` creating a link; `mise bootstrap plan`; `mise bootstrap dotfiles status --json`; `mise bootstrap --skip-dirty --yes` with its hook, dotfile, tools and task order as described under [`dots bootstrap`](#dots-bootstrap), and its exit 1 on a conflicting target; `mise x github:EzraCerpac/dots -- COMMAND` from a `pre-dotfiles` hook fetching the pinned version before it was installed, on macOS; the `--adopt URL` syntax in `mise bootstrap remote --help`.

The v0.4.0 restore and backup probe used mise 2026.11.9, Git, age, a private bare remote and a stubbed watcher check.
The recovery identity and a configured recipient each decrypted the undo checkpoint, and an unrelated identity could not.
The later explicit backup pushed the undo checkpoint in the history branch's ancestry.
An overlapping conflict kept the live edit, exited 1 and made only fetch-side remote requests.

The contract also relies on 0.3.5's use of other history save, sync and rollback paths, tracked-path listing, package bootstrap, services, `mise upgrade --no-prune`, `mise unuse`, tool pruning, `mise self-update` and real remote enrollment.

## Internal and subject to change

An implementation may choose and change these freely; nothing outside `dots` may depend on them:

- The format, schema and location of sync receipts, reports, logs and their retention.
- Lock files, pending markers, backup and recovery file names, temporary files, and other metadata under the configuration's Git directory or `dots`' state directory.
- Where the extracted agent skill and the embedded templates' working copies live, and how they are laid out.
- Where `dots` records the skill links and the GitHub apps it installed.
- How stages are supervised: processes, threads and any internal protocol between them, within the foreground rules of [Output and reports](#output-and-reports).
- How children reach the running `dots` on `PATH`.
- The variables `dots bootstrap` uses to pass options, such as `--restore`, to its own callbacks, `persist-selected` and `complete`.
- The exact Git and mise commands `dots` runs to produce the behaviour described here.
- The wording of messages, beyond what each line must say.
- How the completion scripts get their candidates: the protocol between a script and `dots` and its tokens, the wording of descriptions, the matching algorithm, time and file-size limits, temporary state and the environment of any mise probe; the commands and behaviour each shell sees, under [`dots completions`](#dots-completions-bashzshfish), are public.

## What 1.0 drops

| 0.3.5 | Reason |
| --- | --- |
| `DOTS_HOME` and `dots internal home` | One executable has no manager directory |
| The `dots internal` namespace | Replaced by public commands; see [Replacing 0.3.5 interfaces](#replacing-035-interfaces) |
| The sourced libraries in `lib/` | Replaced by subcommands, so hooks need no Bash |
| The separate `dots-up` renderer, its build from a checkout, path search and fallback, its `--capabilities` handshake (`dots-shared-cli/3`), `DOTS_UP_BIN`, `--backend`, its event protocol and its event, acknowledgement and stream-nonce variables | `dots` is one executable and has no full-screen interface |
| From 0.4.0: the `dots up` dashboard and its size check, the `menu` built-in, and the report browser with its forms, diagnostics tree, log search and replay navigation | Ordinary scrollback is simpler to read and keep, needs less machinery, and nothing depends on the old interface |
| From 0.4.0: `--no-wait`, the `A` key for agent handoff on a finished issue and `DOTS_AGENT_COMMAND` | As above |
| From 0.4.0: `dots internal shell-complete`, `dots-up complete` and the completion scripts in the support tree | The completion protocol is internal; `dots completions` prints the scripts |
| From 0.4.0: the quiet `uv run` wrapper of `dots remove` | The executable needs no Python or uv |
| The Bash updater fallback and its logs | There is nothing to fall back from |
| Bash, Node.js, uv and jq as requirements | The executable needs none of them |
| `dots atuin-bundle`, `dots atuin-login`, `dots_atuin_username`, `encrypted/atuin.json.age`, the Atuin status section and bootstrap stage | App enrollment belongs to configurations, as hooks and commands |
| The Tailscale bootstrap stage and its auth-key handling | As above |
| Built-in private files written from bundle values, and `dots bundle create --from-live` | As above; values reach hooks through `DOTS_BUNDLE_FD` |
| `GH_TOKEN` and a GitHub credential helper injected from a bundle value | Credentials come from the user's Git setup; a hook can configure its own children |
| The fixed list of bundle secret keys | No key has built-in meaning any more |
| Sync receipts from every earlier release | Receipts are internal |
| `SETUP_CONFIG_ROOT`, `SETUP_MISE_BIN`, `MISE_BIN` as overrides, `BREW_EXCEPTION_PLISTBUDDY` | Test fixtures and duplicates of `DOTS_ROOT` and `DOTS_MISE_BIN` |
| `checkout` and `archive` in `dots --version` | There is no installed directory to inspect |
| Running `tests/run` with `bash` | `dots` no longer requires Bash |
| `MISE_ENV` from the caller in ordinary commands | A project's selector must not change the machine's profile |
