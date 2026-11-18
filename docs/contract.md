# dots 1.0 contract

Status: 1.0 candidate, not frozen.
It becomes binding when it is marked frozen; until then it may change.

This document describes what `dots` 1.0 does, as seen from outside: commands, files, settings, environment, output and exit codes.
It is written so that someone who has never used `dots` can implement it or test against it.
How 1.0 is built is out of scope.

## How to read this document

A statement without a label holds for both 0.3.5 and 1.0.
**New in 1.0** marks behaviour 0.3.5 does not have.
**Changed in 1.0** marks behaviour that differs from 0.3.5, and says how.
[What 1.0 drops](#what-10-drops) lists everything 0.3.5 has and 1.0 does not, each with its reason.
The 0.3.5 documents remain the detailed reference where this one points to them: [the README](../README.md), [the `dots/` reference](../init/README.md), [the task notes](../tasks/README.md) and [the sync guide](sync.md).
File references such as `bin/dots:153` name lines of the 0.3.5 implementation, as evidence for a statement.
Examples use generic names: profiles `laptop` and `server`, a machine `my-laptop`, and repositories `OWNER/NAME`.

## Platforms and programs

`dots` runs on macOS and Linux.
Linux under WSL is supported; native Windows is not.
**Changed in 1.0:** `dots` is one self-contained executable.
It needs no Bash, Node.js, uv or jq, and no support directory next to it.

| Program | Needed by | Version |
| --- | --- | --- |
| mise | every command that reads the configuration through mise | 2026.11.8 or newer |
| Git | `init`, `sync`, `up` (its sync stage), `backup`, `restore`, `bootstrap`, `publish` | 2.43 or newer for sync |
| gitleaks | sharing in `sync` and `up`, `publish`, patch display in `sync --last` | any release that writes a JSON report |
| age, age-keygen | `bundle`, a bootstrap that uses a bundle, history enrollment, `backup`, `restore` | |
| `gh` | `publish`; the optional remote offer in `init` | |
| JJ | `publish` | |
| Homebrew | Homebrew packages and the `dots brew` helpers, on macOS | |
| Bash | only a configuration's own hooks, commands and recipes that are Bash scripts | |

Native package managers (Homebrew, apt, dnf, pacman, apk, mas) stay external programs that mise and `dots` call.
A command checks only the programs that the requested action uses.
A missing program fails that action with one line naming the program and the action, and exit status 1.
A missing mise fails with exit status 2, as in 0.3.5 (`bin/dots:233`).
A missing optional program never fails an inspection command or a stage that has nothing to do.
`dots sync` with gitleaks missing still exits 0 and reports the missing scan in its receipt.
**New in 1.0:** `dots status` lists each program above as found, with its version, or missing, with the commands that need it.
This list prints even when mise itself is missing.

## Configuration root

The configuration is a mise config directory.
`dots` selects it in this order:

1. `DOTS_ROOT`, when set.
2. `MISE_CONFIG_DIR`, when set.
3. `${XDG_CONFIG_HOME:-$HOME/.config}/mise`.

`dots` resolves the result to its physical path (symlinks resolved) and exports it to every child as `DOTS_ROOT`.
Every command except `init`, `help`, `--version`, `completions` and `sync --last` stops with exit status 2 and `dots: setup not found at PATH` when `PATH/config.toml` does not exist.
For each mise child, `dots` sets `MISE_CONFIG_DIR` to the root, runs mise with `-C ROOT`, and removes `MISE_ENV` from the environment, so a project's own environment selector never leaks in (`bin/dots:236`).
The selected profile and machine reach mise through the root's `miserc.toml` and through `-E PROFILE,host-ID` where `dots` passes them explicitly.

### Files in the root

| Path | Purpose | Shared |
| --- | --- | --- |
| `config.toml` | Base mise configuration, always loaded; its `[vars]` holds the `dots` settings | yes |
| `config.PROFILE.toml` | Additions for one profile, loaded with `mise -E PROFILE` | yes |
| `config.host-ID.toml` | Optional additions for one machine, loaded with its `host-ID` environment | no |
| `dots/` | Hooks, commands, installer exceptions, retired dotfiles and retired declarations; see [The `dots/` directory](#the-dots-directory) | yes |
| `dotfiles/` | Conventional home of dotfile sources; `dots add FILE` puts sources here | yes |
| `.gitleaks.toml`, `.gitleaksignore` | Secret-scan rules for `sync` and `publish` | yes |
| `.gitignore` | Must ignore the machine-local files below and mise lockfiles | yes |
| `encrypted/bootstrap.PROFILE.json.age` | That profile's encrypted bootstrap bundle | yes |
| `tests/run` | The configuration's own tests, for `dots test` | yes |
| `miserc.toml` | This machine's mise environment selector, `env = ["PROFILE", "host-ID"]`, mode 0600 | no |
| `config.local.toml` | This machine's profile, machine ID, history and identity settings, mode 0600 | no |
| `mise.lock`, `mise.*.lock`, `locks/`, `.setup-state/` | Files mise and older setups write per machine | no |

"Shared" means the file belongs in the configuration repository.
`dots sync` refuses to share a machine-local file, and refuses a merged configuration whose ignore rules would expose one.
The `.gitignore` that `dots init` writes lists exactly the machine-local paths in this table.

### Settings in `[vars]`

mise `[vars]` values are strings, so a list is one string of space-separated words.
`dots` reads each setting from `ROOT/config.toml` with `mise config get --file ROOT/config.toml vars.NAME`, without auto-install.

| Setting | Meaning |
| --- | --- |
| `dots_profiles` | Required. The profiles this configuration selects, for example `"laptop server"` |
| `dots_source_repo` | Optional. The configuration's own repository; see [Remotes](#remotes) |
| `dots_state_repo` | Optional. The private repository for settings history; see [Settings history](#settings-history) |
| `dots_self_updating_casks` | Optional. Casks that `dots up` only installs when missing |
| `dots_publish_on_behalf_of` | Optional. A person's name; `dots publish` then adds a note that a model wrote the pull request text |

**Changed in 1.0:** `dots_source_repo` is optional, and both repository settings accept a Git URL as well as GitHub `OWNER/NAME`.
**Changed in 1.0:** `dots_atuin_username` is no longer read.
A profile name matches `^[a-z0-9][a-z0-9-]*$` and is not `base`, `local` or any name starting with `host-`.
`dots` refuses any profile not listed in `dots_profiles`: in `dots profile`, `dots add --profile`, hook suffixes and installer exception rows alike.
An empty or invalid `dots_profiles` stops the command with exit status 2 and a line naming the setting and the file.

## Profiles and machine identity

A profile is a mise environment: `config.laptop.toml` applies when the machine selects `laptop`.
A machine ID names one machine and matches `^[a-z0-9][a-z0-9-]*$`.
The machine's own mise environment is `host-ID`, which loads `config.host-ID.toml` and selects the machine's stream in settings history.
A machine persists its selection in two ignored files, both mode 0600:

- `miserc.toml` gets `env = ["PROFILE", "host-ID"]`; other selectors already in that array are kept, after these two.
- `config.local.toml` gets `env.SETUP_PROFILE`, `env.SETUP_MACHINE_ID` and `settings.history.sync = "manual"`, and with `--origin URL` also `env.SETUP_HISTORY_ORIGIN`, `history.origin.url` and `history.origin.branch = "host-ID"`.

Both candidate files are checked with mise before either live file changes, and each is replaced atomically.
A `miserc.toml` that selects two profiles or two `host-` IDs is ambiguous; `dots` stops with exit status 2 and asks for it to be resolved by hand.
Commands that run inside the configuration take the profile from `SETUP_PROFILE` in the environment, else from a single profile named in `MISE_ENV`, else from the persisted selection (`lib/setup-common.sh:27-49`).
They take the machine ID from `SETUP_MACHINE_ID` and load `PROFILE,host-ID` when it is set.
They export `DOTS_PROFILE` (equal to the profile), `SETUP_PROFILE` and, when known, `SETUP_MACHINE_ID` to their children.

## First run

This section is the 1.0 first-run flow.
The table compares each behaviour with 0.3.5.

| Behaviour | 0.3.5 | 1.0 |
| --- | --- | --- |
| Configuration files | `dots init` writes `[vars] dots_profiles = "default"`, the `dots/` layout, `.gitleaks.toml` and `.gitignore`; it never overwrites a file (`tasks/init:57-107`) | Same files, from templates embedded in the executable; still never overwrites |
| Profile selection | A separate `dots internal profile --profile P --machine-id ID` (`README.md:99-103`) | `dots init` selects a profile and machine ID itself |
| Machine ID | Chosen by hand, or `auto-` plus a UUID during bootstrap | Derived from the short hostname by `init` |
| Git repository | Made by hand, with `main` and `dev` (`README.md:122-128`) | `dots init` creates it, with `main` and `dev` on one initial commit and `HEAD` on `dev` |
| Remote | A GitHub `origin` named in `dots_source_repo` is required before `dots up` | Optional; `init` offers to create one in a terminal with `gh`, otherwise prints two commands |
| Remote kinds | GitHub only (`lib/source-repo.mjs:30-48`) | GitHub `OWNER/NAME` or any Git URL |
| `dots up` without a remote | Sync stops: `missing setting: add dots_source_repo` | The sync stage is skipped with one line; `dots up` exits 0 |
| `dots sync` without a remote | Exits 0 with a stop line | Exits 0 with a one-line receipt saying how to add a remote; no network, no scan |
| Receipt on absent `origin` | `stopped · ... no origin remote` | Local-only mode when no source setting is configured either; a diagnostic otherwise |
| `dots add` | Software only | Software, or an existing file (`dots add FILE`) |
| `dots status` | No prerequisite list (`tasks/status:15-45`) | Lists each required program with version or missing |
| Internal commands in the quickstart | Required | None |

### `dots init [DIR]`

`DIR` defaults to the configuration root.
`init` creates `DIR` when it does not exist.
Each line of its output is `created`, `added` or `kept`, followed by the item.
A second `dots init` in the same directory changes nothing and prints only `kept` lines.

Files:

1. Without `config.toml`, `init` writes one with `[vars] dots_profiles = "default"` and commented hints for `dots_source_repo` and `dots_state_repo`.
2. With a `config.toml` that lacks `dots_profiles`, `init` adds `dots_profiles = "default"` under `[vars]` and keeps every other byte, comment and order.
3. With a `config.toml` that has `dots_profiles`, `init` leaves the file alone.
4. It creates `dots/`, `dots/hooks/update.d/`, `dots/hooks/bootstrap.d/`, `dots/commands/` and `dots/exceptions/`.
5. It writes `dots/README.md`, `dots/exceptions.tsv`, `dots/retired-dotfiles`, `dots/retired-declarations`, `.gitleaks.toml` and `.gitignore` from embedded templates, each only when absent.

`dots/README.md` is the 1.0 version of [the `dots/` reference](../init/README.md).
The `.gitleaks.toml` template extends gitleaks' default rules.

Profile:

1. A valid persisted selection is kept.
2. Otherwise, when `dots_profiles` contains `default`, `init` selects `default`.
3. Otherwise `init` selects the first profile listed in `dots_profiles`.

Machine ID:

1. A valid persisted machine ID is kept.
2. Otherwise `init` takes the hostname up to its first `.`, lowercases it, replaces each run of characters outside `a-z0-9` with one `-`, and trims `-` from both ends.
3. When the hostname is unavailable, the result is empty, or the persisted selection is ambiguous, `init` selects nothing.
   It prints `dots profile --profile PROFILE --machine-id ID` as the next step, keeps every file it made, and continues with the Git step.

`init` persists the selection exactly as `dots profile` does.

Git:

1. When `DIR` is not inside a Git repository, `init` runs `git init`, creates one commit containing only the files that `init` itself created in this run from embedded templates and that are still unchanged, points `main` and `dev` at that commit, and puts `HEAD` on `dev`.
   The commit message is `Initial dots configuration`.
2. A `config.toml` that existed before `init` is never part of that commit, even when `init` added a setting to it.
3. `init` never stages a file that existed before it ran, and never changes the index otherwise.
4. Without a Git identity, `init` passes `-c user.name=dots -c user.email=dots@localhost` to that one `git commit`; it never writes Git configuration.
5. When `DIR` is a repository that has no commits and an empty index, `init` treats it as new.
6. When `DIR` is any other repository, `init` keeps its branches, history, index and work tree.
   It reports each problem that `dots sync` would stop on (see [the checkout shape](sync.md#checkout-shape)), with the fix, and exits 0.
7. When `DIR` is a subdirectory of another repository, `init` stops with exit status 2.

Remote:

1. `init` offers to create a remote only when all of these hold: stdin and stdout are terminals, `gh` is on `PATH`, `gh auth status` succeeds, the repository has no `origin`, and `main` and `dev` are still the untouched initial commit that `init` made.
2. The question is whether to create a private GitHub repository, with a proposed name of `dotfiles`; the default answer is no.
3. On yes, `init` creates the private repository with `gh`, adds it as `origin`, and pushes `main` and `dev` from that initial commit, without force.
4. When creation or push fails, `init` keeps the local configuration and repository, prints the failure and the two manual commands below, and still exits 0.
5. A noninteractive `init` never creates a remote.
6. Otherwise, while the repository is still the untouched initial commit, `init` prints two commands for a new, empty remote repository:

   ```sh
   git -C DIR remote add origin URL
   git -C DIR push origin main:main main:dev
   ```

7. `init` says that these commands publish only the initial commit, and that later changes are shared with `dots sync`, which scans them first.
8. `init` prints neither command for a repository with other history, so an empty remote is never seeded with unscanned local files.

The local quickstart needs only mise and Git:

```sh
dots init
dots up
```

On a fresh configuration, `dots up` skips the checkpoint because nothing is tracked, skips sync with one line because there is no remote, finds no packages, exceptions or hooks, and exits 0 with nothing under "Needs attention".

### Remotes

`dots` works in one of three source modes, decided at each run:

| `dots_source_repo` | `origin` | Mode |
| --- | --- | --- |
| unset | absent | Local-only |
| unset | present | Shared through `origin` |
| set | present and matching | Shared through `origin` |
| set | absent | Diagnostic: the setting names a remote the checkout lacks |
| set | present, not matching | Diagnostic: `origin` is not the configured repository |

In local-only mode, `dots up` skips its sync stage with one line, and `dots sync` prints a one-line receipt that says how to add a remote and exits 0.
Local-only sync makes no network call, runs no scan and makes no commit.
An unreachable remote is never local-only mode: sync reports `OFFLINE` and keeps the work for the next run.
A configuration that names a remote is never treated as local-only, whatever is wrong with the remote.

A repository setting is either GitHub `OWNER/NAME` or a Git URL.
`OWNER/NAME` matches `^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$`.
It matches an `origin` spelled as any of `https://github.com/OWNER/NAME`, `https://github.com/OWNER/NAME.git`, `git@github.com:OWNER/NAME`, `git@github.com:OWNER/NAME.git`, `ssh://git@github.com/OWNER/NAME` or `ssh://git@github.com/OWNER/NAME.git`.
**Changed in 1.0:** both settings accept all six spellings; 0.3.5 accepted five for the source and six for the state repository.
Any other value is a Git URL: an SSH URL, an scp-like `user@host:path`, an HTTPS URL, or a `file://` URL of a bare repository.
A Git URL matches only an `origin` with exactly the same string.
`dots` passes a URL to Git as one argument and never through a shell.
Before any push, `dots` resolves the effective fetch URL and every effective push URL, after Git's `insteadOf` and `pushInsteadOf` rewrites, and refuses the push unless each one matches the setting (or, without a setting, `origin`).
Every URL that `dots` prints has its credentials removed.

`dots publish` works only with a GitHub repository; see [`dots publish`](#dots-publish).
The source repository and the state repository are independent.
A local-only configuration may still keep settings history in a state repository.

## Commands

### Global syntax

```text
dots [--plain | --verbose] [--no-wait] COMMAND [ARGS...]
```

`--plain` prints concise progress lines instead of the dashboard.
`--verbose` shows full child output.
`--plain` and `--verbose` together exit 2.
`--no-wait` closes the dashboard as soon as the command finishes; `up`, `sync` and `add` behave this way by default.
These flags apply only to built-in commands; with a configuration command they exit 2.
Without a terminal on stdout, output is plain.
`dots COMMAND -h` and `dots COMMAND --help` print that command's usage and exit 0.
An unknown command or option exits 2 with one line that points to `dots help`.

### Effects

Each command below is marked with what it can change:

- **R/O**: read-only; changes nothing listed below.
- **M**: the machine (installed software, files in `HOME`, services).
- **C**: the configuration checkout (its files, index or local branches) or its machine-local files.
- **R**: a remote repository (a push or a pull request).
- **H**: another host, over SSH.
- **D**: delegated; the child decides.

A letter in parentheses applies only under the condition given.
Two side effects apply to almost every invocation and are not repeated in the table:
every invocation may refresh the agent skill links (see [Agent skill](#agent-skill)),
and commands marked "report" save a private report under `${XDG_STATE_HOME:-~/.local/state}/dots/`.
Read-only commands never install tools: they run mise with `MISE_AUTO_INSTALL=0`.

### Inventory

| Command | Effects | Notes |
| --- | --- | --- |
| `dots` | R/O | In a terminal, the command menu; otherwise the help text, exit 0 |
| `dots help`, `dots -h`, `dots --help` | R/O | Built-in commands, then the configuration's commands |
| `dots --version` | R/O | `dots VERSION (INSTALLER)` |
| `dots reports [COMMAND]` | R/O | Browse saved reports; needs a terminal |
| `dots completions bash\|zsh\|fish` | R/O | **New in 1.0.** Prints a completion script |
| `dots status` | R/O, report | Profile, prerequisites, source, packages, tools, dotfiles, history, exceptions |
| `dots plan` | R/O, report | mise's bootstrap plan for the selected profile and machine |
| `dots sync --last` | R/O | Replays the latest sync receipt |
| `dots up [--mas]`, `dots update [--mas]` | M, C, (R with a remote), report | Update the machine; see [`dots up`](#dots-up) |
| `dots up --last` | R/O | Reopens the last `dots up` report |
| `dots sync` | C, M, (R with a remote), report | Share and apply configuration edits; see [Sync](#sync) |
| `dots apply [TARGET...]` | M, report | `mise bootstrap dotfiles apply TARGET...` |
| `dots test [ARGS...]` | D, report | Runs `ROOT/tests/run ARGS...` |
| `dots add [SELECTOR] [--asset GLOB] SOFTWARE...` | M, C, report | Install, then record |
| `dots add [SELECTOR] [--dry-run] FILE...` | M, C, report | **New in 1.0.** Adopt a dotfile; R/O with `--dry-run` |
| `dots remove`, `dots uninstall` `[SELECTOR] [--dry-run] [--keep-installed] TARGET...` | (M), C, report | R/O with `--dry-run`; no M with `--keep-installed` |
| `dots init [DIR]` | C, (R on an accepted offer) | See [`dots init`](#dots-init-dir) |
| `dots bootstrap [OPTIONS] [MISE-ARGS...]` | M, C, report | Full native bootstrap, then completion |
| `dots bootstrap persist-selected` | C | Callback for a `pre-dotfiles` hook |
| `dots bootstrap complete` | M, C | Callback for the final `bootstrap` task |
| `dots remote HOST --profile P [OPTIONS]` | H | R/O with `--dry-run` |
| `dots bundle create OPTIONS` | M (writes one file) | Encrypt a bootstrap bundle |
| `dots bundle check OPTIONS` | R/O | Decrypt and validate a bundle |
| `dots backup` | M, R, report | Save and publish settings history |
| `dots restore [--initialize-history]` | M, C, report | Restore settings history; fetches only |
| `dots publish [--all \| --bookmark NAME]` | C, R, report | Pull request from a JJ workspace |
| `dots NAME [ARGS...]` | D | A configuration command, `ROOT/dots/commands/NAME` |
| `dots root` | R/O | **New in 1.0.** Prints the physical configuration root |
| `dots profile` | R/O | **New in 1.0.** Prints the selected profile |
| `dots profile --profile P --machine-id ID [--origin URL]` | C | Select and persist a profile and machine |
| `dots mise ARGS...` | D | **New in 1.0.** mise rooted in the configuration |
| `dots retire-dotfiles` | M | **New in 1.0.** Delete retired dangling links |
| `dots exceptions` | M | Install missing installer exceptions |
| `dots enroll-history OPTIONS` | M, C | Prepare this host for encrypted history |
| `dots install-release-app OWNER/REPO` | M | **New in 1.0** as a public name; mise post-install callback |
| `dots brew path` | R/O | **New in 1.0.** Print the Homebrew executable |
| `dots brew version formula\|cask TOKEN` | R/O | **New in 1.0.** Print an installed version |
| `dots brew admin-available` | R/O | **New in 1.0.** Exit 0 when administrator access is available |
| `dots brew defer-for-admin TOKEN` | R/O | **New in 1.0.** Print the deferral line, exit 3 |
| `dots brew record formula\|cask TOKEN installed\|updated [BEFORE]` | writes `DOTS_CHANGES` | **New in 1.0.** Record a verified change |
| `dots brew update-cask [--auto-updates] [--admin] TOKEN [PROCESS] [--install-only]` | M | **New in 1.0** as a command |
| `dots brew update-formula TOKEN [--install-only]` | M | **New in 1.0** as a command |
| `dots brew hold-formula TOKEN VERSION [REASON]` | R/O | **New in 1.0** as a command |
| `dots brew hold-cask TOKEN VERSION APP_PATH [REASON]` | R/O | **New in 1.0** as a command |

`update` is an alias of `up`, and `uninstall` of `remove`.
Built-in names win over configuration commands of the same name.
**Changed in 1.0:** `root`, `profile`, `mise`, `retire-dotfiles`, `exceptions`, `enroll-history`, `install-release-app`, `brew` and `completions` are new built-in names, so a configuration command with one of these names is no longer reachable.
`dots help` lists the helpers in a separate group from the everyday commands.

### Exit status

| Status | Meaning |
| --- | --- |
| 0 | Success, including a run whose only problems are deferred |
| 1 | A failure, including a missing program for the requested action |
| 2 | A usage or configuration error: unknown command or option, invalid arguments, missing configuration, invalid `[vars]`, missing mise |
| 3 | `dots publish`: deferred because of conflicts; `dots brew defer-for-admin`; an `update-cask` deferral |

Commands that run a child pass its status through unchanged: configuration commands, `dots test`, `dots mise`, and `dots apply`.
`dots up` and `dots bootstrap` exit 0 when every stage completed, was skipped or was deferred, and 1 when any stage failed.
`dots sync` exits 0 after every run that parsed its arguments, including stops, a missing gitleaks and a missing remote; the receipt carries the problem.
`dots sync` with invalid arguments exits 2.
**Changed in 1.0:** `dots bundle` usage errors exit 2; 0.3.5 exited 1.

### Hooks and recipes

A hook or installer exception recipe reports its outcome with its exit status:

| Status | Outcome |
| --- | --- |
| 0 | completed |
| 3 | deferred: needs attention, without failing the run |
| 4 | skipped |
| other | failed |

### Output and reports

Every problem is one issue, listed under "Needs attention" in the dashboard, in `--plain` output and in the final report.
Each issue shows the last lines of the error, the command to run next, and the path of the step's full log.
Reports and logs live under `${XDG_STATE_HOME:-~/.local/state}/dots/`, in directories of mode 0700 and files of mode 0600.
`dots` keeps the ten most recent completed reports.
In the dashboard, `A` on an issue hands it to a coding agent once the run has finished.
`dots` then suspends the dashboard and runs the shell command in `DOTS_AGENT_COMMAND` in the configuration directory, with the issue (its output, log path, configuration root, host, profile and `dots` version) as the last argument.
Without `DOTS_AGENT_COMMAND`, `A` says how to set it.
**Changed in 1.0:** the dashboard, menu and report browser are part of the one executable; there is no separate renderer.

## Command reference

### `dots --version`

It prints `dots VERSION (INSTALLER)`.
`INSTALLER` is `homebrew` when the executable lives in a Homebrew Cellar, `mise` when it lives in a mise install directory, `cargo` when it lives in Cargo's bin directory, and `other` otherwise.
**Changed in 1.0:** `checkout` and `archive` are gone, because there is no support directory to inspect.

### `dots status`

It prints, in order: the version line, the agent skill links, the prerequisites, the selected profile, the machine ID, the source mode and repository, the declared package state, `mise ls --current`, `mise bootstrap dotfiles status`, and the installer exceptions with `ready` or `missing executable: PATH`.
It exits 1 when a section fails, and 0 otherwise.
A missing optional program and a source diagnostic that needs attention are reported without failing the command.
**Changed in 1.0:** the Atuin section is gone, and the prerequisite section is new.

### `dots plan`

It prints mise's bootstrap plan for the selected profile and machine, without installing anything.
It takes no arguments.

### `dots apply [TARGET...]`

It runs `mise bootstrap dotfiles apply TARGET...` in the configuration.

### `dots test [ARGS...]`

It runs `ROOT/tests/run` with `ARGS` unchanged, inside the configuration's mise environment, without auto-install.
**Changed in 1.0:** `tests/run` is executed directly and must be executable; 0.3.5 ran it with `bash`.

### `dots up`

Options: `--mas` includes Mac App Store updates; `--last` reopens the last report; `--last` with `--mas` exits 2.
`dots up` runs these stages in this order:

1. **Pre-update checkpoint.** Saves tracked settings with `mise bootstrap dotfiles save`, after checking that every tracked entry is encrypted.
   With no `mode = "track"` entries, the stage is skipped without attention.
   When it fails, `dots up` starts no update and exits 1.
2. **Sync.** As `dots sync`; skipped with one line in local-only mode.
   A sync problem is reported and later stages still run.
3. **Reload.** Later stages read the configuration as sync left it.
4. **Packages.** Each available package manager with declarations for this profile and machine gets its own pass.
   The cask pass installs missing declared casks and upgrades installed ones, except casks whose metadata marks them as auto-updating and casks listed in `dots_self_updating_casks`.
5. **Tools.** Installs newly declared mise tools, then upgrades declared tools; tools pinned to a revision keep it.
   mise keeps replaced versions (`--no-prune`).
6. **Installer exceptions.** Each selected recipe, with `--install-only` when its row has the `install-only` flag.
7. **Update hooks.** `dots/hooks/update.d`, in name order.
8. **Standalone mise.** `mise self-update`, skipped when a package manager installed mise.
9. **Manager.** When the configuration declares `dots` and a stage above installed a different version, it reports `dots X -> Y, used from the next run`.
10. **Post-update checkpoint.** As stage 1.

With `--mas` on macOS, and declarations that select the `mas` manager, App Store updates run alongside stages 5 to 7, and stage 8 waits for them.
Without cached administrator access, a noninteractive run defers the App Store stage; an interactive one can prompt through `sudo -v`.
`dots up` sets `HOMEBREW_NO_INSTALL_CLEANUP=1`, so Homebrew keeps the old keg of anything it upgrades.
The run finishes on the `dots` version it started with.
A failing stage does not stop later ones.
A configuration that becomes invalid during the run stops it with exit status 1.
`dots up` never prunes software, restarts services, updates project dependencies or publishes the configuration.
Package, tool and standalone mise changes come from snapshots taken before and after their stage.
Hooks and recipes report theirs through `DOTS_CHANGES`.
The sync stage reports the configuration revision it adopted.
**Changed in 1.0:** the pre-update checkpoint is the first stage; 0.3.5 ran it after the sync and reload (`tasks/update:186-216,346`).

### `dots sync`

`dots sync` takes no arguments except `--last`.
It keeps every behaviour that [the sync guide](sync.md) describes for 0.3.5, apart from the remote rules in [Remotes](#remotes).
The rest of this section is the summary an implementation must satisfy.

Shape and locking:

- The checkout is a plain Git checkout at the repository root, with `HEAD` on `dev` and at least one commit.
- Sync stops with one line on a detached `HEAD`, another branch, a `.jj` directory, or `.git/dots-dev.json`.
- `dots bootstrap` moves a fresh clone that is still on `main`, with no edits, to `dev`.
- Sync skips with one line while a merge, rebase, cherry-pick, revert or bisect is in progress, or while another sync or publish holds the repository lock.
- A lock left by a dead process on this host is reclaimed.
- Sync skips with one line when Git cannot run `merge-tree -X` (Git older than 2.43).

One run:

1. Saves tracked history, after refusing any tracked entry that is not `encrypt = true`.
2. Captures edits to copied dotfiles whose source still matches the last common version.
3. Commits every tracked or unignored public file onto `dev` as `Save local setup (HOST)`, leaving machine-local files out.
4. Scans that commit with gitleaks.
   A flagged file stays on disk, out of `dev`; the rest is shared.
   A flagged commit already in `origin/dev..dev` blocks the push.
   Without gitleaks nothing is shared or pushed.
5. Fetches `dev` and `main`, or continues offline.
6. Merges local work with `origin/dev`, then with `origin/main`, outside the live files.
   On a conflicting hunk, `main` wins over `dev` and `dev` wins over local work.
   Binary, file-type and delete conflicts take the published file whole.
   Both parents stay in history.
7. Validates the merged configuration with this machine's local files; an invalid result is neither pushed nor applied.
8. Fast-forwards `origin/dev` only, with a lease on the tip it validated, retrying a lost race up to three times.
   A push that takes longer than 60 seconds is stopped; Git never prompts for credentials.
9. Updates the checkout and applies dotfiles, refusing to overwrite a path edited during the run or obstructed by local material.
   An interrupted checkout is finished by the next run before it takes a new snapshot.

Sync never force-pushes, skips validation, discards uncommitted edits or writes conflict markers into live files.
Each issue carries a diagnostic and a recovery action; [the sync guide](sync.md#verbose-diagnostics) lists every line.
Retired declarations work as described in [Retired dotfiles and retired declarations](#retired-dotfiles-and-retired-declarations).

Receipt:

- Each finished sync, including the one inside `dots up`, writes a receipt atomically with mode 0600.
- A clean run prints one line.
- Otherwise the receipt leads with any issue and its next action, then lists up to five paths per group: Shared on dev, Saved locally, Updated in this checkout, Finished pending checkout.
- In a terminal, standalone sync can show a complete small text diff for up to two paths and twelve displayed lines.
- `dots sync --last` replays the latest receipt for this checkout from the recorded Git revisions, without the network and without mise.
- Replay spends up to three seconds screening patches and keeps up to 1 MiB for the pager.
- A patch is shown only when gitleaks returns a clean JSON report for both file versions and for the displayed text; otherwise the file is listed without its patch.
- Binary, encrypted and unverified changes are listed as file entries.
- Piped output and saved reports contain metadata only.
- **Changed in 1.0:** receipts written by 0.3.5 are ignored; `dots sync --last` then reports that no receipt exists.

### `dots add` and `dots remove`: requests

Each argument after the options is one request.
A request is a **file** when it starts with `/`, `~/`, `./` or `../`.
Every other request is **software**: a GitHub URL, a name with a backend prefix such as `npm:prettier`, or a bare name.
A bare name is always software, even when it starts with a dot, so adopt `~/.zshrc` from `HOME` as `./.zshrc` or `~/.zshrc`.
A relative file request is resolved against the caller's directory.
One invocation may mix file and software requests.
The selectors are `--base` (`config.toml`), `--profile P` (`config.P.toml`) and `--path FILE` (that file); at most one may be given.
Without a selector, `dots add` writes to the selected profile's file, and stops with exit status 2 when no profile is selected.
`--asset` and `--keep-installed` with a file request exit 2.

### `dots add SOFTWARE...`

It installs first and records only what installed.
A bare name uses mise's preferred backend and the latest version.
A backend prefix (`npm:`, `cargo:` and the like) is recorded as a mise tool.
`brew:` prefers a formula and selects a cask only when the formula is confirmed absent; `brew-cask:` always selects a cask.
New Homebrew declarations are restricted to macOS.
`apt:`, `dnf:`, `pacman:`, `apk:` and `mas:` select the other native package managers.
A GitHub URL with a prebuilt command-line release uses mise's GitHub backend.
A macOS app in a ZIP or tar release is recorded with a post-install hook that runs `dots install-release-app OWNER/REPO`.
DMG and pkg releases, and repositories without a release, are refused.
`--asset GLOB` names one release asset and needs exactly one GitHub URL.
A package that an installer exception holds (`hold=` in `dots/exceptions.tsv`) is refused before anything installs.
On an install failure, `dots add` shows the declaration and asks whether to record it for a later retry; the default is no, and a noninteractive run never records it.
With several requests, earlier successes stay recorded and the command stops at the first failure.
It prints `Config destination: FILE` before the first request.

### `dots add FILE...`

**New in 1.0.**
For each file request, with `REL` the target's path relative to `HOME`:

1. The target, after resolving its parent directories, must be a regular file under `HOME` and outside the configuration root.
2. Each parent directory between `HOME` and the target must be a real directory, not a symlink.
3. The source path is `ROOT/dotfiles/REL`.
4. The declaration is `"~/REL" = { source = "dotfiles/REL", mode = "symlink" }` under `[dotfiles]` in the selected file.

`dots add` refuses, with exit status 2 and nothing changed:

- a target that is a symlink, a directory or anything but a regular file;
- a target that any active configuration file already declares;
- a source path that already exists or that another declaration uses;
- two requests in one invocation that resolve to the same target.

Then it copies the target's bytes and mode to the source, adds the declaration while keeping every other byte, comment and order in the file, and replaces the target with a symlink to the source.
When any step fails, it restores the target and removes the source and declaration it added, and exits 1.
Running the same request again after success reports the file as already adopted and exits 0.
`--dry-run` prints the source, declaration and file it would write, and changes nothing.
Afterwards the target is a link and `dots status` lists it.
The new source reaches other machines through the next sync, which scans it with gitleaks first.

### `dots remove TARGET...`

It searches every active configuration file, including the machine-local ones; a selector narrows the search.
A software target is named by its exact declaration (`brew-cask:firefox`), by a short name that matches exactly one declaration, or by a GitHub URL.
`mise:NAME` selects a mise tool when another manager also has `NAME`.
When another active file still declares the software, only the selected declaration goes and the software stays.
mise tools are removed with `mise unuse` and then `mise prune --tools TOOL`, which keeps versions other projects use.
Native packages are removed with one targeted uninstall after checking dependents, never with zap, purge or force.
GitHub apps are moved to the Trash and their release installation is removed.
When a removal cannot be confirmed safe, the command stops.
When a step fails, it restores the declarations and any staged app.
Settings and app data stay where they are.
`--keep-installed` deletes only the declaration.
`--dry-run` prints the plan and changes nothing.

**New in 1.0:** a file target is a symlink that `dots add FILE` would have made: it points at `ROOT/dotfiles/REL`, exactly one active declaration names it with that source and `mode = "symlink"`, and no other declaration uses that source.
For such a target, `dots remove` replaces the link with a regular file holding the source's current bytes and mode, then removes the declaration and the source.
Edits made through the link are therefore kept in the restored file.
Any other file target is refused with exit status 2.
No record of earlier adoptions is needed.

### `dots bootstrap`

```text
dots bootstrap [--bundle FILE] [--identity KEY] [--restore ID] [MISE-BOOTSTRAP-ARGS...]
```

It runs `mise bootstrap --skip-dirty MISE-BOOTSTRAP-ARGS...` in the configuration, with `DOTS_ROOT` exported.
Relative `--bundle` and `--identity` paths are resolved against the caller's directory.
`--identity` names the external age identity for both the bundle and recovery.
`--restore ID` restores that machine's settings history; `ID` must be a valid machine ID, it cannot be combined with `--only`, and it adds `--skip services`.
A configuration connects bootstrap to `dots` in two places:

```toml
# config.laptop.toml
[bootstrap.hooks]
pre-dotfiles = 'SETUP_PROFILE=laptop "$(command -v mise || echo "$HOME/.local/bin/mise")" x github:EzraCerpac/dots -- dots bootstrap persist-selected'

# config.toml
[tasks.bootstrap]
description = "Finish the dots bootstrap"
run = 'dots bootstrap complete'
raw = true
```

The `pre-dotfiles` hook runs before mise installs `[tools]`, with the caller's `PATH`, so it runs `dots` through `mise x`.
The `bootstrap` task runs last, with the tools on `PATH`.

`dots bootstrap persist-selected` persists the profile and machine ID as `dots profile` does.
It keeps an existing persisted profile and its machine ID.
Without one, it takes the profile from `SETUP_PROFILE`, else from a single profile in `MISE_ENV`, and the machine ID from `SETUP_MACHINE_ID`, else from a persisted `host-` selector, else `auto-` followed by a random UUID in lowercase hex without dashes.
With `--restore ID` in effect, the machine ID is `ID`, and a different persisted ID stops it with exit status 2.
It passes `SETUP_HISTORY_ORIGIN` on as `--origin` when set.
It never runs a full bootstrap.

`dots bootstrap complete` runs these stages and prints one result line per stage and a final count:

1. **Private inputs.** Selects a bundle and identity as in [Bootstrap bundle](#bootstrap-bundle), decrypts and validates it, and records the identity path in `config.local.toml` as `vars.bootstrap_bundle_identity`.
2. **Source repository.** Moves a fresh clone that is still on `main`, with no edits, to `dev`.
3. **Restore private settings**, only with `--restore`: as `dots restore --initialize-history`, with the recovery identity.
4. **Installer exceptions**, as `dots exceptions`.
5. **Bootstrap hooks**, `dots/hooks/bootstrap.d` in name order, each its own stage.
6. **Encrypted local history**, only when `dots_state_repo` or `SETUP_HISTORY_ORIGIN` is set: as `dots enroll-history`, then the history origin, `history.sync = "manual"` and mise's history-watch service.

Without a state repository or origin, stage 6 is not listed at all, so an empty bootstrap needs no attention.
Stage 6 is deferred when private recovery is not ready, when no age identity exists, or when no tracked file names this machine's `host-ID` profile yet.
It writes a summary to `${XDG_STATE_HOME:-~/.local/state}/mise/bootstrap/last-run.json` with mode 0600.
It exits 1 when any stage failed, and 0 otherwise.
**Changed in 1.0:** the Atuin, Tailscale and private-file stages are gone; see [What 1.0 drops](#what-10-drops).

### `dots remote`

```text
dots remote HOST --profile P [--dry-run] [--yes] [--identity-file FILE] [--port N] [--connect-timeout S] [--github-relay-repo R] [--github-relay-read-only]
```

It runs `mise bootstrap remote --host HOST --adopt SOURCE --remote-env P --install-mise` with the other options passed through.
`P` must be one of `dots_profiles`.
`SOURCE` is `dots_source_repo`, else the `origin` URL; without either it exits 2.
**Changed in 1.0:** `SOURCE` may be a Git URL.
Assumption, unverified: `mise bootstrap remote --help` (2026.11.9) shows that `--adopt` accepts a URL, but a real remote enrollment from a URL has not been run.

### `dots bundle`

```text
dots bundle create --input PRIVATE.json --output BUNDLE.age --recipient age1... [--recipient age1...]...
dots bundle check --bundle BUNDLE.age --identity KEY
```

`create` validates the input, encrypts it with age to every recipient, and writes the output with exclusive creation and mode 0600.
It refuses an output that already exists.
It writes no plaintext anywhere and prints `Encrypted bootstrap bundle created; no plaintext was written.`
`check` decrypts and validates, and prints `Bootstrap bundle decrypts and validates.`
**Changed in 1.0:** `--from-live` is gone.

### `dots backup` and `dots restore`

See [Settings history](#settings-history).

### `dots publish`

```text
dots publish [--all | --bookmark NAME]
```

It runs in a colocated JJ workspace of the configuration repository, never in the live checkout.
Its first check is that the repository is on GitHub; otherwise it exits 1 with one line naming that requirement, before it looks for JJ, `gh` or gitleaks.
It then needs JJ, `gh` and gitleaks.
By default it publishes exactly one unpublished working change directly above `main@origin`.
`--bookmark NAME` publishes an existing private `wip/...` bookmark.
`--all` publishes the current stack, offering to merge `main@origin` into it first.
It scans the outgoing commits with gitleaks, asks for confirmation, pushes a bookmark without force, and opens or updates the pull request.
When `dots_publish_on_behalf_of` names a person, the pull request text carries a note that a model, named by `DOTS_PUBLISH_MODEL` when set, wrote it on that person's behalf.
It exits 3 when conflicts stop it, and 1 on other failures.

### `dots reports [COMMAND]`

It opens the saved report browser, optionally on one command's reports.
Without a terminal it exits 2 with one line.

### `dots completions bash|zsh|fish`

**New in 1.0.**
It prints a completion script for built-in commands and their options to stdout.

## The `dots/` directory

Every part is optional.

| Path | Purpose |
| --- | --- |
| `hooks/update.d/` | Executables `dots up` runs after packages, tools and installer exceptions |
| `hooks/bootstrap.d/` | Executables `dots bootstrap` runs after installer exceptions |
| `commands/NAME` | An executable run as `dots NAME [ARGS...]` |
| `exceptions.tsv`, `exceptions/NAME` | Apps mise cannot install, each with its own recipe |
| `retired-dotfiles` | Removed dotfile targets whose dangling links may be deleted |
| `retired-declarations` | Stale declarations sync removes from machines' local configuration |

### Hooks

A hook is a regular executable file whose name matches `^[A-Za-z0-9][A-Za-z0-9._-]*(@[A-Za-z0-9._-]+)?$`.
Other names are ignored.
Hooks run in byte order of their names (C locale), from the configuration root, through `mise exec` in the configuration's environment.
Each hook is its own reported stage; a failed hook does not stop later ones.
The stage label is the name without its `@PROFILE` suffix, without a leading number followed by `-` or `_`, and without a final extension, with dashes and underscores as spaces: `20-Login-shell.sh` reports as `Login shell`.
A name ending in `@PROFILE` applies only to that profile; other profiles do not list it.
A suffix that names a profile outside `dots_profiles` fails the whole listing with exit status 2 before any hook runs.

Every hook gets `DOTS_ROOT` and `DOTS_PROFILE`.
Update hooks also get `DOTS_TOOLS_OK`, `DOTS_TOOLS_UPGRADED` and `DOTS_CHANGES`.
Bootstrap hooks also get `DOTS_PRIVATE_READY`, `DOTS_RESTORED` and `DOTS_BUNDLE_FD`.
**Changed in 1.0:** `DOTS_TOOLS_OK`, `DOTS_TOOLS_UPGRADED`, `DOTS_PRIVATE_READY` and `DOTS_RESTORED` are always set, to `1` or `0`; `DOTS_HOME` is no longer set.
`DOTS_CHANGES` names a file; append `ITEM<TAB>BEFORE<TAB>AFTER` lines to it, with `not installed` as `BEFORE` for a new installation, and `dots up` shows them as changes in the hook's report.

### Commands

`dots NAME ARGS...` runs `commands/NAME` when `NAME` is not a built-in, matches `^[A-Za-z0-9][A-Za-z0-9._-]*$`, and is a regular executable file.
The arguments reach it unchanged, and it runs in the caller's directory.
It gets `DOTS_ROOT` and `DOTS_PROFILE`.
`DOTS_PROFILE` keeps a value the caller set; otherwise it is `SETUP_PROFILE`, else `env.SETUP_PROFILE` from `config.local.toml`, else empty.
It runs before mise loads the configuration: no sync, tool installs or package updates.
Its exit status is the command's.

### Installer exceptions

`exceptions.tsv` has one row per app, tab-separated: the profile (`base` for every profile), the recipe name, and optional comma-separated flags.
Empty lines and lines starting with `#` are ignored.
A row with a fourth column, an unknown profile, an invalid name or an unknown flag stops the reading command with exit status 2.
The recipe is the executable `exceptions/NAME`; `NAME` matches `^[A-Za-z0-9][A-Za-z0-9._-]*$`.
A selected row whose recipe is missing or not executable is an error.
`install-only` makes `dots up` pass `--install-only`, so the recipe only installs a missing app.
`hold=MANAGER:NAME` records that the recipe pins that package, so `dots add` refuses it; the value matches `^[a-z][a-z0-9-]*:[A-Za-z0-9@._/+-]+$`, and a request matches it after dropping any tap prefix or `@version`.
Bootstrap always passes `--install-only`.
Under `dots up`, a recipe also gets `DOTS_CHANGES`, like an update hook.
A recipe updates exactly its one app with its own package-manager command, never a whole-environment update.
A recipe keeps the caller's stdin, so an administrator prompt can reach the terminal.

### Retired dotfiles and retired declarations

`retired-dotfiles` lists one `~/PATH` per line; empty lines and `#` lines are ignored.
`dots retire-dotfiles` deletes each listed target only while it is a symlink that points into the configuration root (by its logical or physical path) and whose destination no longer exists.
It ignores, with a warning, an entry that does not start with `~/` or that contains `..`.
It prints `Removed retired link ~/PATH` for each deletion.
Call it from a mise `post-dotfiles` hook.

`retired-declarations` rows are `dotfiles.NAME<TAB>SOURCE`, with `SOURCE` a plain relative path without `.`, `..` or empty parts; other rows are ignored.
Before validating a merged configuration, sync checks it with each machine-local file's matching tables taken out.
A table goes only when it is exactly `[dotfiles.NAME]`, its `source` is exactly `SOURCE`, nothing else defines it, and `SOURCE` does not exist in the merged configuration.
The machine-local file changes only once the merged files are on disk; the rest of it, its mode and the deployed file stay as they were, and a copy goes to `.git/dots-retired/`.
A rendered template is a regular file and is never deleted.

## Helper subcommands

**New in 1.0.**
These replace the shell libraries that configurations sourced from the 0.3.5 manager directory, so a hook or recipe can be written in any language.
Each resolves the configuration root as described in [Configuration root](#configuration-root).

### `dots root`

It prints the physical configuration root followed by a newline.

### `dots profile`

Without options it prints the selected profile; with none selected it exits 2 with one line.
`dots profile --profile P --machine-id ID [--origin URL]` validates `P` against `dots_profiles` and `ID` against the machine ID pattern, persists both as in [Profiles and machine identity](#profiles-and-machine-identity), and prints `Saved local setup profile P for host ID.`

### `dots mise ARGS...`

It runs `mise -C ROOT -E PROFILE[,host-ID] ARGS...` with `MISE_CONFIG_DIR=ROOT`, without `MISE_ENV`, and with `MISE_AUTO_INSTALL=0`.
`ARGS` are passed as an argument array and never evaluated by a shell.
On macOS, for `bootstrap status` and `bootstrap packages`, the directory of the configuration's already-installed Ruby is put first on `PATH`, so Homebrew tap evaluation does not go through a shim; Ruby is never installed for it.
It exits with mise's status.
It replaces `setup_mise` and `setup_init` from `lib/setup-common.sh:9-132`.

### `dots retire-dotfiles`

See [Retired dotfiles and retired declarations](#retired-dotfiles-and-retired-declarations).

### `dots exceptions`

It runs every selected installer exception with `--install-only`, printing `Installer exception: NAME` before each and `Failed installer exception: NAME` on stderr for each failure.
It exits 1 when any failed.

### `dots enroll-history`

```text
dots enroll-history --profile P --machine-id ID --identity PATH [--origin URL] [--recipient age1...]...
```

It persists the profile, machine ID, history branch `host-ID`, identity and recipients in local configuration, and sets the files this machine tracks to mode 0600.
`PATH` is absolute or starts with `~/`, and names a readable regular file.
It does not save a checkpoint, create a history repository, or contact a remote.
It exits 3 when no tracked file names this machine's `host-ID` profile yet.

### `dots install-release-app OWNER/REPO`

mise runs it as the post-install hook of a GitHub release that `dots add` recorded as a macOS app.
It reads `MISE_TOOL_INSTALL_PATH`, requires exactly one app bundle inside it, and replaces `/Applications/APP` only when `dots` installed that app from this repository before, as recorded under `${XDG_STATE_HOME:-~/.local/state}/dots/github-apps`.
It fails on non-macOS systems.
The hook command `dots add` writes runs `dots` without a version directory: a Homebrew install runs the prefix's `bin/dots`; a mise install runs `"${DOTS_MISE_BIN:-$(command -v mise || echo "$HOME/.local/bin/mise")}" x github:EzraCerpac/dots -- dots install-release-app OWNER/REPO`; any other install runs the executable's absolute path.
So the hook keeps working after `dots` upgrades.

### `dots brew`

Each subcommand works on one named package and never runs a global Homebrew update.
On a system other than macOS, `update-cask`, `update-formula`, `hold-formula` and `hold-cask` print `Deferred: TOKEN is macOS-only.` and exit 0.
Install and upgrade commands run with `HOMEBREW_NO_AUTOREMOVE=1` and `HOMEBREW_NO_INSTALL_CLEANUP=1`.

- `path` prints the first of `brew` on `PATH`, `/opt/homebrew/bin/brew` and `/usr/local/bin/brew` whose `brew --prefix` is `/opt/homebrew` or `/usr/local`; with none it exits 1.
- `version formula|cask TOKEN` prints the installed version; when it is unknown it exits 1.
- `admin-available` exits 0 when the user is root, when stdin and stdout are terminals, or when `sudo -n true` succeeds, and 1 otherwise.
- `defer-for-admin TOKEN` prints `Deferred: TOKEN needs interactive administrator authorization; run in a terminal: sudo -v && dots up` and exits 3.
- `record formula|cask TOKEN installed|updated [BEFORE]` appends `brew-KIND:TOKEN<TAB>BEFORE<TAB>AFTER` to `DOTS_CHANGES` when that is set and the package is installed; `installed` writes `not installed` as `BEFORE`, and `updated` writes a line only when `BEFORE` and the installed version are both known and differ.
- `update-cask [--auto-updates] [--admin] TOKEN [PROCESS] [--install-only]` installs a missing cask, or upgrades an outdated one unless `--install-only`; `--auto-updates` compares and upgrades with `--greedy`.
  With `PROCESS`, it defers while a process with exactly that name runs, printing `Deferred: TOKEN is running; close it before installing.` (or `upgrading`) and exiting 3.
  With `--admin`, it defers when administrator access is not available, as `defer-for-admin`.
  It prints `Unchanged: TOKEN` or `Already installed: TOKEN` when there is nothing to do.
  It records each verified installation or upgrade as `record` does.
- `update-formula TOKEN [--install-only]` does the same for one formula, without the process and administrator checks.
- `hold-formula TOKEN VERSION [REASON]` prints `Held: TOKEN VERSION (REASON).` when that version is installed; otherwise it prints a refusal on stderr and exits 1.
- `hold-cask TOKEN VERSION APP_PATH [REASON]` does the same, reading the version from `APP_PATH/Contents/Info.plist`.
- Hold commands never install, upgrade or remove anything.

**Changed in 1.0:** a deferral exits 3; in 0.3.5 the helper returned 0 and sent a notice to the dashboard (`lib/brew-exception.sh:72-75,105-108,218-221`).
A recipe that ends with the helper's status is reported as deferred.

### Replacing 0.3.5 interfaces

| 0.3.5 | 1.0 |
| --- | --- |
| `source "$DOTS_HOME/lib/brew-exception.sh"` and its functions | `dots brew` subcommands |
| `find_real_brew` | `dots brew path` |
| `brew_exception_version`, `brew_exception_record` | `dots brew version`, `dots brew record` |
| `brew_exception_admin_available`, `brew_exception_defer_for_admin` | `dots brew admin-available`, `dots brew defer-for-admin` |
| `update_brew_cask_exception`, `update_brew_formula_exception` | `dots brew update-cask`, `dots brew update-formula` |
| `hold_brew_formula_exception`, `hold_brew_cask_exception` | `dots brew hold-formula`, `dots brew hold-cask` |
| `dots_up_emit_notice` from a recipe | A message on the recipe's output and exit status 3 |
| `source "$DOTS_HOME/lib/setup-common.sh"`; `setup_init`; `setup_mise` | `dots root`, `dots profile`, `dots mise` |
| `setup_error` | Write the message to stderr |
| `quiet_probe` (`lib/quiet-probe.sh`) | Capture the child's output in the script's own language and print it on failure |
| `bash "$DOTS_HOME/lib/retire-dotfiles.sh" "$DOTS_ROOT"` | `dots retire-dotfiles` |
| `dots internal home` | `dots root` for the configuration; nothing for the manager, which has no directory |
| `dots internal persist-selected` | `dots bootstrap persist-selected` |
| `dots internal complete` | `dots bootstrap complete` |
| `dots internal profile` | `dots profile` |
| `dots internal enroll-history` | `dots enroll-history` |
| `dots internal exceptions` | `dots exceptions` |
| `dots internal update`, `status`, `backup`, `restore` | `dots up`, `dots status`, `dots backup`, `dots restore` |
| `dots internal github-install-app` | `dots install-release-app` |
| `dots internal commands`, `profiles`, `sync-present` | Dropped: they served the renderer |

`dots internal` itself is gone; it exits 2 as an unknown command.
A configuration's own tests should exercise these public commands, not manager files.

## Bootstrap bundle

A bundle is age-encrypted UTF-8 JSON:

```json
{ "version": 1, "secrets": { "KEY": "VALUE" }, "recipients": ["age1..."] }
```

- `version` is the number 1.
- `secrets` is an object whose keys are strings and whose values are nonempty strings without NUL.
- `recipients` is optional; each entry matches `^age1[0-9a-z]+$`, and duplicates are dropped.
- Any other top-level field is rejected.

**Changed in 1.0:** any secret key is accepted; 0.3.5 accepted only a fixed list (`bootstrap/bundle.mjs:8-28`).
Every bundle 0.3.5 wrote still validates.

Selection, unchanged from 0.3.5 (`bootstrap/complete.mjs:78-131`):

- The bundle is `--bundle FILE` when given, else `encrypted/bootstrap.PROFILE.json.age` when it exists.
- A bundle is requested when that profile file exists or when a bundle or bundle identity was given or persisted.
- The identity is `--identity KEY`, else the persisted `vars.bootstrap_bundle_identity`, else, for a requested bundle, the recovery identity.
- The recovery identity is `SETUP_AGE_IDENTITY`, else `env.SETUP_AGE_IDENTITY` in `config.local.toml`, else `~/.config/age/keys.txt`.
- A requested bundle whose file or identity is missing is deferred, with `dots bootstrap --bundle FILE --identity FILE` as the next step.

The bundle's recipients, together with the recovery identity's public key, become the recipients of history enrollment.

**New in 1.0:** bundle values reach bootstrap hooks through `DOTS_BUNDLE_FD`.

- Each selected bootstrap hook gets `DOTS_BUNDLE_FD`, the number of an inherited read-only file descriptor.
- Each hook gets its own descriptor, readable from the start, holding the UTF-8 JSON `secrets` object.
- The object is `{}` when no bundle was requested, and `{}` when private recovery is not ready.
- Private recovery is ready when any requested bundle decrypted and validated, and any requested restore succeeded.
- `DOTS_PRIVATE_READY` is `1` when private recovery is ready and `0` otherwise, so a hook can tell "nothing requested" (`{}` and `1`) from "requested and failed" (`{}` and `0`).
- `DOTS_RESTORED` is `1` only when this run restored settings history.
- Values arrive intact, including newlines.
- No value is written to a plaintext file, put in an environment variable or an argument, or printed by `dots`.
- No other stage, child or command gets the descriptor.
- The hook's stdin is unchanged, so it can still prompt.
- A hook that uses the descriptor is responsible for what it prints and for closing the descriptor before starting long-lived children.

Example, a hook that writes one value to a private file:

```sh
#!/bin/sh
set -eu
[ "$DOTS_PRIVATE_READY" = 1 ] || { echo 'private inputs not ready'; exit 3; }
value="$(jq -r '.example_token // empty' <&"$DOTS_BUNDLE_FD")"
[ -n "$value" ] || exit 4
umask 077
printf '%s\n' "$value" > "$HOME/.config/example/token"
```

Applying app files, credential helpers and service enrollment from bundle values is the configuration's job; `dots` has no built-in behaviour for any app.

## Settings history

`dots backup` and `dots restore` keep app-written files in mise's encrypted dotfile history, in the private `dots_state_repo`, on one `host-ID` branch per machine.
Each such file is a native track entry with one `host-ID` profile variant per machine that keeps it:

```toml
# config.laptop.toml
[dotfiles]
"~/.config/app/state.json" = { mode = "track", encrypt = true, variants = [{ profile = "host-my-laptop" }] }
```

The variant names that machine's stream in history (`home@host-my-laptop/...`), so another machine never applies it.
A variant that has history must never be renamed or removed.
A tracked file is never saved unless it is declared `encrypt = true`; every spelling of its target in its declaring file must say so.
Tracked files end at mode 0600.
`dots_state_repo` is required for backup and restore, and follows the rules in [Remotes](#remotes).

`dots backup` takes no arguments.
It checks, in order, and stops with exit status 2 at the first failure:

1. `history.sync` is `manual`.
2. The machine ID is set, and the branch is `host-ID`.
3. `dots_state_repo` is valid.
4. Every tracked entry is encrypted and uses this machine's `host-ID` variant.
5. mise's effective history origin branch is `host-ID`.
6. Every effective push URL of the history store, after rewrites, matches `dots_state_repo`.

Then it runs `mise bootstrap dotfiles save` and `mise bootstrap dotfiles sync`, and prints `Settings backup published.`

`dots restore [--initialize-history]` follows [the task notes](../tasks/README.md#dots-restore):

- It needs `SETUP_HISTORY_ORIGIN` in ignored local configuration, matching `dots_state_repo`.
- It never adopts private history into the public source checkout.
- Before anything else it checks the mise history watcher process and its launchd or systemd service, and stops when the watcher runs or its state cannot be confirmed; it never stops services itself.
- It never saves or publishes during restore; it starts in fetch-only mode, and publication stays a separate `dots backup`.
- A new store is staged and validated as a bare clone before it is installed.
- The fetched remote commit is applied as the initial checkpoint.
- A private pending marker lets an interrupted first restore resume.
- An existing store connected elsewhere is replaced only with `--initialize-history`; the old store moves to a mode-0700 sibling `repo.git.pre-restore.*`, and a failed clone leaves it in place.
- It refreshes the machine's history identity and recipients, sets exactly the tracked files active for this machine to mode 0600, and sets history sync to `manual`.
- It needs Git and the age identity to be present, and installs nothing.

## Environment variables

Read by `dots`:

| Variable | Meaning |
| --- | --- |
| `DOTS_ROOT` | Selects the configuration |
| `MISE_CONFIG_DIR`, `XDG_CONFIG_HOME` | Fallbacks for the configuration root |
| `DOTS_MISE_BIN` | The mise executable to use; else `~/.local/bin/mise`, else `mise` on `PATH` |
| `DOTS_SKILL_LINKS` | `0` turns agent skill linking off |
| `DOTS_AGENT_COMMAND` | Shell command for the dashboard's agent handoff |
| `DOTS_PUBLISH_MODEL` | Model named in the `dots publish` disclosure note |
| `DOTS_PROFILE` | Kept for configuration commands when already set |
| `SETUP_PROFILE`, `SETUP_MACHINE_ID` | The profile and machine; persisted in `config.local.toml`, which mise exports |
| `SETUP_HISTORY_ORIGIN`, `SETUP_AGE_IDENTITY` | History origin and recovery identity; persisted in `config.local.toml` |
| `MISE_ENV` | Read by `persist-selected` for a single profile; removed from mise children |
| `XDG_STATE_HOME` | Base of `dots` state, reports and logs |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | Where agents keep their configuration |

**Changed in 1.0:** a `DOTS_MISE_BIN` that is set but not executable exits 2; 0.3.5 fell back silently.
Set by `dots` for children: `DOTS_ROOT`, `DOTS_PROFILE`, `SETUP_PROFILE`, `SETUP_MACHINE_ID`, `MISE_CONFIG_DIR`, and for `dots up` `HOMEBREW_NO_INSTALL_CLEANUP=1`; the hook variables are in [Hooks](#hooks).
Variables that are flags take exactly `0` or `1`.
`dots bootstrap` passes its options to its own completion task through environment variables; their names are not part of this contract.

## Agent skill

The agent skill is embedded in the executable.
`dots` writes it to a directory it owns and links it for the agents in use, on every invocation:

| Link | Made when |
| --- | --- |
| `${CLAUDE_CONFIG_DIR:-~/.claude}/skills/dots` | That directory's parent agent directory exists |
| `~/.agents/skills/dots` | `~/.agents` or `${CODEX_HOME:-~/.codex}` exists |

The location of the extracted skill is not part of this contract.
`dots` replaces only links it made that still point where it left them, recorded in `${XDG_STATE_HOME:-~/.local/state}/dots/skill-links`.
A directory, a file or any other link at those paths belongs to the user, and `dots` leaves it alone with a one-time notice.
`dots` never writes outside the agent's directory and never creates an agent directory.
`dots status` shows each link.
`DOTS_SKILL_LINKS=0` turns linking off.

## mise behaviour this contract relies on

Verified on mise 2026.11.9 with an isolated configuration:
`mise config get`, `mise -E default,host-example exec`, `mise x -- COMMAND` without a tool, `mise bootstrap dotfiles apply` creating a link, `mise bootstrap plan`, `mise bootstrap dotfiles status --json`, and the `--adopt URL` syntax in `mise bootstrap remote --help`.
Relied on as 0.3.5 uses them, and not verified again for this document:
`mise x TOOL -- COMMAND` installing a pinned tool, `mise bootstrap --skip-dirty` and `--skip services`, `mise bootstrap dotfiles save`, `sync`, `paths --json` and `rollback`, `mise bootstrap packages`, `mise bootstrap services apply`, `mise dot status --json`, `mise upgrade --no-prune`, `mise unuse`, `mise prune --tools`, `mise self-update`, `mise ls --current`, `mise bootstrap files apply`, and a real `mise bootstrap remote` enrollment.

## What 1.0 drops

| 0.3.5 | Reason |
| --- | --- |
| `DOTS_HOME` and `dots internal home` | One executable has no manager directory |
| The `dots internal` namespace | Replaced by public commands; see [Replacing 0.3.5 interfaces](#replacing-035-interfaces) |
| The sourced libraries in `lib/` (`brew-exception.sh`, `setup-common.sh`, `retire-dotfiles.sh`, `quiet-probe.sh`, `mise-bin.sh`) | Replaced by subcommands, so hooks need no Bash |
| The `dots-up` renderer, its `--capabilities` handshake (`dots-shared-cli/3`), `DOTS_UP_BIN`, `--backend` and the event protocol | The dashboard is in the one process |
| The Bash updater fallback and its `.fallback` logs | There is nothing to fall back from |
| Bash, Node.js, uv and jq as requirements | The executable needs none of them |
| `dots atuin-bundle`, `dots atuin-login`, `dots_atuin_username`, `encrypted/atuin.json.age`, the Atuin status section and bootstrap stage | App enrollment belongs to configurations, as hooks and commands |
| The Tailscale bootstrap stage and its auth-key handling | As above |
| Built-in private files from the bundle (an app config file written through mise's file resources) and `dots bundle create --from-live` | As above; bundle values reach hooks through `DOTS_BUNDLE_FD` |
| `GH_TOKEN` and a GitHub credential helper injected from a bundle value | As above; a hook can do it for its own children |
| The fixed list of bundle secret keys | Keys are arbitrary now that no key has built-in meaning |
| Sync receipts written by 0.3.5 | The receipt format is internal; old receipts are ignored |
| `SETUP_CONFIG_ROOT`, `SETUP_MISE_BIN`, `MISE_BIN` as overrides, `BREW_EXCEPTION_PLISTBUDDY` | Test fixtures and duplicates of `DOTS_ROOT` and `DOTS_MISE_BIN` |
| `checkout` and `archive` in `dots --version` | There is no installed directory to inspect |
| Running `tests/run` with `bash` | The manager no longer requires Bash |
| `dots reports` failing for a missing renderer | There is no renderer |

## Coverage of the 0.3.5 configuration reference

| 0.3.5 item | Here |
| --- | --- |
| README: `DOTS_ROOT` and its defaults | [Configuration root](#configuration-root) |
| README: libraries find the root wherever `dots` is installed | Dropped; `dots root` |
| README: `config.toml`, `config.PROFILE.toml`, `config.host-ID.toml`, `dots/`, `.gitleaks.toml`, `.gitleaksignore`, `encrypted/bootstrap.PROFILE.json.age`, `tests/run`, `miserc.toml`, `config.local.toml`, `.gitignore` | [Files in the root](#files-in-the-root) |
| README: `encrypted/atuin.json.age` | Dropped |
| README: `dots init` copies the `dots/` reference | [`dots init`](#dots-init-dir) |
| README: profiles and settings, `[vars]` keys | [Settings in `[vars]`](#settings-in-vars), [Profiles and machine identity](#profiles-and-machine-identity) |
| README: `dots_atuin_username` | Dropped |
| README: settings history | [Settings history](#settings-history) |
| README: bootstrap, the hook and the task | [`dots bootstrap`](#dots-bootstrap) |
| `dots/` reference: the table of paths | [The `dots/` directory](#the-dots-directory) |
| `dots/` reference: settings paragraph | [Settings in `[vars]`](#settings-in-vars) |
| `dots/` reference: hooks, names, labels, `@PROFILE`, environment, exit codes, example | [Hooks](#hooks), [Hooks and recipes](#hooks-and-recipes) |
| `dots/` reference: commands | [Commands](#commands-1) |
| `dots/` reference: installer exceptions | [Installer exceptions](#installer-exceptions) |
| `dots/` reference: retired dotfiles and retired declarations | [Retired dotfiles and retired declarations](#retired-dotfiles-and-retired-declarations) |
| Task notes: Homebrew recipe helpers | [`dots brew`](#dots-brew) |
| Task notes: `dots up` | [`dots up`](#dots-up) |
| Task notes: `dots restore` | [Settings history](#settings-history) |

## Open questions for review

- Hooks and recipes find `dots` on `PATH`; `dots` does not put its own directory on their `PATH`.
  A configuration that installs `dots` as a mise tool gets it there through `mise exec`.
  Review whether children need a guaranteed way to reach the running executable.
- The proposed repository name `dotfiles` in the `init` remote offer, and the placeholder commit identity `dots <dots@localhost>`, are candidates.
- `init` exits 0 when only the optional remote step failed.
