# dots 1.0 contract

Status: Frozen for 1.0. Changes go through the coordinator as amendments to this document.

This document is the reference for what `dots` 1.0 does as seen from outside: commands, files, settings, environment, output and exit codes.
Every statement describes 1.0.
**New in 1.0** marks behaviour that 0.3.5 does not have.
**Changed from 0.3.5** marks behaviour that differs from 0.3.5 and says how.
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
| Git | `init`, `sync`, the sync stage of `up`, `backup`, `restore`, `bootstrap`, `publish`; sync needs Git 2.43 or newer |
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
The exceptions are `init`, `help`, `menu`, `--version`, `completions`, `bundle`, `brew`, `install-release-app`, `status` (which prints its program list first), and `sync` (which always exits 0).
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
| `dots status` | No program list (`tasks/status:15-45`) | Lists each program with its version, or missing with the commands that need it |
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

The seed is **pristine** while all of these hold:

- `main` and `dev` exist and point at the same commit, which has no parents, so no unrelated commit is part of it.
- Its tree contains only zero or more embedded template files, each at its template path with the template's bytes and file mode; any subset of the templates, including none, qualifies.
- `HEAD` is on `dev`, there is no `origin`, and nothing has been committed since.

The remote step:

1. `init` offers to create a remote only in the run that created the seed, and only when stdin and stdout are terminals, `gh` is on `PATH`, and `gh` reports a signed-in account.
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
dots [--plain | --verbose] [--no-wait] COMMAND [ARGS...]
```

`--plain` prints concise progress lines instead of the dashboard.
`--verbose` shows full child output.
`--plain` with `--verbose` exits 2.
`--no-wait` closes the dashboard as soon as the command finishes; `up`, `sync` and `add` do this by default.
These flags apply only to built-in commands; before a configuration command they exit 2.
Without a terminal on stdout, output is plain.
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
Every invocation may also refresh the agent skill links ([Agent skill](#agent-skill)).
Commands marked "report" save a private report.
Read-only commands never install tools.

### Inventory

| Command | Effects | Notes |
| --- | --- | --- |
| `dots`, `dots menu` | R/O | In a terminal, the command menu; otherwise the help text and exit 0 |
| `dots help`, `-h`, `--help` | R/O | Built-in commands, the helpers, then the configuration's commands |
| `dots --version` | R/O | Version and install method |
| `dots reports [COMMAND]` | R/O | Report browser; without a terminal, exit 2 |
| `dots completions bash\|zsh\|fish` | R/O | **New in 1.0.** Prints a completion script |
| `dots status` | R/O, report | Programs, profile, source, packages, tools, dotfiles, history, exceptions |
| `dots plan` | R/O, report | mise's bootstrap plan for this profile and machine |
| `dots sync --last` | R/O | Replays the latest sync receipt |
| `dots up [--mas]`, `dots update` | M, C, (R: pushes `dev` when shared), (D: hooks and recipes), report | [`dots up`](#dots-up) |
| `dots up --last` | R/O | Reopens the last `dots up` report |
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
| `dots remote HOST --profile P [OPTIONS]` | H | `--dry-run`: R/O |
| `dots bundle create OPTIONS` | M, (C when the output is inside the configuration) | Writes one encrypted file |
| `dots bundle check OPTIONS` | R/O | Decrypts and validates in memory |
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
**Changed from 0.3.5:** `root`, `profile`, `mise`, `retire-dotfiles`, `exceptions`, `enroll-history`, `install-release-app`, `brew` and `completions` are new built-in names, so a configuration command with one of these names is no longer reachable.

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
Ctrl-C cancels the current stage's entire process group, and no later stage starts.
A cancellation is not a stage failure, so the rule above for `dots up` and `dots bootstrap` does not apply to it.
A cancelled run exits with the cancelled child's own status, unchanged, when the child exits with one.
Otherwise it exits with a nonzero status, and no particular value is promised.
`dots sync` still exits 0, and its receipt records the stopped run.
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

Every problem is one issue, listed under "Needs attention" in the dashboard, in `--plain` output and in the final report.
Each issue shows the last lines of the error, the command to run next, and where the step's full log is.
Reports and logs are private to the user and live under `${XDG_STATE_HOME:-~/.local/state}/dots/`.
Recent completed reports are kept for `dots reports` and `--last`.
In the dashboard, `A` on an issue hands it to a coding agent once the run has finished.
`dots` then suspends the dashboard and runs the shell command in `DOTS_AGENT_COMMAND` in the configuration directory, with the issue (its output, log path, configuration root, host, profile and `dots` version) as the last argument.
Without `DOTS_AGENT_COMMAND`, `A` says how to set it.
`dots help` and the menu both list the configuration's commands.

A hook, a recipe or any other running stage may prompt.
While the dashboard is shown, it tells the user how to open the running stage's terminal to answer and how to return to the dashboard; the keys for this are not part of the contract.
`dots` hands the terminal to a stage by itself only for a step it knows is interactive, such as its own `sudo` authorization.
It does not detect prompts inside a hook or recipe.
With `--plain`, or without a terminal for the dashboard, the child reads the caller's stdin, which may be redirected.
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
**Changed from 0.3.5:** the program list is new, and the Atuin section is gone.

### `dots plan`

It prints mise's bootstrap plan for the selected profile and machine, without installing anything.

### `dots apply [TARGET...]`

It applies the declared dotfiles and templates, or only the named targets, through mise.

### `dots test [ARGS...]`

It runs `ROOT/tests/run ARGS...` in the configuration's mise environment.
**Changed from 0.3.5:** `tests/run` is executed directly and must be executable; 0.3.5 ran it with `bash`.

### `dots up`

`--mas` includes Mac App Store updates; `--last` reopens the last report; both together exit 2.
The stages run in this order:

1. **Pre-update checkpoint.** Saves tracked settings, after checking that every tracked entry is encrypted.
   With nothing tracked, it is skipped without attention.
   When it fails, no update starts and `dots up` exits 1.
2. **Sync**, as `dots sync`.
   In local-only mode it is skipped with one line and needs no attention.
   A sync problem makes the stage deferred: it is listed under "Needs attention", later stages still run, and on its own it never makes `dots up` exit 1.
3. **Reload.** Later stages use the configuration as sync left it.
4. **Packages.** Each available package manager with declarations for this profile and machine gets its own pass.
   Missing declared casks are installed; installed casks are upgraded unless their metadata marks them auto-updating or `dots_self_updating_casks` lists them.
5. **Tools.** Installs newly declared mise tools, then upgrades declared tools; a tool pinned to a revision keeps it, and replaced versions stay installed.
6. **Installer exceptions.** Each selected recipe; with `install-only`, the recipe gets `--install-only`.
7. **Update hooks**, in name order.
8. **Standalone mise.** Updates mise itself, unless a package manager installed it.
9. **Manager.** When the configuration declares `dots` and a stage installed another version, it reports `dots X -> Y, used from the next run`.
10. **Post-update checkpoint.** As stage 1.

**Changed from 0.3.5:** the pre-update checkpoint is the first stage; 0.3.5 ran it after sync and reload (`tasks/update:186-216,346`).
With `--mas` on macOS and declarations that select `mas`, App Store updates run alongside stages 5 to 7, and stage 8 waits for them.
Without cached administrator access, a noninteractive run defers the App Store stage; an interactive one can prompt.
Homebrew keeps the old version of anything `dots up` upgrades until the user's next cleanup.
The run finishes on the `dots` version it started with.
A failing stage does not stop later ones; Ctrl-C does, as [Exit status](#exit-status) describes.
A configuration that becomes invalid during the run stops it with exit status 1.
`dots up` never prunes software, restarts services or updates project dependencies.
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
A bare name uses mise's preferred backend and the latest version.
A backend prefix such as `npm:` or `cargo:` is recorded as a mise tool.
`brew:` prefers a formula and selects a cask only when the formula is confirmed absent; `brew-cask:` always selects a cask.
New Homebrew declarations are restricted to macOS.
`apt:`, `dnf:`, `pacman:`, `apk:` and `mas:` select those managers.
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

It searches every active configuration file, including the machine-local ones; a selector narrows the search.
A software target is named by its exact declaration (`brew-cask:firefox`), by a short name that matches exactly one declaration, or by a GitHub URL.
`mise:NAME` selects a mise tool when another manager also has `NAME`.
When another active file still declares the software, only the selected declaration goes and the software stays.
mise tools are removed with `mise unuse` and then pruned only for that tool, which keeps versions other projects use.
Native packages are removed with one targeted uninstall after checking dependents, never with zap, purge or force.
GitHub apps are moved to the Trash and their release installation is removed.
When a removal cannot be confirmed safe, the command stops; when a step fails, it restores the declarations and any staged app.
Settings and app data stay where they are.
`--keep-installed` deletes only the declaration.
`--dry-run` prints the plan and changes nothing.

**New in 1.0:** a file target is an adopted dotfile when all of these hold, with paths compared after normalization:

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
It records `SETUP_HISTORY_ORIGIN` as the history origin when set.
It never runs a full bootstrap.

`dots bootstrap complete` runs these stages, printing one result line per stage and a final count:

1. **Private inputs.** Selects, decrypts and validates a bundle as in [Bootstrap bundle](#bootstrap-bundle), and remembers the identity path in `config.local.toml`.
2. **Source repository.** Moves a fresh clone that is still on `main`, with no edits, to `dev`.
3. **Restore private settings**, only with `--restore`.
   The history origin is the one recorded in `config.local.toml` when it matches `dots_state_repo`; otherwise it comes from `dots_state_repo`, where `OWNER/NAME` becomes `https://github.com/OWNER/NAME` and a URL is used as written.
   Before restoring, bootstrap records that origin, the branch `host-ID` and the recovery identity in `config.local.toml`.
   A recorded origin that does not match `dots_state_repo`, or a recorded recovery identity other than the one in use, stops the stage with nothing changed; a recorded value is never replaced by a default.
   It then restores as `dots restore --initialize-history`, with the recovery identity and the user's normal Git credentials.
