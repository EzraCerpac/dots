# dots 0.5 contract

Status: Frozen for 0.5. Changes go through the coordinator as amendments to this document.
Amended 2026-11-18 to cover shell completion, what removal can target, the output of individual commands, cancellation, creating the shared agent skill directory, the 0.5 guarantee that children find the running `dots` on `PATH`, adopting and removing files, the syntax of software versions, program versions in status, enrolling and restoring history, and retired declarations.
Amended 2026-11-18 for the redesigned terminal output: `dots up` now writes to ordinary scrollback, hooks and recipes run in the foreground, `dots reports` prints a list, and bare `dots` prints help.
Amended 2026-11-18 to cover the undo checkpoints that restore saves and the scope of package passes.
Amended 2026-11-18 to cover how reports are listed and their exit status, the message of the init seed commit, ownership of GitHub apps, the output modes of updates, scan policy, protection of lockfiles, what a checkpoint captures, and settings history kept locally versus published.
Amended 2026-11-18 to cover sync diagnostics and inputs that hooks generate, how backup verifies publication and checks the history format, and the replay of update reports.
Amended 2026-11-19 to cover encryption of the checkpoints restore saves, and the context in which a validated sync pre-hook runs.
Amended 2026-11-19 to cover what source sync writes and publishes, native recovery of history, recipient fragments from trusted scopes, audits of outgoing history, resuming a suspended terminal session, and edits to paths that an interrupted checkout had already finished.
Amended 2026-11-19 to cover how directory copies are matched and protected, the groups in a live sync receipt, how far an installer-exception failure reaches, and the recipient check before restore.
Amended 2026-11-19 to remove `dots publish` and the need for JJ.
Amended 2026-11-19 to remove `dots plan` and `dots apply`, and to have local-only `dots up` apply the declared dotfiles.

This contract defines `dots` 0.5 as the outside world sees it: its commands, files, settings, environment, output and exit codes.
Each statement is about 0.5.
Labels mark where an earlier release behaves differently:
**New in 0.5** marks behaviour that 0.3.5 lacks.
**Changed from 0.3.5** marks behaviour that differs from 0.3.5, and explains the difference.
**Changed from 0.4.0** marks behaviour that 0.4.0 ships and 0.5 changes, and explains the change.
**New in 0.4.0** and **Changed in 0.4.0** mark a difference of that kind that 0.4.0 already ships; every other label marks a change that arrives with 0.5.
A citation of a 0.3.5 file, such as `bin/dots:233`, is historical evidence for one of these labels, not part of the specification.
What an implementation may decide for itself is listed under [Internal and subject to change](#internal-and-subject-to-change).
A quoted message shows what its line must convey; the exact wording is binding only where this document says so.
Examples use generic names: the profiles `laptop` and `server`, the machine `my-laptop` and the repository `OWNER/NAME`.

## Platforms and programs

`dots` runs on both macOS and Linux.
It supports Linux under WSL, but not native Windows.
`dots` ships as a single self-contained executable.
**Changed from 0.3.5:** it needs neither Bash, Node.js, uv nor jq, and no support directory beside it.

| Program | Needed by |
| --- | --- |
| mise 2026.11.8 or newer | Any command that reads the configuration through mise |
| Git | `init`, `sync`, the sync stage of `up`, `backup`, `restore`, `bootstrap`, `add FILE` (**New in 0.5**); sync needs Git 2.43 or newer |
| gitleaks | Sharing changes in `sync` and `up`; patches shown by `sync --last` |
| age, age-keygen | `bundle`, any bootstrap with a bundle, history enrollment, `backup`, `restore` |
| ssh | `remote` |
| `gh` | The remote that `init` optionally offers to create |
| Homebrew | On macOS, declarations selecting `brew:` or `brew-cask:`, and the `dots brew` helpers |
| apt, dnf, pacman, apk, mas | Declarations selecting the manager in question |

Native package managers remain external programs, called by mise and by `dots`.
Hooks, commands and recipes in a configuration need their own interpreters, such as Bash for a Bash script; `dots` itself needs none of them.
Each command checks only for the programs its requested action uses.
When one is missing, that action fails with exit status 1 and a single line naming both the program and the action.
When mise is missing, the exit status is 2.
An inspection command, or a stage with nothing to do, never fails because an optional program is missing.

## Configuration root

A configuration is a mise config directory.
`dots` picks it from the first of these that applies:

1. `DOTS_ROOT`, when set.
2. `MISE_CONFIG_DIR`, when set.
3. `${XDG_CONFIG_HOME:-$HOME/.config}/mise`.

`dots` resolves that choice to a physical path and exports it as `DOTS_ROOT` to every child.
When `config.toml` is absent there, a command that needs the configuration stops with exit status 2 and a line naming the path.
These do not need it: bare `dots`, `init`, `help`, `--version`, `completions`, `bundle`, `brew`, `install-release-app`, `status` (which prints its program list first), and `sync` (which always exits 0).
All other built-in commands, helper subcommands and configuration commands need it.
mise runs as a child rooted in the configuration, with `MISE_CONFIG_DIR` set to the root.
**Changed from 0.3.5:** every command except `dots bootstrap persist-selected` ignores the caller's `MISE_ENV`, so a project's environment selector cannot leak in; the machine's own selection comes from the files it persisted.

### Files in the root

| Path | Purpose | In the repository |
| --- | --- | --- |
| `config.toml` | The base mise configuration, loaded every time; the `dots` settings live in its `[vars]` | yes |
| `config.PROFILE.toml` | What one profile adds | yes |
| `config.host-ID.toml` | What one machine optionally adds, loaded with that machine's `host-ID` environment | no |
| `dots/` | Hooks, commands, installer exceptions, retired dotfiles and retired declarations | yes |
| `dotfiles/` | Sources of dotfiles, where `dots add FILE` places them | yes |
| `.gitleaks.toml`, `.gitleaksignore` | Rules for the secret scan in `sync` | yes |
| `.gitignore` | Keeps the machine-local files listed below out of Git | yes |
| `encrypted/bootstrap.PROFILE.json.age` | The encrypted bootstrap bundle for that profile | yes |
| `tests/run` | Tests that belong to the configuration | yes |
| `miserc.toml` | The selector of this machine's mise environment, mode 0600 | no |
| `config.local.toml` | The profile, machine ID, history settings and identity settings of this machine, mode 0600 | no |
| `mise.lock`, `mise.*.lock`, `locks/`, `.setup-state/` | Per-machine files written by mise and by older setups | no |

`dots sync` never shares a machine-local file, and it refuses a merged configuration whose ignore rules would expose one.
Root `mise.lock` and `mise.*.lock` files are never shared.
Among lockfiles, incoming ignore rules only have to protect those present at the root.
They must still protect `config.host-*.toml` selectors that do not exist yet.

### Settings in `[vars]`

Values in mise `[vars]` are strings, so a list is written as one string of words separated by spaces.
`dots` reads the following settings, and only from `ROOT/config.toml`:

| Setting | Meaning |
| --- | --- |
| `dots_profiles` | Required. The profiles that this configuration selects, such as `"laptop server"` |
| `dots_source_repo` | Optional. Fixes which repository holds the configuration itself; see [Remotes](#remotes) |
| `dots_state_repo` | Optional. The private repository that holds settings history; see [Settings history](#settings-history) |
| `dots_self_updating_casks` | Optional. Casks that `dots up` installs only when they are missing |

**Changed from 0.3.5:** both repository settings take a Git URL, where 0.3.5 took only `OWNER/NAME`.
**Changed from 0.3.5:** `dots_atuin_username` is no longer read.
A profile name matches `^[a-z0-9][a-z0-9-]*$` and must not be `base`, `local` or begin with `host-`.
`dots` refuses a profile that `dots_profiles` does not list, wherever it appears: in a command, a hook suffix or an installer exception row.
When `dots_profiles` is missing, empty or invalid, the command stops with exit status 2 and a line naming the setting and its file.

## Profiles and machine identity

Each profile is a mise environment: when the machine selects `laptop`, `config.laptop.toml` applies.
A machine ID must match `^[a-z0-9][a-z0-9-]*$`.
A machine's own mise environment is `host-ID`; it loads `config.host-ID.toml` and names that machine's stream in settings history.
The machine keeps its selection in two ignored files, both mode 0600:

- `miserc.toml` selects `PROFILE` and `host-ID`, and keeps any selectors already there after them.
- `config.local.toml` records the profile as `env.SETUP_PROFILE`, the machine ID as `env.SETUP_MACHINE_ID`, and `settings.history.sync = "manual"`; when there is a history origin, it also records that origin and the branch `host-ID`.

mise validates both files before either one changes, and each is replaced atomically.
The selection is ambiguous when `miserc.toml` selects two profiles or two `host-` IDs, or when the two files disagree on the profile or machine ID, as an interrupted write can leave them.
`dots` never picks one: each command that reads the selection stops with exit status 2 and gives `dots profile --profile PROFILE --machine-id ID` as the fix.
That command alone is exempt: it swaps every profile and `host-` selector for the pair given, keeps all other selectors, and rewrites the profile and machine ID in both files, with the same validation and atomic replacement.
Commands use `SETUP_PROFILE` and `SETUP_MACHINE_ID` from the environment when they are set, and the persisted selection otherwise.

## First run

| Behaviour | 0.3.5 | 0.5 |
| --- | --- | --- |
| Configuration files | `dots init` writes `[vars] dots_profiles`, the `dots/` layout, `.gitleaks.toml` and `.gitignore`, without overwriting anything (`tasks/init:57-107`) | The same files, from templates built into the executable, without overwriting anything |
| Profile and machine | Set by a separate `dots internal profile` command (`README.md:99-103`) | Selected by `dots init`, with the machine ID taken from the hostname |
| Git repository | Created by hand, with `main` and `dev` | Created by `dots init`, with a single seed commit on `main` and `dev` and `HEAD` on `dev` |
| Remote | Created by hand | Optional; in a terminal with `gh`, `init` offers to create one, and otherwise prints two commands |
| Remote kinds | Only GitHub (`lib/source-repo.mjs:30-48`) | GitHub or any Git URL |
| `dots_source_repo` | Needed by `sync`, the sync stage of `up`, `publish` and `remote` | Optional; `origin` on its own is enough |
| `dots up` without a remote | The sync stage stops with "missing setting: add dots_source_repo" | One line notes that the sync stage is skipped |
| `dots sync` without a remote | Prints a stop line and exits 0 | Prints a one-line receipt explaining how to add a remote, and exits 0 |
| `dots add` | Only software | Software or an existing file |
| `dots remove ./FILE` | Taken as a software name that no declaration matches | Puts an adopted file back in place |
| A missing program | Often just the child's own error, such as a failed `mise exec` | A single line that names the program and the action |
| `dots status` | Has no program list (`tasks/status:15-45`) | Lists program versions, shows an optional version it cannot read as `version unavailable`, and lists each missing program with the commands that need it |
| Internal commands in the quickstart | Needed | None needed |

With only mise and Git installed, the local quickstart is:

```sh
dots init
dots up
```

On a fresh configuration, `dots up` skips the checkpoint because nothing is tracked, skips the sync stage with one line because there is no remote, applies the declared dotfiles, and finds no packages, exceptions or hooks.
If its ordinary update stages succeed, the standalone mise update among them, which needs the network, it exits 0 and lists nothing under "Needs attention".
The skipped sync stage makes no network call and never needs attention or fails; any other stage that fails is reported and makes `dots up` exit 1, as it would on any machine.

### `dots init [DIR]`

`DIR` defaults to the configuration root; `init` creates it if it is missing.
Every line of output names one item and says whether `init` created, added or kept it.
A second run of `init` leaves everything it made before unchanged.
In every command `init` prints, `DIR` is quoted for the shell.
If `DIR` is not the root `dots` would select, `init` says so and prints every later `dots` command with `DOTS_ROOT` set, as in `DOTS_ROOT='/path/to/dotfiles' dots up`.

The Git shape is checked first.
Before writing anything, `init` looks at the repository around `DIR`:

- `DIR` lies inside another repository without being its root: `init` writes nothing and stops with exit status 2.
- `DIR` is the root of a repository that has commits or a non-empty index: an existing repository; see step 6 below.
- `DIR` is not in a repository, or is the root of one with no commits and an empty index: a new repository.

The steps then run in this order:

1. **Files.** If there is no `config.toml`, `init` writes one containing `[vars] dots_profiles = "default"` and commented hints for the optional settings.
   If `config.toml` exists without `dots_profiles`, `init` adds `dots_profiles = "default"` and leaves every other byte alone.
   It creates `dots/` with `hooks/update.d/`, `hooks/bootstrap.d/`, `commands/` and `exceptions/`, and writes `dots/README.md`, `dots/exceptions.tsv`, `dots/retired-dotfiles`, `dots/retired-declarations`, `.gitleaks.toml` and `.gitignore`, each one only if it is absent.
2. **Profile.** A valid persisted profile stays.
   If there is none, `init` selects `default` when `dots_profiles` lists it, and the first listed profile otherwise.
3. **Machine ID.** A valid persisted machine ID stays.
   If there is none, `init` takes the hostname up to its first `.`, lowercases it, replaces every run of characters outside `a-z0-9` with a single `-`, and trims `-` from both ends.
4. If the hostname is unavailable, the result is empty or the persisted selection is ambiguous, `init` stops with exit status 2.
   It keeps the files it has written and prints `dots profile --profile PROFILE --machine-id ID` and `dots init DIR`, both aimed at `DIR`; running the second finishes the remaining steps.
5. **Selection.** `init` persists the profile and machine ID the way `dots profile` does.
6. **Repository.** In a new repository, `init` makes the seed commit described below.
   In an existing repository, it commits and stages nothing, keeps every branch and all history, and reports, with its fix, each problem that sync would stop on (see [Checkout shape](#checkout-shape)).
   If `HEAD` is on `main` with commits and no uncommitted changes, and there is neither a local `dev` nor an `origin/dev`, the fix printed is `git -C DIR switch -c dev`; in every other case no branch command is printed.
7. **Remote.** Described below.

`init` exits 0 when every step it attempted succeeded, leaving aside the optional remote step.

The seed commit:

- It is a root commit, without parents.
- It holds exactly those files at embedded template paths that are regular files, are ignored neither by the repository nor by the user, and match the embedded template in bytes and file mode, whichever run of `init` wrote them.
- Every other file is left out of it and untouched on disk, including a `config.toml` that differs from its template because it predates `init` or was edited afterwards.
- If no Git identity is configured, `init` passes a synthetic author on the command line for that one commit; it never writes Git configuration.
- `main` and `dev` both point at it, with `HEAD` on `dev`.
- Its message is exactly `Initialize dots configuration`, followed by a single LF.

The seed counts as **pristine** as long as all of these hold:

- `main` and `dev` both exist and point at the same parentless commit, so no unrelated commit is part of it.
- Its tree holds nothing but zero or more embedded template files, each at its template path with the template's bytes and file mode; any subset of the templates qualifies, the empty one included.
- `HEAD` is on `dev`, no `origin` exists, and no commit has been made since.
- **New in 0.5:** a seed push without a scan, automatic or printed, requires the root commit message to equal the seed message byte for byte.
  Any other message makes the seed ineligible, and `init` never prints the message it rejected.

The remote step:

1. `init` offers to create a remote only during the run that created the seed, and only if the seed is pristine, stdin and stdout are both terminals, `gh` is on `PATH`, and `gh` reports an account that is signed in.
2. What it offers is a private GitHub repository named `dotfiles`, with no as the default answer.
3. If the user accepts, `init` creates the repository, adds it as `origin`, and pushes the verified seed without force, as described below.
4. If any part of this fails, `init` keeps the local configuration, reports what happened and what did not, and prints only the commands still to run; once `origin` has been added, that is just the push of the verified seed.
   It still exits 0.
5. A run without a terminal never creates a remote, and no later run asks again.
6. On every run in which the seed is still pristine and no remote was created, `init` prints the two commands for a new, empty remote repository:

   ```sh
   git -C DIR remote add origin URL
   git -C DIR push origin SEED_OID:refs/heads/main SEED_OID:refs/heads/dev
   ```

   Each seed push, whether automatic or printed, takes the seed's full commit ID, captured once the seed is verified as pristine, as every source, and fully qualified branch names as its destinations.
   A printed command shows that ID literally where `SEED_OID` stands, never a branch name that could later move.
7. `init` explains that these commands publish only the seed, and that `dots sync` shares later changes after scanning them.
8. For any other repository that has no `origin`, `init` prints only `git -C DIR remote add origin URL`, followed by `dots sync` on `dev`, which scans every local commit before pushing `dev`.
   It never prints a push for such a repository, so unscanned files can never seed a new remote, and the remote `main` stays however the user leaves it.

### Remotes

`dots` determines the source mode on every run:

| `dots_source_repo` | `origin` | Mode |
| --- | --- | --- |
| unset | absent | Local-only |
| unset | present | Shared through `origin` |
| set | present and matching | Shared through `origin` |
| set | absent | Diagnostic: there is no `origin` for the configured remote |
| set | present, not matching | Diagnostic: `origin` points somewhere other than the configured repository |

In local-only mode, `dots up` skips its sync stage with one line, and `dots sync` exits 0 after a one-line receipt that says how to add a remote.
Local-only `dots sync` skips its entire pipeline: it saves no history and captures no copies, makes no network call, scan or commit, and applies nothing.
**Changed from 0.4.0:** in local-only mode, `dots up` still skips its sync stage and runs both settings checkpoints, and applies the declared dotfiles locally in a separate Apply dotfiles stage, without fetching or pushing; see [`dots up`](#dots-up).
Once a configuration names a remote or has an `origin`, it is never local-only: a remote that cannot be reached is reported as offline, and the work waits for the next run.
A reachable `origin` that lacks `dev` or `main` counts as online, not offline.
In that case sync scans every local commit that no published branch contains, creates `dev` with an ordinary push, and treats the missing `main` as empty; sync never creates or moves `main`.

A repository setting is one of a GitHub `OWNER/NAME`, a GitHub URL or another Git URL.

- `OWNER/NAME` matches `^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$`.
- A GitHub URL is `https://github.com/OWNER/NAME`, `ssh://git@github.com/OWNER/NAME` or `git@github.com:OWNER/NAME`, each with or without `.git`; as a setting, it means the same as `OWNER/NAME`.
- A GitHub repository matches an `origin` written as any of those six URLs.
  **Changed from 0.3.5:** the source repository accepted five spellings of `origin`, lacking `ssh://git@github.com/OWNER/NAME`.
- Any other Git URL matches only an `origin` that is exactly the same string.
  The comparison is literal: `dots` recognizes no other spelling or alias of that repository.
- The accepted URL forms are `ssh://`, scp-like `[user@]host:path`, `git://`, `http://`, `https://` and `file://`.
- `dots` refuses a URL that contains a password or token, a `TRANSPORT::ADDRESS` remote-helper form such as `ext::`, and any value beginning with `-`.
- Credentials come from the user's ordinary Git setup: SSH keys or a configured credential helper.
  `dots` adds no protection to Git's own transport, so a repository that needs authentication belongs on `https://` or SSH, never `http://`.

`dots` hands a URL to Git as a single argument, never through a shell.
Before any fetch, clone, adoption or push, `dots` resolves the effective fetch URL and every effective push URL after this machine's Git URL rewrites, and does not contact the remote unless each of them matches the expected repository.
For the source repository, the expected one is `dots_source_repo` when set, and otherwise the `origin` URL as configured; for history, it is `dots_state_repo`.
No URL that `dots` prints, logs or stores contains credentials.
The source and state repositories are separate and must not resolve to the same repository; `dots` refuses any history push whose destination is the source repository.
For URLs that are not GitHub's, this check is the same literal comparison, so it cannot detect an alias of the source repository.
A local-only configuration can still keep settings history in a state repository.

## Commands

### Global syntax

```text
dots [--plain | --verbose] COMMAND [ARGS...]
```

`--plain` prints brief progress lines.
`--verbose` shows the children's full output.
Giving both `--plain` and `--verbose` exits 2.
**Changed from 0.4.0:** `--no-wait` no longer exists; like any unknown option, it exits 2.
Except for `dots up`, a command neither wraps its output nor waits for the user under these flags, as [Output and reports](#output-and-reports) describes.
The flags belong to built-in commands only; placed before a configuration command, they exit 2.
When stdout is not a terminal, output is plain by default.
`dots COMMAND -h` and `dots COMMAND --help` print the usage of a built-in command and exit 0.
Configuration commands, `dots test` and `dots mise` hand every argument, `--help` among them, to their child unchanged.
**Changed from 0.3.5:** `dots test --help` is passed on to `tests/run`.
An unknown command or option exits 2 with a single line that points to `dots help`.

### Effects

| Mark | The command can change |
| --- | --- |
| R/O | Nothing below |
| M | This machine: its installed software, files in `HOME` and services |
| C | The configuration checkout (its files, index and local branches) or its machine-local files |
| R | A remote repository, through a push or by creating one |
| H | A different host, reached over SSH |
| D | Anything the child it runs changes |

A mark in parentheses holds only under the stated condition.
Any invocation except `dots completions` and shell completion may also refresh the agent skill links ([Agent skill](#agent-skill)).
A command marked "report" saves a private report.
A read-only command never installs a tool.

### Inventory

| Command | Effects | Notes |
| --- | --- | --- |
| `dots` | R/O | **Changed from 0.4.0:** prints the help text and exits 0, in a terminal as well |
| `dots help`, `-h`, `--help` | R/O | Lists the built-in commands, then the helpers, then the configuration's commands |
| `dots --version` | R/O | Prints the version and how `dots` was installed |
| `dots reports [COMMAND]` | R/O | **Changed from 0.4.0:** prints one listing, newest first, whether to a terminal, a pipe or a redirected file. Exits 0 when the listing is valid and 2 on an invalid filter or usage |
| `dots completions bash\|zsh\|fish` | R/O | **New in 0.4.0.** Prints the script for shell completion |
| `dots status` | R/O, report | Shows programs, profile, source, packages, tools, dotfiles, history and exceptions |
| `dots sync --last` | R/O | Replays the most recent sync receipt |
| `dots up [--mas]`, `dots update` | M, C, (R: pushes `dev` when shared), (D: hooks and recipes), report | [`dots up`](#dots-up) |
| `dots up --last` | R/O | **Changed from 0.4.0:** prints the most recent `dots up` report and exits with the status recorded in it |
| `dots sync` | C, M, (R: pushes `dev` when shared), report | [`dots sync`](#dots-sync) |
| `dots test [ARGS...]` | D, report | Runs `tests/run` |
| `dots add [SELECTOR] [--asset GLOB] SOFTWARE...` | M, C, report | Installs the software, then records it |
| `dots add [SELECTOR] FILE...` | M, C, report | **New in 0.5.** Takes an existing dotfile under management |
| `dots remove`, `dots uninstall` `[SELECTOR] [--dry-run] [--keep-installed] TARGET...` | M, C, report | `--dry-run`: R/O; `--keep-installed`: C only |
| `dots init [DIR]` | C, (R: an accepted offer creates a GitHub repository and pushes the seed) | [`dots init`](#dots-init-dir) |
| `dots bootstrap [OPTIONS] [MISE-ARGS...]` | M, C, (D: mise hooks, bootstrap hooks, recipes), report | [`dots bootstrap`](#dots-bootstrap) |
| `dots bootstrap persist-selected` | C | Called back from a `pre-dotfiles` hook |
| `dots bootstrap complete` | M, C, D | Called back from the final `bootstrap` task |
| `dots remote HOST --profile P [OPTIONS]` | H, report | `--dry-run`: R/O |
| `dots bundle create OPTIONS` | M, (C when the output is inside the configuration), report | Writes a single encrypted file |
| `dots bundle check OPTIONS` | R/O, report | Decrypts and validates entirely in memory |
| `dots backup` | M (history store), R (state repository), report | [Settings history](#settings-history) |
| `dots restore [--initialize-history]` | M (history store, tracked files), C (machine-local files), report | Fetches, but never pushes |
| `dots NAME [ARGS...]` | D | Runs a configuration command |
| `dots root` | R/O | **New in 0.5.** Prints where the configuration root is |
| `dots profile` | R/O | **New in 0.5.** Prints which profile is selected |
| `dots profile --profile P --machine-id ID [--origin URL]` | C | Persists the given profile and machine |
| `dots mise ARGS...` | D | **New in 0.5.** Runs mise rooted in the configuration |
| `dots retire-dotfiles` | M | **New in 0.5.** Deletes dangling links of retired dotfiles |
| `dots exceptions` | M, D | Installs the installer exceptions that are missing |
| `dots enroll-history OPTIONS` | M (file modes), C (machine-local files) | Prepares the machine for encrypted history |
| `dots install-release-app OWNER/REPO` | M (`/Applications`) | **New in 0.5** as a public name; called back by mise after an install |
| `dots brew path`, `version`, `admin-available`, `hold-formula`, `hold-cask` | R/O | **New in 0.5** as commands |
| `dots brew defer-for-admin TOKEN` | R/O | **New in 0.5.** Always exits 3 |
| `dots brew record ...` | Appends to the `DOTS_CHANGES` file | **New in 0.5** |
| `dots brew update-cask ...`, `update-formula ...` | M (that one Homebrew package), appends to `DOTS_CHANGES` | **New in 0.5** as commands |

`update` is an alias for `up`, and `uninstall` for `remove`.
When a configuration command shares a name with a built-in, the built-in wins.
**Changed from 0.3.5:** `root`, `profile`, `mise`, `retire-dotfiles`, `exceptions`, `enroll-history`, `install-release-app` and `brew` are new built-in names, so a configuration command by any of these names can no longer be reached.
**Changed in 0.4.0:** `completions` is a built-in name, with the same consequence.
**Changed from 0.4.0:** `menu`, `publish`, `plan` and `apply` are no longer built-in names, and no alias is reserved for them.
An executable configuration command with one of these names runs and completes under the ordinary rules of [Configuration commands](#configuration-commands).
Without one, `dots menu`, `dots publish`, `dots plan` or `dots apply` is an unknown command and exits 2.

### Exit status

| Status | Meaning |
| --- | --- |
| 0 | Success, which includes a run whose only problems are stages that were deferred or skipped |
| 1 | Failure, which includes a program missing for the requested action |
| 2 | An error of usage or configuration: an unknown command or option, invalid arguments, no configuration, invalid `[vars]`, or no mise |
| 3 | Deferred: `dots enroll-history` when no tracked file belongs to this machine, or when no recipient exists besides this machine's own key; `dots brew defer-for-admin`; a deferral inside `dots brew update-cask` |
| 4 | Skipped: a `dots brew` command that installs, updates or holds, run anywhere but macOS |

A configuration command, `dots test` and `dots mise` each exit with their child's status.
`dots up` and `dots bootstrap` exit 0 when each stage completed, was skipped or was deferred, and 1 when any stage failed.
With valid arguments, `dots sync` always exits 0, even for a missing configuration, a missing mise or gitleaks, or a stopped run; the receipt carries the problem.
With invalid arguments, `dots sync` exits 2.
If Ctrl-Z suspends `dots` in a session that has no job-control shell, resume it by sending `SIGCONT` to the `dots` process from another session.
Ctrl-C cancels the whole process group of each active stage, such as an App Store stage that runs alongside others under `--mas`.
Once cancellation has begun, no further stage starts, wherever it falls in the order.
Ctrl-C keeps cancelling while descendants owned by a stage drain its output after the stage's leading process has exited.
`dots` waits until this cleanup is done before it saves the report or exits.
Cancellation does not count as a stage failure, so the rule above for `dots up` and `dots bootstrap` does not cover it.
When `dots` handles a cancellation for a command it supervises, it records the command as cancelled, and the command exits with the first of these statuses that applies:

1. `dots sync` exits 0, with the stopped run recorded in its receipt.
2. If the interrupted foreground child exits with a nonzero status of its own, `dots` exits with exactly that status; cleaning up the other active stages does not replace it.
3. Otherwise `dots` exits 130, even when the leading process returns 0 while cleanup is still under way.

**Changed in 0.4.0:** `dots` handles the interrupt of a command it supervises, waits for cleanup, and then exits 130.
0.4.0 exits 130 whatever the child returns, 0 included, and for a supervised `dots sync` as well; 0.5 keeps the child's own nonzero status and the sync status 0 described above.
If signalling is refused, `dots` warns on stderr that the command's processes may still be running.
A cancelled `dots remote` states on stderr and in its report that the remote host's state is unknown and needs inspecting before a retry.
**Changed from 0.3.5:** `dots bundle` usage errors exit 2 rather than 1.

### Hook and recipe outcomes

| Status | Outcome |
| --- | --- |
| 0 | completed |
| 3 | deferred: calls for attention but does not fail the run |
| 4 | skipped |
| other | failed |

**Changed from 0.3.5:** installer exception recipes follow this table, where 0.3.5 counted any nonzero recipe status as a failure.

### Output and reports

Each problem in `dots up` is a single issue, listed under "Needs attention" in its terminal output, in its `--plain` output and in the final report.
An issue shows the command to run next and, when its output was captured, the last lines of the error and where the step's full log is.
Reports and logs are private to the user and are kept under `${XDG_STATE_HOME:-~/.local/state}/dots/`.
Recent completed reports are kept there for `dots reports` and `--last`.
Both `dots help` and bare `dots` list the configuration's commands.
**Changed from 0.4.0:** the `A` key on a finished issue no longer offers to hand off to a coding agent, and `DOTS_AGENT_COMMAND` is not read.

**Changed from 0.4.0:** `dots` has no full-screen interface.
In an interactive terminal, by default, the full `dots up`, also run as `dots update`, writes into the terminal's ordinary scrollback:

- a line as each step starts;
- the output of a foreground step, which owns the terminal while it runs;
- a line with each step's result;
- a final outcome line, such as `dots up · Complete` or `dots up · Failed (exit 1)`;
- "Needs attention", with every issue, its next action and, when its output was captured, the location of its log;
- `Details and logs · dots up --last` when the final report is saved successfully.

It uses no alternate screen, navigation keys, spinner or minimum terminal size, and never moves the cursor across child output.
A step whose output is recorded privately may update its own progress line once, and completes that line before any child can print or prompt.
As in 0.4.0, in concise output, headless runs and `--plain` included, hooks and recipes read the caller's stdin, and `dots` captures their stdout and stderr in private logs that supply the failure tails.
`--verbose` also mirrors the captured output, with the same logs and failure tails.

`dots reports`, `dots up --last`, the sync receipt and the `dots sync --last` pager have displays of their own, and the rest of this paragraph does not apply to them.
Every other individual command that `dots` supervises, `dots status` among them, writes straight into the terminal's scrollback.
Its child runs in the caller's foreground, so a native prompt from `sudo` or `ssh`, for example, reads the terminal as usual.
It reads the caller's stdin, which may be redirected, and its stdout and stderr reach the caller's own separately and byte for byte, even when the reader is slow.
When native transports run as part of [settings history](#settings-history), `dots` reports a diagnostic of its own and keeps the transport's verbatim error out of these streams.
Around this output, `dots` adds no banner, no footer and no second copy of a diagnostic.
When such a command is marked "report", its report keeps the outcomes of its stages and its exit status, but neither the child's raw output nor a log path that does not exist.

A report of a failure, or of an issue that needs attention, names the command to rerun; a report of a success need not.
`dots sync` may keep its backends' output in a private log, and shows file contents only as [Receipt](#receipt) permits.
If any report cannot be written or completed, `dots` says so on stderr, naming the command, and still exits with that command's own result.

In interactive scrollback, hooks, recipes and the steps `dots` knows to be interactive, such as its own `sudo` authorization, run in the foreground and may prompt.
**Changed from 0.4.0:** in this mode a foreground step has the caller's stdin, stdout, stderr and controlling terminal from the moment it starts, without a handoff.
Only one step writes to the terminal at a time, and `dots` never reads the reply to a prompt.
`dots` makes no attempt to detect prompts.
In interactive scrollback, `dots` records the output of the other stages privately, for logs and issue tails.
When a foreground step fails in interactive scrollback, `dots` reports its outcome, exit status and next action, and says `Output was shown on the terminal` instead of giving a tail and log location.
`dots` restores the caller's terminal after a normal exit, a cancellation or a panic that unwinds.
A `SIGKILL` or an abort leaves it no chance to do so.

## Command reference

### `dots --version`

It prints the version and the way `dots` was installed: `homebrew`, `mise` or `cargo` when the executable's location reveals it, and `other` otherwise.
**Changed from 0.3.5:** `checkout` and `archive` are gone.

### `dots status`

It prints the program list first, followed by the version, the agent skill links, the selected profile and machine ID, the source mode and repository, the state of declared packages, the tool state, the dotfile and history state, and the installer exceptions.
Without a configuration, it prints the program list and exits 2.
With a configuration but no usable mise, it prints the program list, the version and the skill links, and then exits 2.
Otherwise it exits 1 when a section fails, and 0 when every section succeeded or merely needs attention.
Neither a missing optional program nor a source diagnostic fails it.
**New in 0.5:** when a found optional program's version cannot be read, the program list gives it the informational label `version unavailable`.
**Changed in 0.4.0:** it names the host and profile, prints each item once under its group with its status and summary, and puts beneath it any action the item supplies, even for an item that is ok, such as a pending sync.
Each of these fields appears as a single line free of control characters.
A selected profile missing from `dots_profiles` produces one diagnostic on stderr and exit status 2, and the report keeps that diagnosis and its next action as a startup issue.
**Changed from 0.3.5:** the program list is new, and the Atuin section has been removed.

### `dots test [ARGS...]`

It runs `ROOT/tests/run ARGS...` within the configuration's mise environment.
**Changed from 0.3.5:** `tests/run` is executed directly and has to be executable; 0.3.5 ran it with `bash`.

### `dots up`

`--mas` adds Mac App Store updates; `--last` prints the last saved `dots up` report and exits with the status recorded in it; giving both exits 2.
An ordinary update appears as `updated  item  old → new` in live output, in `--plain` and in replay.
`--last` replays the final report byte for byte, the successful-save footer `Details and logs · dots up --last` included.
Replay leaves out live child and stage output.
The stages run in this order:

1. **Pre-update checkpoint.** Saves tracked settings once it has checked that every tracked entry is encrypted.
   When nothing is tracked, it is skipped without needing attention.
   If it fails, no update starts and `dots up` exits 1.
2. **Sync**, as `dots sync`.
   In local-only mode it is skipped with one line and needs no attention, and stage 4 applies the declared dotfiles instead.
   A sync problem defers the stage: the problem is listed under "Needs attention", later stages still run, and by itself it never makes `dots up` exit 1.
3. **Reload.** Later stages see the configuration as sync left it.
4. **Apply dotfiles**, only in local-only mode; in shared mode the sync stage applies dotfiles and this stage does not run.
   It runs mise's native dotfile apply on the selected configuration as it is on disk, including uncommitted edits, without asking, forcing or pruning, and commits nothing.
   A native failure makes the stage deferred, as a sync problem does: it is listed under "Needs attention", later stages still run, and on its own it never makes `dots up` exit 1.
5. **Packages.** Every available package manager that has declarations for this profile and machine gets a pass of its own.
   The apply pass respects explicit `state = "absent"` declarations, even when they are a manager's only declarations.
   The outcome is whatever native mise apply decides, including when the manager cannot remove a package.
   Every operation keeps the declared package pins and exclusions, such as holds.
   A pass limited to one manager can run when all of that manager's declarations are eligible for the operation.
   A pass without eligible work is skipped.
   If eligible work is left that the pass cannot select while keeping pins and exclusions, the manager stage fails before it changes any package, and says why.
   Other independent stages carry on under the usual `dots up` rules.
   An eligible cask declared present is installed when it is missing.
   An eligible installed cask declared present is upgraded, unless its metadata marks it as auto-updating or `dots_self_updating_casks` lists it.
6. **Tools.** Installs newly declared mise tools, then upgrades the declared tools; a tool pinned to a revision stays at that revision, and versions that are replaced stay installed.
7. **Installer exceptions.** Every selected recipe; with `install-only`, the recipe receives `--install-only`.
8. **Update hooks**, in name order.
9. **Standalone mise.** Updates mise itself, unless a package manager installed it.
10. **Manager.** If the configuration declares `dots` and a stage installed a different version, it reports `dots X -> Y, used from the next run`.
11. **Post-update checkpoint.** As stage 1.

**Changed from 0.3.5:** the pre-update checkpoint is the first stage, where 0.3.5 ran it after sync and reload (`tasks/update:186-216,346`).
On macOS, with `--mas` and declarations that select `mas`, App Store updates run alongside stages 6 to 8, and stage 9 waits for them to finish.
Without cached administrator access, a run without a terminal defers the App Store stage.
An interactive run asks for administrator access in the foreground, if it needs it, before the App Store stage starts.
**Changed from 0.4.0:** the App Store worker runs noninteractively and detached from the terminal, writing its output to its log.
`dots up` reports the worker's result at the next stage boundary.
If its administrator access expires, the stage is deferred with a next action, and it never prompts again while another step owns the terminal.
Homebrew keeps the previous version of everything `dots up` upgrades until the user next cleans up.
The run completes on the `dots` version it began with.
A failed stage does not stop the stages after it; Ctrl-C does, as [Exit status](#exit-status) describes.
If the configuration becomes invalid during the run, the run stops with exit status 1.
`dots up` never prunes undeclared packages or old versions, never restarts services and never updates project dependencies.
Its sync stage pushes `dev` just as `dots sync` does; `dots up` never pushes `main` or opens a pull request.
Changes to packages, tools and the standalone mise come from comparing the state before and after their stage; hooks and recipes report theirs through `DOTS_CHANGES`; the sync stage reports the configuration revision it adopted.

### `dots sync`

`dots sync` accepts no argument other than `--last`.

#### Checkout shape

- The checkout is a plain Git checkout at the root of the repository, with `HEAD` on `dev` and at least one commit.
- Sync stops with a single line on a detached `HEAD`, on another branch, with a `.jj` directory, or with state left by a pre-0.5 sync layout.
- `dots bootstrap` moves a fresh clone that is still on `main` and has no edits to `dev`; any other shape has to be converted by hand.
- Sync skips with a single line while a merge, rebase, cherry-pick, revert or bisect is under way, or while another sync of the same repository is running.
- A lock that a dead process on this host left behind is reclaimed.
- Sync skips with a single line when Git is older than 2.43.

#### One run

1. Saves tracked history, and refuses any tracked entry that is not `encrypt = true`.
   When a known configuration problem is refused before the checkpoint, sync reports a bounded, vetted reason and tells the user how to fix the active encryption setting or the refused declaration.
   When the native save fails, sync reports a bounded, vetted reason and tells the user to retry the local checkpoint with `dots sync`.
2. Captures edits to copied dotfiles: when a deployed copy has changed and its source still matches the last common version, the deployed bytes become the source.
   When the copy and the source have both changed, sync leaves both alone and reports it.
3. Stages every public edit, tracked or unignored, as a candidate commit, leaving machine-local files out, and moves neither `dev` nor the real index.
   Sync keeps any existing staging and refuses staged edits that are ambiguous.
4. Scans the candidate with gitleaks before `dev` or the index moves.
   As in 0.4.0, sync scans with the checkout's live gitleaks rules and ignore entries, so a local exception already governs the scan of the change that introduces it.
   The native Git scan covers the diffs of outgoing commits, merge diffs against their first parent included, and checks which commits any published branch already contains.
   A flagged file stays on disk but leaves the candidate, and what remains of the candidate is scanned again; `dev` advances only to a clean candidate.
   A rejected candidate never reaches `dev`, the real index or the push, and its edits remain on disk.
   If gitleaks does not work, or a scan fails, nothing is shared or pushed.
   The push is blocked by a flagged commit already in local history that no published branch contains, and by any commit that ever tracked a machine-local file; sync never rewrites the branch to get rid of it.
5. Fetches `dev` and `main`, or carries on offline.
6. Plans the merge of local work with `origin/dev` and then with `origin/main`, without touching the live checkout.
   On a conflicting hunk, `main` beats `dev`, and `dev` beats local work.
   For binary, file-type and delete conflicts, the published file is taken whole.
   Both parents remain in history, so the losing version can be recovered.
7. Validates the merged configuration together with this machine's local files; a result that fails validation is neither pushed nor applied.
8. Pushes exactly the validated candidate OID to `refs/heads/dev` at the checked push destination, on condition that the remote holds the expected old OID or no such branch, and only to create the branch or fast-forward it.
   It retries, at most three times, only once it has confirmed a race with a moving tip in which no published history was withdrawn.
   If published history vanishes during a retry, sync keeps the local work and applies nothing from the withdrawn history.
   A push that runs longer than 60 seconds is stopped, and Git never prompts for credentials.
   Clear success in Git's porcelain output for that ref update is enough to report it as shared.
   **Changed from 0.4.0:** when the outcome of an attempted push is indeterminate, sync stops before any retry or apply, and the live and replayed receipts report publication as unconfirmed.
   The issue gives the candidate OID and the expected old remote `dev` OID, or its absence, and tells the user to compare `refs/heads/dev` at the push endpoint.
9. Updates the checkout and applies dotfiles under the guarded rules that follow.

#### Guarded checkout and copies

- Before it writes, sync checks the incoming paths for local obstructions, among them ignored files, directories and symlinks.
- A blocked path stops the checkout, while unrelated edits can still be shared once publication is confirmed.
- Dots protects its checkout and its own writes from edits made in between, and reports a competing change with a recovery action.
- Before a fresh snapshot, sync completes an interrupted checkout, index installation or `dev` update only as long as the checkout, the index and `dev` still match the recorded recovery state.
  If they do not, it keeps the competing tip or edits, the private originals and the pending recovery, and reports the recorded before and after revisions, the actual recovery paths, and how to reconcile and run again.
  An edit or deletion at a path the interrupted checkout had already finished is not a competing edit: it is later local work, which sync handles in the same run once recovery finishes.
- When dots takes over or displaces an existing file, it verifies the displaced file and installs the replacement only at an empty path, exclusively.
- If another writer claims the path, sync leaves it to that writer and prints where the moved file went.
- The original inode stays at a printed private path on the same filesystem, so an editor that still holds it open can finish saving.
- Before replacing an existing file in a directory copy, sync compares it with the source last deployed at that path.
- If bytes and permissions both match, sync treats the file as deployed, whoever created it, and updates it to the new source without reporting it as kept.
- If the bytes or verified permissions at the target differ, including through a change noticed after the first comparison, sync leaves the file in place and reports it as kept.
- If sync notices an edit to the moved original before it installs the replacement, it puts the edited original back at the target where it can.
- If the replacement is already in place, or the original cannot be put back, sync keeps the original with its edits at the printed private recovery path.
- If the deployed mode cannot be verified, sync updates the file only when its bytes match the source last deployed and its current mode can be kept.
- Local files that match nothing stay where they are.
- Recovery files stay on the target's filesystem; where that is impossible, sync reports the path and keeps both versions.
- Other targets are applied in dependency order, with the usual mise hooks.
- Ordinary template and inline installs apply natively through mise, on the live targets.
  Mise 2026.11.9 can overwrite, and so lose, a file that another writer creates between native planning and installation.
- Sync uses native reload matching and runs each matching reload command once after that run's writes, reporting any failure with a next action.
- As in 0.4.0, a validated pre-hook runs with the real configuration checkout as both its working directory and its `DOTS_ROOT`.
  Public source or configuration edits that it generates remain there for the next ordinary screened sync.
  Within the same run, entries already validated use the generated source bytes and permissions, honour `create_if_missing`, and may consume the output of a declared dependency that succeeded.
  A dependency that was kept or failed blocks whatever depends on it.
  Dots' own writes stay guarded, and no omitted input, changed selector, target, source path or operation, and no newly added hook, can authorize an extra write.

Sync never force-pushes, never skips validation and never writes conflict markers into live files.
Every issue comes with a diagnostic and a recovery action.

#### Receipt

- Every finished sync, including the one inside `dots up`, prints a receipt and records it, with private permissions, for replay.
- If recording fails, sync still prints this run's receipt, warns that it could not be saved for replay, and exits 0; the run's outcome stays the same, and `sync --last` may show an earlier sync or none.
- A clean receipt is a one-line summary of local work, incoming changes and the push.
- When the run has issues, the receipt prints them, with their next actions, ahead of the summary.
- Groups that are not empty may follow the summary.
- For shared files, local saves, checkout updates and work finished after an interruption, the live receipt lists up to five paths per group.
- When stdin and stdout are both terminals, a standalone sync may show a complete small text diff covering up to two paths and twelve displayed lines.
- Piped output, `--plain` output and saved reports hold metadata only.
- **New in 0.5:** live and saved sync receipts leave out raw child stderr, and may carry vetted scanner metadata such as rule, path and line.
- A missing gitleaks produces a receipt such as `dots sync: kept 2 files on disk · pulled dev +1, main +0 · not pushed`, headed by the issue `NOT SHARED: gitleaks not found; local edits kept on disk, nothing pushed` and the action to install it.

`dots sync --last` does nothing but replay this checkout's latest receipt from the recorded Git revisions, with neither the network nor mise.
It spends at most three seconds screening patches and keeps at most 1 MiB for the pager.
It shows a patch only when gitleaks returns a clean JSON report for both versions of the file and for the text displayed; otherwise it lists the file without its patch.
Binary, encrypted and unverified changes appear as file entries.
**Changed from 0.3.5:** receipts that any earlier release wrote are ignored, and `sync --last` then reports that no receipt exists.

### Requests to `dots add` and `dots remove`

Every argument after the options is a request of its own.
A request beginning with `/`, `~/`, `./` or `../` is a **file**.
Any other request is **software**: a GitHub URL, a name with a backend prefix such as `npm:prettier`, or a bare name.
A bare name is software even if it begins with a dot, so to adopt `~/.zshrc` from `HOME`, write `./.zshrc`.
A relative file request resolves against the caller's directory.
The selectors are `--base` (`config.toml`), `--profile P` (`config.P.toml`) and `--path FILE`, and at most one of them may be given.
With no selector, `dots add` writes to the selected profile's file, and stops with exit status 2 if no profile is selected.
A selected file that does not exist yet, such as a new `config.PROFILE.toml`, is created only after a request succeeds; a failed request leaves behind no new or empty file.
Combining `--asset` or `--keep-installed` with a file request exits 2.
For file requests, `dots add` and `dots remove` accept a selected file only if, after `--path` and any symlinks are resolved, it is `ROOT/config.toml` or `ROOT/config.PROFILE.toml` for a profile in `dots_profiles`.
They refuse any other selected file, including one that a symlink leads out of that set, with exit status 2 before changing anything, so every adoption stays reversible through the selector that made it.
For software requests, `--path FILE` keeps its usual meaning.

### `dots add SOFTWARE...`

It installs first, and records only what was installed.
A bare tool name takes mise's preferred backend, at `latest` by default.
A request with a backend prefix such as `npm:` or `cargo:` is recorded as a mise tool.
A mise tool request accepts `TOOL@VERSION` and the supported inline tool options in brackets, as in `TOOL@VERSION[KEY=VALUE]`.
Existing inline tool options are kept unless the request changes them explicitly.
In `npm:@scope/name`, the leading `@` belongs to the package name.
`brew:` prefers a formula and picks a cask only once the formula is confirmed absent; `brew-cask:` always picks a cask.
New Homebrew declarations are limited to macOS.
`apt:`, `dnf:`, `pacman:`, `apk:` and `mas:` select the manager of that name.
For `brew:`, `brew-cask:` and `mas:`, an `@` belongs to the name.
For `apt:`, `dnf:`, `pacman:` and `apk:`, `NAME@VERSION` gives the version for a new declaration.
A GitHub URL whose release holds a prebuilt command-line tool uses mise's GitHub backend.
A macOS app shipped in a ZIP or tar release is recorded together with a post-install hook that runs `dots install-release-app OWNER/REPO`.
DMG and pkg releases are refused, and so are repositories with no release; no script from the repository is run.
`--asset GLOB` names a single release asset and requires exactly one GitHub URL.
A package held by an installer exception is refused before anything is installed.
If the install fails, `dots add` shows the declaration and asks whether to record it so it can be retried later; the default is no, and a run without a terminal never records it.
With several requests, the ones that succeeded earlier stay recorded, and the command stops at the first failure.

### `dots add FILE...`

**New in 0.5.**
For a file request, `REL` is the target's path relative to `HOME`, the source is `ROOT/dotfiles/REL`, and the declaration is `"~/REL" = { source = "dotfiles/REL", mode = "symlink" }` under `[dotfiles]` in the selected file.
`dots add FILE` needs Git to check ignore rules, even for a root that is not a repository.
Such a root is not turned into a repository.
Without Git, it prints a single diagnostic naming Git and the action, and exits 1.

`dots add` first looks for an exact earlier adoption: the target is a symlink to that source, the source is a regular file, and the selected file contains that declaration.
Such a request changes nothing, reports the file as already adopted, and succeeds.

Otherwise it refuses the following, with exit status 2 and no change:

- a target that is not a regular file, or whose resolved path lies outside `HOME` or inside the configuration root or `dots`' own state;
- a target that has a symlink among its parent directories below `HOME`;
- a source path with a symlink or a non-directory among its parent directories below `ROOT`, or one that the configuration's Git ignore rules would exclude;
- a source path that already exists, or that some configuration file in `ROOT` already uses, for whatever profile;
- a target that some configuration file in `ROOT` already declares;
- two requests in the same invocation that resolve to one target.

It then copies the target's bytes and mode to the source, adds the declaration while keeping every other byte, comment and the order of the file, and replaces the target with a symlink to the source.
If the target changes meanwhile, or any step fails, `dots add` restores the original target, removes the source and declaration it added, never overwrites a file it did not create, and exits 1.
After an interruption, a rerun either completes the adoption or reports exactly what remains to reconcile.
The target is then a link, and `dots status` lists it.
The next sync, which scans the new source first, carries it to other machines.

### `dots remove TARGET...`

For software targets, it searches every active configuration file, machine-local ones included, and a selector narrows that search.
A software target is identified by its exact declaration (`brew-cask:firefox`), by a short name that identifies exactly one piece of software, or by a GitHub URL.
`mise:NAME` picks the mise tool when another manager also has a `NAME`.
If another active file still declares the software, only the selected declaration is removed and the software stays installed.
A mise tool is removed with `mise unuse` and then pruned for that tool alone, which keeps the versions other projects use.
A native package is removed with a single targeted uninstall once its dependents are checked, never with zap, purge or force.
GitHub apps that pass the [ownership checks](#dots-install-release-app-ownerrepo) go to the Trash, and their release installation is removed.
If a removal cannot be confirmed as safe, the command stops; if a step fails, it restores the declarations and any staged app.
Settings and app data are left where they are.
`--keep-installed` removes only the declaration.
`--dry-run` only prints the plan, and changes nothing.

**Changed in 0.4.0:** a short name taken from any declaration or alias of the software matches it, not just one from the first; the bundle name of an app that `dots` owns matches as before.
Names count pieces of software, not declarations or spellings: software declared in several files, or under several spellings, counts once.
A software target that matches nothing prints a single diagnostic on stderr and exits 1, changing nothing.
The diagnostic suggests a name only when exactly one piece of software is a close match and no installer exception holds it; with several candidates it never guesses.
If the app ownership records cannot be read, a full removal suggests no GitHub app, though `--keep-installed` still may.
An exact name that several pieces of software share lists their qualified names, `mise:NAME` for a mise tool, as alternatives.

**New in 0.4.0:** after `dots remove`, shell completion offers the software that the command could remove:

- Tools under `[tools]`, packages under `[bootstrap.packages]` for a supported manager, and GitHub apps that have a valid local ownership record.
- With no selector, it reads the active mise files that physically lie in the selected root, machine-local ones included, and ignores the caller's `MISE_ENV`.
- With `--base`, `--profile` or `--path`, it reads that one file only, without discovering active files; a relative `--path` resolves against the caller's directory.
- An app whose ownership record outlived its declaration, as happens after `--keep-installed`, is offered with no selector, and with one only when the selected file declares it.
- Every spelling of a declaration yields case-insensitive names: the name without its prefix, including any tap or repository path, and its last path component; an owned app adds its bundle name without `.app`.
- A name that several pieces of software share, counted as above, gives way to their qualified names, `mise:NAME` for a mise tool.
- A typed prefix matches qualified names as well: after `npm:`, completion may offer `npm:@scope/tool` even when no other software shares that short name.
- Every qualified name on offer is one that removal accepts, such as an exact declaration or `mise:NAME`.
- Targets already on the command line, targets that an installer exception holds under the same rule as removal, and names that are not valid targets are left out.
- Where the shell shows descriptions, each candidate's description says what kind of software it is.

**New in 0.5:** with no selector, `dots remove FILE` searches every configuration file in `ROOT`, whatever the active profile or machine.
A selector restricts the file that may supply the declaration to the base file or the file of a listed profile, as [Requests to `dots add` and `dots remove`](#requests-to-dots-add-and-dots-remove) describes.
Selector or not, uniqueness is checked across every configuration file in `ROOT`.
A file target is an adopted dotfile when all of the following hold, comparing paths after normalization:

- the target is a symlink whose destination is exactly `ROOT/dotfiles/REL`, where `REL` is the target's path relative to `HOME`;
- the source is a regular file, with no symlink or non-directory among its parent directories below `ROOT`;
- across all of `ROOT`'s configuration files, for any profile or machine, exactly one declaration is `"~/REL" = { source = "dotfiles/REL", mode = "symlink" }`, and it is in the selected file when a selector is given;
- no other declaration either names that target or uses that source.

For such a target, `dots remove` replaces the link with a regular file that has the source's current bytes and mode, and then removes the declaration and the source.
Edits made through the link are kept that way.
Any other file target, and any target whose uniqueness it cannot prove, for instance because a configuration file does not parse, is refused with exit status 2 and no change.
If the link, the source or the configuration file changes meanwhile, or a step fails, it stops with exit status 1.
It then restores the link, the source with its mode, and the configuration file, but only where nothing has changed them since; a later edit is kept, and `dots` never overwrites a file or an edit that it did not make.
After an interruption, a rerun either completes the removal or reports exactly what remains to reconcile.
It needs no record of earlier adoptions.

### `dots bootstrap`

```text
dots bootstrap [--bundle FILE] [--identity KEY] [--restore ID] [MISE-BOOTSTRAP-ARGS...]
```

It runs mise's native bootstrap of the whole declared setup with `--skip-dirty`, exporting `DOTS_ROOT`.
On mise 2026.11.9, a dotfile target that already held different bytes made the native bootstrap exit 1 in spite of `--skip-dirty`, so some collisions are not skipped.
`dots bootstrap` then reports the native failure, exits 1 and leaves the target's bytes untouched.
If the native bootstrap stops before its final task, `dots bootstrap complete` does not run, and `dots bootstrap` says so rather than running it on its own.
Relative `--bundle` and `--identity` paths resolve against the caller's directory.
`--identity` names the external age identity used for both the bundle and recovery.
`--restore ID` restores the settings history of machine `ID`, which must be a valid machine ID; it cannot be combined with `--only`.
Under `--restore`, `dots` skips mise's services step in the native bootstrap, so no history watcher runs before the restore; the completion stage restores first, and enrollment then activates the watcher.

A configuration links bootstrap to `dots` in two places:

```toml
# config.laptop.toml
[bootstrap.hooks]
pre-dotfiles = 'SETUP_PROFILE=laptop "$(command -v mise || echo "$HOME/.local/bin/mise")" x github:EzraCerpac/dots -- dots bootstrap persist-selected'

# config.toml
[tools]
"github:EzraCerpac/dots" = "0.5.0"

[tasks.bootstrap]
description = "Finish the dots bootstrap"
run = 'dots bootstrap complete'
raw = true
```

On mise 2026.11.9, with nothing but `/usr/bin:/bin` on `PATH`, a native bootstrap ran the `pre-dotfiles` hook before applying dotfiles and without `mise` on `PATH`, then applied dotfiles, ran the `post-dotfiles` hook and the tools step, and ran the `bootstrap` task last.
That is why the `pre-dotfiles` hook finds the standalone mise for itself and runs `dots` through `mise x github:EzraCerpac/dots`, which uses the version pinned by `[tools]` in `config.toml`, 0.5.0 in this example.
On mise 2026.11.9 on macOS, with an empty temporary `HOME` and empty data, state and cache directories, and `"github:EzraCerpac/dots" = "0.3.5"` in `[tools]`, a `pre-dotfiles` hook that ran `mise x github:EzraCerpac/dots -- dots --version` through the standalone mise fetched and installed the pinned 0.3.5 before it existed and printed its version; dotfiles were then applied, the tools step found `dots` installed already, and the bootstrap exited 0.
This verifies resolving the pin and launching it from the hook, not the 0.5 callback itself.

`dots bootstrap persist-selected` persists the profile and machine ID the way `dots profile` does.
An existing persisted profile and machine ID are kept.
Failing those, it takes the profile from `SETUP_PROFILE`, or else from a single profile named in the caller's `MISE_ENV`, and the machine ID from `SETUP_MACHINE_ID`, or else from a persisted `host-` selector, or else `auto-` followed by a random identifier.
When `--restore ID` is in effect, the machine ID is `ID`, and a different persisted ID makes it stop with exit status 2.
It records `SETUP_HISTORY_ORIGIN` as the history origin when that is set, and keeps any recorded history origin that conflicts with it.
A full bootstrap is never run from it.

`dots bootstrap complete` runs the following stages, printing a result line for each and a final count:

1. **Private inputs.** Selects, decrypts and validates a bundle as [Bootstrap bundle](#bootstrap-bundle) describes, and remembers the identity path in `config.local.toml`.
2. **Source repository.** Moves a fresh clone that is still on `main` and has no edits to `dev`.
3. **Restore private settings**, only with `--restore`.
   The selected recovery identity must already exist for a restore.
   The history origin is the one recorded in `config.local.toml` if it matches `dots_state_repo`; otherwise it is taken from `dots_state_repo`, with `OWNER/NAME` turned into `https://github.com/OWNER/NAME` and a URL used as written.
   Before it restores, bootstrap records that origin, the branch `host-ID` and the recovery identity in `config.local.toml`.
   If a recorded origin does not match `dots_state_repo`, or a recorded recovery identity differs from the one in use, the stage stops without changing anything; a default never replaces a recorded value.
   It then restores as `dots restore --initialize-history` does, with the recovery identity and the user's normal Git credentials.
4. **Installer exceptions**, as `dots exceptions`.
5. **Bootstrap hooks**, in name order, each as a stage of its own.
6. **Encrypted local history**, when `dots_state_repo` or `SETUP_HISTORY_ORIGIN` is set, as [Enrollment](#enrollment) describes.

**Changed from 0.4.0:** 0.4.0 always listed the history stage, while 0.5 lists it only when the state repository or history origin is set.
`SETUP_HISTORY_ORIGIN` on its own is enough to list the stage, whatever the source repository's origin.
`dots bootstrap complete` exits 1 if any stage failed, and 0 otherwise.
**Changed from 0.3.5:** the Atuin, Tailscale and private-file stages are gone, as is the injection of `GH_TOKEN` from a bundle value.

For a restore, the user's normal Git authentication for the state repository must already work when bootstrap starts: SSH keys or a configured credential helper.
Bootstrap hooks run after the restore stage, so none of them can provide that authentication.

### `dots remote`

```text
dots remote HOST --profile P [--dry-run] [--yes] [--identity-file FILE] [--port N] [--connect-timeout S] [--github-relay-repo R] [--github-relay-read-only]
```

It runs mise's remote bootstrap on `HOST` over SSH, which installs mise there, adopts the source repository and selects profile `P`.
`P` must be listed in `dots_profiles`.
The source is `dots_source_repo`, or else the `origin` URL; with neither, it exits 2.
**Changed from 0.3.5:** the source can be a Git URL.
On 2026.11.9, `mise bootstrap remote --help` accepts a URL for `--adopt`; a real remote enrollment from a URL has not been verified.

As in 0.4.0, `dots remote` does not touch the target's history settings.
When history is enabled on the target, mise keeps tracked files unencrypted, in plaintext, in the target's local history.
Declaring the file with `encrypt = true`, or turning history off on the target, stops new plaintext checkpoints of that file.
The new [backup publication audit](#dots-backup) checks outgoing history on its own terms and honours mise's `allow_plaintext_history` opt-in.

### `dots bundle`

```text
dots bundle create --input PRIVATE.json --output BUNDLE.age --recipient age1... [--recipient age1...]...
dots bundle check --bundle BUNDLE.age --identity KEY
```

`create` validates the input, encrypts it with age for every recipient, and creates the output exclusively, with mode 0600, refusing a path that already exists.
It never writes plaintext anywhere.
`check` decrypts and validates entirely in memory.
**Changed from 0.3.5:** `--from-live` has been removed.

### `dots reports [COMMAND]`

**Changed from 0.4.0:** it prints recent completed reports, newest first, optionally only one command's.
`plan` and `apply` remain valid filters, so reports saved by those former commands can still be listed.
Every line gives the command, the time, the outcome, the exit status and where the report is.
The listing is the same whether it goes to a terminal, a pipe or a redirected file.
A valid listing exits 0; an invalid command filter or usage exits 2.

### `dots completions bash|zsh|fish`

**New in 0.4.0.**
It takes exactly one argument and prints that shell's completion script on stdout, nothing else, with exit status 0.
It runs ahead of any setup, without mise, and leaves the agent skill links alone.
A missing or unknown shell, or any other argument, prints usage on stderr and exits 2.
From 0.5, `-h` and `--help` behave as [Global syntax](#global-syntax) describes; 0.4.0 treats them as an unknown shell.

Homebrew puts the scripts in its standard completion locations, and upgrades them there: `dots` for Bash, `_dots` for Zsh and `dots.fish` for Fish.
When `dots` comes from mise, a release binary or Cargo, or runs in a Homebrew shell that does not load completions on its own, the user adds one line to the shell's interactive startup:

| Shell | Startup file | Line to add |
| --- | --- | --- |
| Fish | `~/.config/fish/config.fish` | `dots completions fish \| source` |
| Zsh | `~/.zshrc`, after `compinit` | `source <(dots completions zsh)` |
| Bash | `~/.bashrc` | `eval "$(dots completions bash)"` |

For a Bash login shell, the login profile either sources `~/.bashrc` or contains the loader itself.
From then on, every new shell completes with the active release.
`dots` never edits a startup file, adds a setting or copies a script that the user would then have to maintain.

Completing a word is local, quiet and bounded in time, and works offline.
It never prompts, runs no hook or configuration command, evaluates no environment, installs or updates nothing, and writes no persistent state, cache, log or skill link.
Commands and options are offered ahead of any setup and without mise.
A dynamic input that is unavailable, such as a file that cannot be read or a mise that does not answer in time, is skipped; completion never guesses which configuration files are active.

Completion takes candidates from these sources only, and does not promise a candidate for every argument:

- Commands: the public built-in commands, the aliases `update` and `uninstall`, and every configuration command that [Configuration commands](#configuration-commands) would run, only where that command could start; the arguments of a configuration command are not completed.
- Global flags: `--plain` or `--verbose`, before the command and after `up` or `update`; `-h`, `--help` and `--version` only as the first word.
- A command's options, short forms such as `-n` included, only where the command accepts them: never an option that conflicts with one already given, a second copy of an option allowed only once, or any option after `--` where the command accepts `--`.
  A repeatable option, such as `dots bundle create --recipient` or `dots bootstrap --env`, `--only` and `--skip`, stays on offer as far as the grammar allows.
- An option's value, inline as `--option=VALUE` or as a comma-separated list, only where the option accepts that form.
- Profile names, for `--profile` and for `dots bootstrap --env` or `-E`, taken from `dots_profiles` in `ROOT/config.toml`.
- `bash`, `fish` and `zsh` for `dots completions`; `create` and `check` for `dots bundle`; the commands that save reports for `dots reports`.
- `python`, `node` and `shell` for `dots test`, as hints for a conventional `tests/run`; any other argument is still passed through.
- For `dots add`, prefixes only, with no registry search: the native managers `apk:`, `apt:`, `brew:`, `brew-cask:`, `dnf:`, `mas:` and `pacman:`, the mise backends `aqua:`, `asdf:`, `cargo:`, `conda:`, `dotnet:`, `gem:`, `github:`, `go:`, `npm:`, `pipx:`, `spm:`, `ubi:` and `vfox:`, and `https://github.com/`.
- For `dots remove`, the targets described under [`dots remove`](#dots-remove-target).
- For `dots bootstrap`, its own options and the native bootstrap options it passes on: `--from`, `--adopt`, `--from-dir`, `--config-dir`, `--replace-history`, `--take-remote-all`, `--dry-run` (`-n`), `--yes` (`-y`), `--skip-dirty`, `--force-dotfiles`, `--prune-dotfiles`, `--only`, `--skip`, `--prompt-secrets`, `--prompt-vars`, `--save-env`, `--env` (`-E`), `--update` and `--upgrade`.
  `--only` and `--skip` exclude each other, as do `--only` and `--restore`.
  `--only` and `--skip` take part names: `plugins`, `packages`, `accounts`, `files`, `services`, `firewall`, `compose`, `repos`, `dotfiles`, `mise-shell-activate`, `macos-defaults`, `settings`, `macos-launchd-agents`, `linux-systemd-units`, `user`, `tools`, `task` and `final-hook`; `--restore` takes a machine ID.
- Local files for `--path`, `--bundle`, `--identity`, `--identity-file`, `--input` and `--output`; directories for `dots init` and `--from-dir`; nothing for free text, such as `--asset`, `--restore`, a port or a repository, nor for `--config-dir`, which is relative to the checkout rather than the caller's directory.

Each script keeps the usual behaviour of its shell: quoting and escapes, completion at a cursor that has text after it, a prefix such as `brew:` left open for further typing, and the shell's own path completion, names with spaces or tabs, a literal `=` and names that look like options included.
An unquoted `~/` at the start of an earlier word stands for the home directory, as it does when the shell runs the command; a quoted or escaped `~/` is literal.

## The `dots/` directory

Each part is optional.

| Path | Purpose |
| --- | --- |
| `hooks/update.d/` | Executables that `dots up` runs once packages, tools and installer exceptions are done |
| `hooks/bootstrap.d/` | Executables that `dots bootstrap` runs once installer exceptions are done |
| `commands/NAME` | An executable that runs as `dots NAME [ARGS...]` |
| `exceptions.tsv`, `exceptions/NAME` | Apps that mise cannot install, each with a recipe of its own |
| `retired-dotfiles` | Targets of removed dotfiles, whose dangling links may be deleted |
| `retired-declarations` | Outdated declarations that sync removes from the machines' local files |

### Environment of children

**New in 0.5:** in the environment it gives every child it starts directly, `dots` puts its own running executable first on `PATH`, even when the user started `dots` by an absolute path.
Every hook, recipe, configuration command and `dots test` child, and every other launch `dots` itself makes for `dots bootstrap`, starts with that executable first on `PATH`, after any configuration environment has been applied, so that `dots` there is the same executable at the same version.
No setting is needed for this.
In 0.4.0, a configuration command and the shell fallback of `dots test` could each run another `dots` from the caller's `PATH`.
For programs that an explicit `dots mise exec` or `dots mise run` starts, see [`dots mise`](#dots-mise-args).
As in 0.4.0, the callbacks of mise's native bootstrap use the `dots` that the configuration pins, as [`dots bootstrap`](#dots-bootstrap) describes.
Every hook, recipe, configuration command, `dots test` child and `dots mise` child receives `DOTS_ROOT` and `DOTS_PROFILE`.
**Changed from 0.3.5:** `DOTS_HOME` is no longer set.

### Hooks

A hook is a regular executable file with a name matching `^[A-Za-z0-9][A-Za-z0-9._-]*(@[A-Za-z0-9._-]+)?$`; files with other names are ignored.
Hooks run in the byte order of their names, from the configuration root and in the configuration's mise environment, each as a reported stage of its own.
A hook that fails does not stop the ones after it.
The stage label is the name stripped of its `@PROFILE` suffix, of a leading number followed by `-` or `_`, and of a final extension, with dashes and underscores shown as spaces: `20-Login-shell.sh` is reported as `Login shell`.
A name that ends in `@PROFILE` applies to that profile only; other profiles leave it out of their listing.
A suffix naming a profile outside `dots_profiles` fails the listing before any hook in that directory runs.
A hook may prompt, as [Output and reports](#output-and-reports) describes.

Update hooks receive these as well:

- `DOTS_TOOLS_OK`: `1` if installing newly declared tools succeeded in this run, and `0` otherwise.
- `DOTS_TOOLS_UPGRADED`: `1` if that succeeded and upgrading the declared tools succeeded as well, and `0` otherwise.
- `DOTS_CHANGES`: a file; every `ITEM<TAB>BEFORE<TAB>AFTER` line appended to it shows up as a change in the hook's report, with `not installed` as `BEFORE` for a new installation.

Bootstrap hooks receive `DOTS_PRIVATE_READY`, `DOTS_RESTORED` and `DOTS_BUNDLE_FD` as well; see [Bootstrap bundle](#bootstrap-bundle).

### Configuration commands

`dots NAME ARGS...` runs `commands/NAME` if `NAME` is not a built-in, matches `^[A-Za-z0-9][A-Za-z0-9._-]*$`, and is a regular executable file.
It receives the arguments unchanged and runs in the caller's directory.
`DOTS_PROFILE` keeps any value the caller set; otherwise it holds the persisted profile, or is empty.
It runs before mise loads the configuration, so there is no sync, tool install or package update.

### Installer exceptions

`exceptions.tsv` holds one tab-separated row per app: the profile (`base` for every profile), the recipe name, and optionally a comma-separated list of flags.
Empty lines and lines that begin with `#` are ignored.
A row with a fourth column, an unknown profile, an invalid name or an unknown flag makes standalone `dots exceptions` exit 2.
In `dots bootstrap complete`, a malformed row fails the installer exceptions stage, and later stages can still run.
A bootstrap that completes with a failed stage exits 1, subject to the existing cancellation rules.
The recipe is the executable `exceptions/NAME`, where `NAME` matches `^[A-Za-z0-9][A-Za-z0-9._-]*$`; a selected row without an executable recipe is an error.
`install-only` makes `dots up` pass `--install-only`, so the recipe only installs an app that is missing.
`hold=MANAGER:NAME` records that the recipe pins that package, so that `dots add` refuses it; the value matches `^[a-z][a-z0-9-]*:[A-Za-z0-9@._/+-]+$`, and a request matches once any tap prefix or `@version` is dropped.
Under bootstrap, `--install-only` is always passed.
Under `dots up`, a recipe also receives `DOTS_CHANGES`.
A recipe updates its own app and nothing else, with that app's own package-manager command, never with a whole-environment update.
A recipe may prompt, as [Output and reports](#output-and-reports) describes.

### Retired dotfiles and retired declarations

`retired-dotfiles` lists one `~/PATH` per line, ignoring empty lines and `#` lines.
`dots retire-dotfiles` deletes a listed target only while it is a symlink into the configuration root, by its logical or physical path, whose destination is gone.
It warns about and ignores an entry that does not begin with `~/` or that contains `..`, and reports every link it removes.
It is meant to be called from a mise `post-dotfiles` hook.

Each `retired-declarations` row is `dotfiles.NAME<TAB>SOURCE`, where `SOURCE` is a plain relative path with no backslashes and no `.`, `..` or empty parts.
Invalid rows are ignored one by one.
Before it validates a merged configuration, sync validates it with the matching tables removed from each machine-local file.
A table is removed only when it is exactly `[dotfiles.NAME]`, its decoded `source` string equals `SOURCE` without path normalization, nothing else defines it, and `SOURCE` is absent from the merged configuration.
**New in 0.5:** the source string may use any valid TOML quoting, including the triple quotes that 0.4.0 left in place.
The machine-local file changes only once the merged files are on disk, and only while it is still the file sync validated; the rest of the file, its mode and the deployed file are kept, and sync keeps a backup and prints its location.
A rendered template is a regular file, so it is never deleted.

## Helper subcommands

**New in 0.5.**
They replace the shell libraries that configurations sourced from the 0.3.5 manager directory, so hooks and recipes can be written in any language and tested against these commands.

### `dots root`

It prints the configuration root as a physical path.

### `dots profile`

With no options, it prints the selected profile; if none is selected, it exits 2 with one line explaining how to select one.
`dots profile --profile P --machine-id ID [--origin URL]` validates `P` and `ID`, persists them as described in [Profiles and machine identity](#profiles-and-machine-identity), and confirms the selection.

### `dots mise ARGS...`

It runs mise rooted in the configuration, with this machine's `PROFILE,host-ID` environment, without the caller's `MISE_ENV`, and with auto-install off.
`ARGS` are handed over as an argument array and never evaluated by a shell.
**New in 0.5:** `dots mise` is a public command; 0.4.0 had no such built-in.
For an explicit `exec` or `run`, mise receives those arguments and the active configuration unchanged, and applies its own `PATH` rules within the command or task that results.
Configured tool directories and `[env] _.path` can come before the inherited `PATH`, so a bare `dots` there resolves as the configuration determines.
On macOS, for mise's package bootstrap and status, it exposes the Ruby the configuration has already installed, so that Homebrew's tap evaluation avoids a shim; it never installs Ruby.
Its exit status is mise's.

### `dots exceptions`

It runs each selected installer exception with `--install-only`, naming every one before it runs and every failure afterwards.
It exits 1 if any recipe failed.

### `dots enroll-history`

```text
dots enroll-history --profile P --machine-id ID --identity PATH [--origin URL] [--recipient age1...]...
```

See [Enrollment](#enrollment).

### `dots install-release-app OWNER/REPO`

It is the post-install hook that mise runs for a GitHub release that `dots add` recorded as a macOS app.
It requires exactly one app bundle in the installed release, and replaces the app in `/Applications` only if `dots` previously installed that app from this repository.
As in 0.4.0, a valid `{repo, path, bundle_id}` ownership record together with a matching app suffices to establish ownership for an upgrade, or for a removal to the Trash, subject to the existing permission, path, marker and bundle checks.
Mismatches and ambiguous rename candidates are refused.
With a matching legacy record, the user does not have to move the app aside first.
**New in 0.5:** `dots` records stronger app identity only after it has installed or replaced the app itself.
It fails on any system other than macOS.
The hook that `dots add` writes runs `dots` by a path that survives upgrades: for a Homebrew install, the prefix's `bin/dots`; for a mise install, `dots` through `mise x github:EzraCerpac/dots`, locating mise through `DOTS_MISE_BIN`, `PATH` or `~/.local/bin/mise`; for any other install, the executable's absolute path.

### `dots brew`

Every subcommand acts on a single named package and never runs a global Homebrew update, autoremove or install cleanup.

- `path` prints the Homebrew executable: the first of `brew` on `PATH`, `/opt/homebrew/bin/brew` and `/usr/local/bin/brew` whose prefix is `/opt/homebrew` or `/usr/local`; if there is none, it exits 1.
- `version formula|cask TOKEN` prints the installed version, and exits 1 if that is unknown.
- `admin-available` exits 0 if the user is root, if stdin and stdout are terminals, or if cached `sudo` access exists, and 1 otherwise.
- `defer-for-admin TOKEN` prints that `TOKEN` needs interactive administrator access, gives `sudo -v && dots up` as the next step, and exits 3.
- `record formula|cask TOKEN installed|updated [BEFORE]` appends `brew-KIND:TOKEN<TAB>BEFORE<TAB>AFTER` to `DOTS_CHANGES` when that is set and the package is installed; `installed` writes `not installed` as `BEFORE`, and `updated` writes only when `BEFORE` and the installed version are both known and different.
- `update-cask [--auto-updates] [--admin] TOKEN [PROCESS] [--install-only]` installs a missing cask, or, unless `--install-only` is given, upgrades an outdated one.
  `--auto-updates` upgrades self-updating casks as well.
  With `PROCESS`, it defers while a process of exactly that name is running.
  With `--admin`, it defers whenever `admin-available` would fail.
  A deferral prints its reason and exits 3.
  It records every verified installation or upgrade the way `record` does.
- `update-formula TOKEN [--install-only]` does likewise for a single formula, without the process and administrator checks.
- `hold-formula TOKEN VERSION [REASON]` succeeds if exactly that version is installed, and otherwise prints a refusal on stderr and exits 1.
- `hold-cask TOKEN VERSION APP_PATH [REASON]` does likewise, taking the version from the app bundle at `APP_PATH`.
- The hold commands never install, upgrade or remove anything.
- Anywhere but macOS, `update-cask`, `update-formula`, `hold-formula` and `hold-cask` print that the package is macOS-only and exit 4.

**Changed from 0.3.5:** 0.3.5 returned 0 for a deferral, sending a notice to the dashboard, and returned 0 with a "Deferred" line on systems other than macOS (`lib/brew-exception.sh:65-75,105-108,218-221`).

### Replacing 0.3.5 interfaces

| 0.3.5 | 0.5 |
| --- | --- |
| `source "$DOTS_HOME/lib/brew-exception.sh"` and its functions | A `dots brew` subcommand for each function |
| `dots_up_emit_notice` from a recipe | The recipe prints a message and exits with status 3 |
| `source "$DOTS_HOME/lib/setup-common.sh"`, `setup_init`, `setup_mise` | `dots root`, `dots profile`, `dots mise` |
| `setup_error` | Print a message on stderr |
| `quiet_probe` | Capture the child's output in the script's own language, and print it if the child fails |
| `bash "$DOTS_HOME/lib/retire-dotfiles.sh" "$DOTS_ROOT"` | `dots retire-dotfiles` |
| `dots internal home` | `dots root` for the configuration; no replacement for the manager |
| `dots internal persist-selected`, `complete` | `dots bootstrap persist-selected`, `dots bootstrap complete` |
| `dots internal profile`, `enroll-history`, `exceptions` | `dots profile`, `dots enroll-history`, `dots exceptions` |
| `dots internal update`, `status`, `backup`, `restore` | `dots up`, `dots status`, `dots backup`, `dots restore` |
| `dots internal github-install-app` | `dots install-release-app` |
| `dots internal commands`, `profiles`, `sync-present` | Removed without replacement |

`dots internal` is an unknown command and exits 2.

## Bootstrap bundle

A bundle is UTF-8 JSON encrypted with age:

```json
{ "version": 1, "secrets": { "KEY": "VALUE" }, "recipients": ["age1..."] }
```

- `version` is the number 1.
- `secrets` is an object with string keys and nonempty string values that contain no NUL.
- `recipients` is optional; every entry matches `^age1[0-9a-z]+$`, and duplicates are dropped.
- Any other field at the top level is rejected.

**Changed from 0.3.5:** every secret key is accepted, where 0.3.5 accepted only a fixed list, so every bundle that 0.3.5 wrote still validates.

Selection:

- The bundle is `--bundle FILE` if given, and otherwise `encrypted/bootstrap.PROFILE.json.age` if that exists.
- A bundle counts as requested when that profile file exists, or when a bundle or bundle identity is given in this run or remembered from an earlier one.
- The bundle identity is `--identity KEY`, or else the remembered bundle identity, or else, for a requested bundle, the recovery identity.
- The recovery identity is `--identity KEY`, or else `SETUP_AGE_IDENTITY`, or else the one recorded in `config.local.toml`, or else `~/.config/age/keys.txt`.
- The remembered bundle identity is used for bundle decryption and nothing else.
- When a requested bundle is missing its file or its identity, the private inputs stage is deferred, with `dots bootstrap --bundle FILE --identity FILE` as the next step.

**New in 0.5:** bundle values reach bootstrap hooks through `DOTS_BUNDLE_FD`.

- Every bootstrap hook that `dots` launches receives `DOTS_BUNDLE_FD`, the number of an inherited read-only file descriptor.
- Each hook has a descriptor of its own, readable from the start, that holds the `secrets` object as UTF-8 JSON.
- The object is `{}` when no bundle was requested, and also `{}` when private recovery is not ready.
- Private recovery is ready once every requested bundle has decrypted and validated, and every requested restore has succeeded.
- `DOTS_PRIVATE_READY` is `1` when private recovery is ready and `0` otherwise, so a hook can tell "nothing requested" (`{}` and `1`) from "requested and failed" (`{}` and `0`).
- `DOTS_RESTORED` is `1` when this run restored settings history, and `0` otherwise.
- Values arrive intact, newlines included.
- `dots` never puts a value in a plaintext file, an environment variable or an argument, and never prints one.
- No other stage or command that `dots` launches receives the descriptor; what the hook's own children inherit is up to the hook.
- The descriptor leaves the hook's prompt channel free, so the hook can still prompt as [Output and reports](#output-and-reports) describes.
- The hook is responsible for what it prints, where it writes values, keeping files that already exist, and closing the descriptor before it starts long-lived children.

For example, a hook that only checks that a value is present and prints nothing secret:

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

`dots` has no built-in behaviour for any app: writing app files, credential helpers and service enrollment from bundle values are the business of the configuration's hooks, and no value ever becomes a credential implicitly.

## Settings history

`dots backup` and `dots restore` keep files that apps write in mise's encrypted dotfile history, in the private `dots_state_repo`, with one `host-ID` branch per machine.
Every such file is a native track entry with a `host-ID` profile variant for each machine that keeps it:

```toml
# config.laptop.toml
[dotfiles]
"~/.config/app/state.json" = { mode = "track", encrypt = true, variants = [{ profile = "host-my-laptop" }] }
```

The variant names the machine's stream in history, so no other machine ever applies it.
A variant with history must never be renamed or removed.
As in 0.4.0, every spelling of the target in the file that effectively declares it must say `encrypt = true`.
A spelling in a lower-priority file that is fully shadowed does not block a capture proven to be encrypted.
**New in 0.5:** before any checkpoint that `dots` starts, `dots` checks every file that the native save would capture, including automatic captures, initial manual-baseline captures and machine-local captures.
Handing mise a named path does not settle which files it will capture.
If any file would be saved without encryption, `dots` refuses before the save and leaves the edits where they are.
Variants that are active only on other machines fall outside this check.
On a target that [`dots remote`](#dots-remote) bootstrapped, native mise stores a tracked file locally in plaintext when history is enabled on the target and the file is tracked without encryption.
Tracked files end up with mode 0600.
Backup, restore and enrollment require `dots_state_repo`, which follows [Remotes](#remotes).

During history operations, `dots` keeps verbatim transport errors out of its output and its saved reports, and does not pass on Git trace or mise log-file overrides.
Native mise owns its health record, `history/sync.json`, whose `last_error` can hold a verbatim transport error.
`dots` does not touch this record.
Dots checks its own preconditions before it launches mise and classifies the native result, adding a host branch check after a successful backup sync.
Mise owns the recovery of stale operations during native fetch and sync, including a record left behind when another native mise process crashes between the precheck and the capture.

### Enrollment

`dots enroll-history` and stage 6 of `dots bootstrap complete` prepare a machine for encrypted history:

- Once its prerequisites hold, stage 6 of `dots bootstrap complete`, and nothing else, creates a missing machine identity at `~/.config/age/keys.txt`, by exclusive creation with mode 0600, and with mode 0700 for a parent directory it creates.
- Standalone enrollment uses an existing identity to prepare the machine-local configuration, the public recipients and the permissions of tracked files.
- If the identity is missing, standalone `dots enroll-history` or `dots restore` exits 2.
- **New in 0.5:** the diagnostic names the missing identity path and tells the user how to supply or generate it.
- Neither another machine's private key nor the recovery identity's private key is ever copied.
- Bootstrap enrollment adds this machine's public key, the recovery identity's public key and the bundle's recipients to `config.local.toml` and nowhere else.
- **Changed from 0.4.0:** 0.4.0 used the bundle identity to derive the recovery recipient when the bundle file existed, while 0.5 uses the recovery identity's public key.
- A refresh of recipients keeps the valid extra public recipients already recorded in `config.local.toml`.
- Bootstrap enrolls only when private recovery is ready and the recovery identity file exists, and defers the stage otherwise.
- **New in 0.5:** once private recovery is ready and the recovery identity exists, a missing `dots_state_repo` fails the stage and bootstrap exits 1.
- **New in 0.5:** when a recorded history origin or recovery identity conflicts, bootstrap fails enrollment and leaves the recorded value as it is.
- Enrollment needs at least one recipient besides this machine's own public key: a recovery identity with a different key, a bundle recipient, or a `--recipient`.
  The default recovery identity is the machine identity's own path, so on its own it never qualifies.
  Without such a recipient, enrollment exits 3 and the bootstrap stage is deferred, pointing to `--identity` or `SETUP_AGE_IDENTITY` as the way to supply a separate recovery identity; the machine's own key is never presented as a recovery backup.
- The identity is a readable regular file outside the configuration root, named by an absolute path or by one beginning with `~/`.
- Each tracked file that this machine keeps gets mode 0600; a tracked path that is a symlink, or that lies outside `HOME`, is refused.
- If no tracked file names this machine's `host-ID` profile yet, enrollment exits 3 and the bootstrap stage is deferred.
- Bootstrap next records the history origin and sets history sync to `manual`.
- Only bootstrap enrollment activates mise's history watcher service and checks that it runs.
- If the watcher is not running after that, the stage stays deferred, with `dots backup` as the way to save.
- Enrollment saves no checkpoint, creates no repository and uploads nothing; publication happens through `dots backup` alone.

### `dots backup`

It accepts no arguments.
It stops with exit status 2 at the first check that fails:

1. History sync is `manual`.
2. A machine ID is set, and the branch is `host-ID`.
3. `dots_state_repo` holds a valid value.
4. Every tracked entry active on this machine is encrypted, and is active only through this machine's `host-ID` variant.
   Entries whose variants name only other machines are not active here, are never saved here and are not checked.
5. The branch of mise's effective history origin is `host-ID`.
6. Every effective push URL of the history store, after Git's URL rewrites, matches `dots_state_repo` and is not the source repository.

It then saves a checkpoint locally.
**New in 0.5:** before publishing, `dots backup` audits the actual tracked payloads in at least every checkpoint reachable from the local history branch that it cannot prove is already on the validated destination host branch.
With a provisioned identity, stock OpenSSH works.
With a custom opaque SSH launcher, one configured in Git included, the guard audits the whole local ancestry without querying the branch, and may refuse a checkpoint that is already on the remote.
Native sync, and the readback after a success, can still use the configured launcher.
Over HTTP(S), remote ancestry counts only if the branch query is bound to the validated destination.
The destination checks still hold, and a malformed remote-helper URL is refused.
A current unencrypted declaration still fails check 4 with exit 2, before the local save and before any contact with the remote.
With mise's default `history.allow_plaintext_history = false`, a plaintext or unreadable payload, or a malformed encrypted wrapper or envelope, stops backup with exit 1 before native sync, and no history is sent.
The historical audit checks the format only, and needs no retired private key.
The diagnostic on stderr names the path that escaped and its checkpoint, and gives the recovery action:

> Keep the current store and inspect the checkpoint in a private copy.
> To publish fresh history, copy your configuration with encrypted tracking, set its history origin and dots_state_repo to an empty private repository, and run dots backup with a separate MISE_STATE_DIR.

Backup reads the effective `[settings.history]` setting with `mise settings get history.allow_plaintext_history`, which includes mise's `MISE_HISTORY_ALLOW_PLAINTEXT_HISTORY` override.
Only a successful read of `true` skips the audit; native sync still applies its own checks.
Should the setting not read as a boolean, backup stops before sync.
Publication to the state repository happens only through native `mise bootstrap dotfiles sync`.
If native sync exits nonzero after the local save, backup exits 1, reports that the checkpoint was saved locally and that publication is unconfirmed, keeps native stderr private and skips the readback.
Version 0.4.0 printed `Settings backup published.` whenever native sync exited 0, even with conflicts still waiting to be reconciled.
**New in 0.5:** after a successful native sync, backup reads the validated remote host branch back once, and reports publication only when the saved checkpoint is that branch's tip or one of its ancestors.
If reconciliation is pending and the checkpoint is absent, backup exits 1, says that the checkpoint was kept locally and points the user to `mise dot status`.
If the readback cannot verify publication, backup exits 1, says that the checkpoint was saved locally and points the user to the private history remote to check it.
The outcome of publication is then left unconfirmed.
Backup runs native sync once, with no publication retry and no force push.

### `dots restore [--initialize-history]`

- It needs a history origin in the machine-local configuration that matches `dots_state_repo`, and checks the effective fetch URL as [Remotes](#remotes) describes before it contacts that origin.
- It leaves the source checkout, and any inactive or unencrypted target, untouched.
- Its first preflight checks the user's mise history watcher process and its launchd or systemd service, and refuses to go on while the watcher runs or when its state cannot be confirmed.
- **Changed from 0.4.0:** when the watcher check and a pending-marker or local-store check would both fail, 0.5 reports the watcher failure first.
  Version 0.4.0 checked the marker and the store first.
- Restore never activates, stops or restarts the watcher.
- It requires a recovery identity that already exists, and never generates one.
- Restore uses only fetch-side remote traffic. During a restore it never runs `dots backup`, never pushes and never publishes.
- **New in 0.5:** before any native restore operation that can capture a checkpoint, fetch included, `dots` requires both a nonempty recipient list from history configuration that native mise trusts at system or global scope, and `history.allow_plaintext_history = false`.
  The recipient check is satisfied even by a recipient that only a trusted system or global `conf.d` fragment supplies.
  Recipients from a project alone do not count toward this check.
  Restore looks for recipient settings in mise’s default trusted directories for the active environment that dots sees, so an environment that only an early `miserc` selects can make this check disagree with the settings mise uses to encrypt history.
  If either condition fails, restore exits 2 before any capture or target installation.
  The diagnostic names the condition that failed and tells the user to configure history recipients or to turn off plaintext history, as the case may be, without showing key material.
- As in 0.4.0, native mise applies the fetched history to the actual targets, and may save local undo checkpoints of their live contents before it overwrites them.
  Each new undo checkpoint must be encrypted with age and decryptable by the identity used for the restore.
  Native mise picks the recipients, and may use either the seed's recipients or the current local list.
  Differences between historical recipients never block a restore by themselves.
  A restore that succeeded in 0.4.0 with an explicit replacement identity, after the old private key was moved, still succeeds when the 0.5 preflight passes.
  `dots` leaves the choice of recipients to mise, and neither writes private mise state to force a choice nor runs a second decrypt or apply pass.
  A later explicit `dots backup` publishes the undo checkpoints that are in the history branch's ancestry.
- When changes overlap, restore keeps the live file and exits 1, with a diagnostic saying that both sides changed the same lines.
- Before a new store is installed, it is staged and validated as a bare clone.
  On a first restore, mise applies the fetched remote state as the initial checkpoint, under its native conflict checks.
- An interrupted first restore resumes on the next run rather than treating the new store as established.
- Restore observes the live files before it fetches, and detects changes made during the fetch before anything is applied.
  A retry of an attempt that never started applying observes the live files afresh, so edits made between attempts count as the user's and are kept.
  Once an apply has started, its observation is kept through fallback and retry.
  If it cannot prove that recovery is safe, it keeps later edits as well as both the dots and the native recovery journals.
- A completed restore leaves behind no staging clone and no pending marker.
- An existing store that is not connected is replaced only with `--initialize-history`.
  The old store is moved to a private sibling directory and kept.
  If preparing the replacement fails, the old store stays where it is.
- It refreshes this machine's identity and recipients, sets this machine's active encrypted tracked files to mode 0600, and sets history sync to `manual`.
  As in 0.4.0, the refresh keeps the local recipients and appends the selected identity's public keys in `config.local.toml`, without promoting recipients that only a project supplies.
- It needs Git, Git authentication for the state repository, and the age identity to be present, and it installs nothing.

## Environment variables

`dots` reads these:

| Variable | Meaning |
| --- | --- |
| `DOTS_ROOT` | Chooses the configuration |
| `MISE_CONFIG_DIR`, `XDG_CONFIG_HOME` | Consulted in that order for the configuration root when `DOTS_ROOT` is unset |
| `DOTS_MISE_BIN` | The mise executable to use; without it, `~/.local/bin/mise`, then `mise` on `PATH` |
| `DOTS_SKILL_LINKS` | `0` disables agent skill linking |
| `DOTS_PROFILE` | Left as it is for configuration commands when already set |
| `SETUP_PROFILE`, `SETUP_MACHINE_ID` | Take precedence over the persisted profile and machine ID |
| `SETUP_HISTORY_ORIGIN`, `SETUP_AGE_IDENTITY` | The history origin and recovery identity that bootstrap uses |
| `MISE_ENV` | Read by `dots bootstrap persist-selected` and by nothing else |
| `XDG_STATE_HOME` | The base directory for `dots` reports and logs |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | Where the agents keep their configuration |

A `DOTS_MISE_BIN` that is set but not executable is a configuration error, with exit status 2.
**Changed from 0.3.5:** in that case the 0.3.5 dispatcher often fell back without a word (`bin/dots:231-232`), although its sync code already refused it (`lib/source-repo.mjs:151-155`).
`dots` sets `DOTS_ROOT`, `DOTS_PROFILE` and the hook variables above for its children; mise children receive `MISE_CONFIG_DIR` as well, and `dots up` sets `HOMEBREW_NO_INSTALL_CLEANUP=1`.
A variable that is a flag takes exactly `0` or `1`.

## Agent skill

The executable carries the agent skill inside it.
On every invocation except `dots completions` and shell completion, `dots` keeps a copy of the skill in storage of its own, and links that copy for the agents in use:

| Link | Made when |
| --- | --- |
| `${CLAUDE_CONFIG_DIR:-~/.claude}/skills/dots` | The directory of that agent exists |
| `~/.agents/skills/dots` | Either `~/.agents` or `${CODEX_HOME:-~/.codex}` exists |

`dots` creates or replaces a link only if it made that link, the link still points where `dots` left it, and the link's directory stays inside the agent's directory.
A directory, a file or any other link at those paths is the user's; `dots` leaves it alone and says so once.
`dots` never creates the configuration root of either agent, `${CLAUDE_CONFIG_DIR:-~/.claude}` or `${CODEX_HOME:-~/.codex}`.
When the Codex configuration root exists, `dots` may create `~/.agents/skills` even though `~/.agents` is missing, as long as the link's parent resolves inside `~/.agents`.
`dots status` shows every link.
Setting `DOTS_SKILL_LINKS=0` turns linking off.

## mise behaviour this contract relies on

The supported minimum remains mise 2026.11.8; the probes below ran on 2026.11.9, and 2026.11.8 was not exercised on its own.
Probes run on mise 2026.11.9 with an isolated configuration:
`mise config get`; `mise -E default,host-example exec`; `mise x -- COMMAND` without a tool; `mise bootstrap dotfiles apply` creating a link; `mise bootstrap plan`; `mise bootstrap dotfiles status --json`; `mise bootstrap --skip-dirty --yes` with its hook, dotfile, tools and task order as described under [`dots bootstrap`](#dots-bootstrap), and its exit 1 on a conflicting target; `mise x github:EzraCerpac/dots -- COMMAND` from a `pre-dotfiles` hook fetching the pinned version before it was installed, on macOS; the `--adopt URL` syntax in `mise bootstrap remote --help`.

The restore and backup probe for v0.4.0 used mise 2026.11.9, Git, age, a private bare remote and a stubbed watcher check.
The recovery identity and a configured recipient could each decrypt the undo checkpoint, and an unrelated identity could not.
The later explicit backup pushed the undo checkpoint that was in the history branch's ancestry.
On an overlapping conflict, the probe kept the live edit, exited 1 and made only fetch-side remote requests.
These observations record the outcomes of the 0.4.0 fixture.
The stronger recipient and plaintext-history preflight is a separate requirement of 0.5.

The contract also relies on how 0.3.5 used other paths of history save, sync and rollback, tracked-path listing, package bootstrap, services, `mise upgrade --no-prune`, `mise unuse`, tool pruning, `mise self-update` and real remote enrollment.

## Internal and subject to change

An implementation may choose these and change them at will, and nothing outside `dots` may rely on them:

- The format, schema and location of sync receipts, reports and logs, and how long they are kept.
- Lock files, pending markers, the names of backup and recovery files, temporary files, and any other metadata under the configuration's Git directory or the state directory of `dots`.
- Where the extracted agent skill and the working copies of the embedded templates live, and how they are laid out.
- Where `dots` keeps its record of the skill links and of the GitHub apps it installed.
- How stages are supervised, by processes, threads and any internal protocol between them, within the foreground rules of [Output and reports](#output-and-reports).
- The mechanism by which children find the running `dots` on `PATH`.
- The variables that `dots bootstrap` uses to pass options, such as `--restore`, to its own callbacks, `persist-selected` and `complete`.
- The exact Git and mise commands that `dots` runs to produce the behaviour described here.
- The wording of messages, beyond what each line has to say.
- How the completion scripts obtain their candidates: the protocol and tokens between a script and `dots`, the wording of descriptions, the matching algorithm, the time and file-size limits, temporary state and the environment of any mise probe; the commands and behaviour that each shell sees, as [`dots completions`](#dots-completions-bashzshfish) describes them, are public.

## What 0.5 drops

| 0.3.5 | Reason |
| --- | --- |
| `DOTS_HOME` and `dots internal home` | A single executable has no manager directory |
| The `dots internal` namespace | Public commands take its place; see [Replacing 0.3.5 interfaces](#replacing-035-interfaces) |
| The sourced libraries in `lib/` | Subcommands take their place, so hooks need no Bash |
| The separate `dots-up` renderer, its build from a checkout, path search and fallback, its `--capabilities` handshake (`dots-shared-cli/3`), `DOTS_UP_BIN`, `--backend`, its event protocol and its event, acknowledgement and stream-nonce variables | `dots` is a single executable without a full-screen interface |
| From 0.4.0: the `dots up` dashboard and its size check, the `menu` built-in, and the report browser with its forms, diagnostics tree, log search and replay navigation | Ordinary scrollback is easier to read and to keep, takes less machinery, and nothing relies on the old interface |
| From 0.4.0: `--no-wait`, the `A` key for agent handoff on a finished issue and `DOTS_AGENT_COMMAND` | As above |
| From 0.4.0: `dots internal shell-complete`, `dots-up complete` and the completion scripts in the support tree | The completion protocol is internal, and `dots completions` prints the scripts |
| From 0.4.0: the quiet `uv run` wrapper of `dots remove` | The executable needs neither Python nor uv |
| The Bash updater fallback and its logs | Nothing is left to fall back from |
| Bash, Node.js, uv and jq as requirements | The executable needs none of them |
| `dots atuin-bundle`, `dots atuin-login`, `dots_atuin_username`, `encrypted/atuin.json.age`, the Atuin status section and bootstrap stage | Enrolling apps is the configuration's job, done with hooks and commands |
| The Tailscale bootstrap stage and its auth-key handling | As above |
| Built-in private files written from bundle values, and `dots bundle create --from-live` | As above; values reach hooks through `DOTS_BUNDLE_FD` |
| `GH_TOKEN` and a GitHub credential helper injected from a bundle value | Credentials come from the user's own Git setup, and a hook can configure its own children |
| The fixed list of bundle secret keys | No key carries a built-in meaning any longer |
| Sync receipts from every earlier release | Receipts are internal |
| `SETUP_CONFIG_ROOT`, `SETUP_MISE_BIN`, `MISE_BIN` as overrides, `BREW_EXCEPTION_PLISTBUDDY` | They served test fixtures or duplicated `DOTS_ROOT` and `DOTS_MISE_BIN` |
| `checkout` and `archive` in `dots --version` | No installed directory is left to inspect |
| Running `tests/run` with `bash` | `dots` no longer needs Bash |
| `MISE_ENV` from the caller in ordinary commands | A project's selector must never change the machine's profile |
| `dots plan` and `dots apply [TARGET...]` | `dots up` applies the declared dotfiles, also in local-only mode; `dots mise bootstrap plan` and `dots mise bootstrap dotfiles apply TARGET` run the native operations explicitly |
| `dots publish` with `--all` and `--bookmark`, the `dots_publish_on_behalf_of` setting, `DOTS_PUBLISH_MODEL`, and JJ as a requirement | Proposing a change for review is ordinary Git and `gh` work |