4. **Installer exceptions**, as `dots exceptions`.
5. **Bootstrap hooks**, in name order, each its own stage.
6. **Encrypted local history**, only when `dots_state_repo` or `SETUP_HISTORY_ORIGIN` is set; see [Enrollment](#enrollment).

Without a state repository or history origin, stage 6 is not listed, so a bootstrap with nothing private needs no attention.
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
It first checks that the repository is on GitHub; otherwise it exits 1 with one line naming that requirement, before looking for JJ, `gh` or gitleaks.
By default it publishes exactly one unpublished working change directly above `main@origin`.
`--bookmark NAME` publishes an existing private `wip/...` bookmark.
`--all` publishes the current stack, offering to merge `main@origin` into it first.
It fails with exit status 1 before any push or pull request when `main` does not exist on the remote.
It scans the outgoing commits with gitleaks, asks for confirmation, pushes a bookmark without force, and opens or updates the pull request.
When `dots_publish_on_behalf_of` names a person, the pull request text notes that a model, named by `DOTS_PUBLISH_MODEL` when set, wrote it on that person's behalf.
It exits 3 when conflicts stop it, and 1 on other failures.

### `dots reports [COMMAND]`

It opens the report browser, optionally on one command's reports.

### `dots completions bash|zsh|fish`

**New in 1.0.**
It prints a completion script for the built-in commands and their options.

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

Every hook, recipe, configuration command, `dots test` child and `dots mise` child that `dots` starts can run the same `dots` executable, at the same version, as `dots` on its `PATH`, even when the user started `dots` by an absolute path.
`dots` arranges this without a setting.
Every such child gets `DOTS_ROOT` and `DOTS_PROFILE`.
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

`retired-declarations` rows are `dotfiles.NAME<TAB>SOURCE`, with `SOURCE` a plain relative path without `.`, `..` or empty parts; other rows are ignored.
Before validating a merged configuration, sync validates it with each machine-local file's matching tables taken out.
A table goes only when it is exactly `[dotfiles.NAME]`, its `source` is exactly `SOURCE`, nothing else defines it, and `SOURCE` does not exist in the merged configuration.
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
- The identity is `--identity KEY`, else the remembered bundle identity, else, for a requested bundle, the recovery identity.
- The recovery identity is `--identity KEY`, else `SETUP_AGE_IDENTITY`, else the one recorded in `config.local.toml`, else `~/.config/age/keys.txt`.
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
A tracked file is never saved unless it is declared `encrypt = true`, in every spelling its declaring file uses.
Tracked files end at mode 0600.
`dots_state_repo` is required for backup, restore and enrollment, and follows [Remotes](#remotes).

### Enrollment

`dots enroll-history`, and stage 6 of `dots bootstrap complete`, prepare a machine for encrypted history:

- Each machine has its own age identity at `~/.config/age/keys.txt`; when it is missing, bootstrap generates one, creating it exclusively with mode 0600 in a mode-0700 directory.
- Another machine's private key, and the recovery identity's private key, are never copied.
- The recipients are this machine's public key, the recovery identity's public key and the bundle's recipients; they are stored only in `config.local.toml`, never in shared files.
- Bootstrap enrolls only once private recovery is ready and the recovery identity file exists; otherwise the stage is deferred.
- Enrollment needs at least one recipient other than this machine's own public key: a recovery identity with a different key, a bundle recipient or a `--recipient`.
  The default recovery identity is the machine identity's own path, so it alone never qualifies.
  Without such a recipient, enrollment exits 3 and the bootstrap stage is deferred, naming `--identity` or `SETUP_AGE_IDENTITY` as the way to supply a separate recovery identity; the machine's own key is never described as a recovery backup.
- The identity is a readable regular file outside the configuration root, named by an absolute path or one starting with `~/`.
- Every tracked file this machine keeps is set to mode 0600; a tracked path that is a symlink or lies outside `HOME` is refused.
- When no tracked file names this machine's `host-ID` profile yet, enrollment exits 3 and the bootstrap stage is deferred.
- Bootstrap then records the history origin, sets history sync to `manual`, and activates mise's history watcher service.
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

Then it saves a checkpoint, publishes it to the state repository, and says so.

### `dots restore [--initialize-history]`

- It needs a history origin in machine-local configuration that matches `dots_state_repo`, and checks the effective fetch URL as in [Remotes](#remotes) before contacting it.
- It never adopts private history into the public source checkout.
- Before anything else it checks the user's mise history watcher process and its launchd or systemd service, and stops when the watcher runs or its state cannot be confirmed; it never stops services itself.
- It never saves or publishes during restore; it fetches only, and publication stays a separate `dots backup`.
- A new store is staged and validated as a bare clone before it is installed.
- The fetched remote state is applied as the initial checkpoint, with mise's conflict checks.
- An interrupted first restore resumes on the next run instead of treating the new store as established.
- An existing store connected elsewhere is replaced only with `--initialize-history`; the old store moves to a private sibling directory and is kept, and a failed clone leaves it in place.
- It refreshes this machine's identity and recipients, sets exactly the tracked files active for this machine to mode 0600, and sets history sync to `manual`.
- It needs Git, Git authentication for the state repository, and the age identity to be present, and installs nothing.

## Environment variables

Read by `dots`:

| Variable | Meaning |
| --- | --- |
| `DOTS_ROOT` | Selects the configuration |
| `MISE_CONFIG_DIR`, `XDG_CONFIG_HOME` | Fallbacks for the configuration root |
| `DOTS_MISE_BIN` | The mise executable; else `~/.local/bin/mise`, else `mise` on `PATH` |
| `DOTS_SKILL_LINKS` | `0` turns agent skill linking off |
| `DOTS_AGENT_COMMAND` | Shell command for the dashboard's agent handoff |
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
On every invocation, `dots` keeps a copy of it in storage `dots` owns and links that copy for the agents in use:

| Link | Made when |
| --- | --- |
| `${CLAUDE_CONFIG_DIR:-~/.claude}/skills/dots` | That agent directory exists |
| `~/.agents/skills/dots` | `~/.agents` or `${CODEX_HOME:-~/.codex}` exists |

`dots` creates or replaces only links it made that still point where it left them, and only when the link's directory stays inside the agent's directory.
A directory, a file or any other link at those paths belongs to the user; `dots` leaves it alone and says so once.
`dots` never creates an agent directory.
`dots status` shows each link.
`DOTS_SKILL_LINKS=0` turns linking off.

## mise behaviour this contract relies on

The supported minimum stays mise 2026.11.8; the probes below ran on 2026.11.9, and 2026.11.8 was not exercised separately.
Run on mise 2026.11.9 with an isolated configuration:
`mise config get`; `mise -E default,host-example exec`; `mise x -- COMMAND` without a tool; `mise bootstrap dotfiles apply` creating a link; `mise bootstrap plan`; `mise bootstrap dotfiles status --json`; `mise bootstrap --skip-dirty --yes` with its hook, dotfile, tools and task order as described under [`dots bootstrap`](#dots-bootstrap), and its exit 1 on a conflicting target; `mise x github:EzraCerpac/dots -- COMMAND` from a `pre-dotfiles` hook fetching the pinned version before it was installed, on macOS; the `--adopt URL` syntax in `mise bootstrap remote --help`.
Relied on as 0.3.5 uses them, not run again for this document:
history save, sync, rollback and tracked-path listing; package bootstrap; services; `mise upgrade --no-prune`; `mise unuse`; tool pruning; `mise self-update`; a real remote enrollment.

## Internal and subject to change

An implementation may choose and change these freely; nothing outside `dots` may depend on them:

- The format, schema and location of sync receipts, reports, logs and their retention.
- Lock files, pending markers, backup and recovery file names, temporary files, and other metadata under the configuration's Git directory or `dots`' state directory.
- Where the extracted agent skill and the embedded templates' working copies live, and how they are laid out.
- Where `dots` records the skill links and the GitHub apps it installed.
- How stages are supervised: processes, pseudo-terminals, threads and any internal protocol between them.
- How children reach the running `dots` on `PATH`.
- The variables `dots bootstrap` uses to pass options, such as `--restore`, to its own callbacks, `persist-selected` and `complete`.
- The exact Git and mise commands `dots` runs to produce the behaviour described here.
- The wording of messages, beyond what each line must say.

## What 1.0 drops

| 0.3.5 | Reason |
| --- | --- |
| `DOTS_HOME` and `dots internal home` | One executable has no manager directory |
| The `dots internal` namespace | Replaced by public commands; see [Replacing 0.3.5 interfaces](#replacing-035-interfaces) |
| The sourced libraries in `lib/` | Replaced by subcommands, so hooks need no Bash |
| The separate `dots-up` renderer, its `--capabilities` handshake (`dots-shared-cli/3`), `DOTS_UP_BIN`, `--backend` and its event protocol | The dashboard is part of the one executable |
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
