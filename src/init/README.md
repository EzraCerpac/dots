# dots/

Everything in this directory is how this configuration talks to `dots`.
None of it is required, so remove whatever you have no use for.
`dots init` creates this file and its neighbours only where nothing exists yet, and leaves existing files untouched.
It writes this guide, `exceptions.tsv`, `retired-dotfiles` and `retired-declarations`, and creates `hooks/update.d/`, `hooks/bootstrap.d/`, `commands/` and `exceptions/` empty: every hook, command and recipe in this guide is an example for you to add.
`dots help` lists the commands, and `dots COMMAND --help` shows how a built-in command is used.
`dots test` and `dots mise` are the exceptions: they hand `--help` to the program they start.

| Path | Purpose |
| --- | --- |
| `hooks/update.d/` | Executables that `dots up` runs once packages, tools and installer exceptions are done |
| `hooks/bootstrap.d/` | Executables that `dots bootstrap` runs once installer exceptions are done |
| `commands/NAME` | An executable you call as `dots NAME [ARGS...]` |
| `exceptions.tsv`, `exceptions/NAME` | Apps that mise cannot install, each with a recipe of its own |
| `retired-dotfiles` | Dotfile targets you removed, whose dangling links may be cleaned up |
| `retired-declarations` | Outdated declarations that sync takes out of machines' local files |

## Settings

The only settings `dots` reads are the `[vars]` in the root `config.toml`.
Every mise `[vars]` value is a string, so a list is written as one string of words separated by spaces.

```toml
[vars]
dots_profiles = "laptop server"
# dots_source_repo = "OWNER/dotfiles"            # pins this configuration's own repository
# dots_state_repo = "OWNER/dotfiles-state"       # private repository for encrypted settings history
# dots_self_updating_casks = "some-cask"         # casks dots up only installs when missing
```

- `dots_profiles` must be set.
  Profile names use lowercase letters, digits and `-`, begin with a letter or digit, and cannot be `base`, `local` or anything beginning with `host-`.
  A profile missing from this list is refused everywhere: in commands, in hook suffixes and in installer exception rows.
- `dots_source_repo` and `dots_state_repo` each accept GitHub `OWNER/NAME`, a GitHub URL or any other Git URL.
  An `origin` remote is enough to share the configuration when `dots_source_repo` is unset; with neither, the configuration stays on this machine.
- The source repository and the state repository cannot be the same repository.

## What children get

Hooks, recipes, configuration commands, and the children of `dots test` and `dots mise` all receive:

- `DOTS_ROOT`: the configuration root, as a physical path.
- `DOTS_PROFILE`: the profile this machine selected.

When `dots` starts a child itself, as it does for a hook, a recipe, a configuration command or `tests/run`, the running `dots` comes first on `PATH`, after the configuration's environment has been applied.
Calling `dots` from inside that child therefore reaches the same executable, at the same version.
Programs launched through an explicit `dots mise exec` or `dots mise run` get `PATH` from mise instead, so the configured tool directories and `[env] _.path` determine which `dots` they see.
The callbacks in mise's native bootstrap use the `dots` version that the configuration pins; see [Connecting mise bootstrap](#connecting-mise-bootstrap).

Hooks, recipes and commands need nothing beyond their own interpreter, such as `sh` for a shell script or Python for a Python script.
Rather than sourcing shell code, they can call these helpers:

| Helper | Does |
| --- | --- |
| `dots root` | Prints the configuration root |
| `dots profile` | Prints the selected profile |
| `dots mise ARGS...` | Runs mise in this machine's configuration environment |
| `dots retire-dotfiles` | Deletes dangling links to retired dotfiles |
| `dots exceptions` | Installs missing installer exceptions |
| `dots brew SUBCOMMAND` | Homebrew steps for one package; see [Homebrew recipes](#homebrew-recipes) |

## Hooks

A hook is a regular executable file.
Its name begins with a letter or digit, goes on with letters, digits, `.`, `_` or `-`, and may end with `@PROFILE`.
Files named any other way are skipped.
Hooks run one after another in the byte order of their names, from the configuration root and inside the configuration's mise environment, and each is reported as a stage of its own.
When one hook fails, the later ones still run.

To get the stage label, `dots` drops the `@PROFILE` suffix, a leading number followed by `-` or `_`, and a final extension, then turns dashes and underscores into spaces.
So `hooks/update.d/20-Login-shell.sh@laptop` is reported as `Login shell`, and runs only on machines whose profile is `laptop`.
If a suffix names a profile that `dots_profiles` does not list, listing that directory fails and none of its hooks run.

A hook's exit status decides its outcome:

| Status | Outcome |
| --- | --- |
| 0 | completed |
| 3 | deferred: needs attention without failing the run |
| 4 | skipped |
| other | failed |

In an interactive update shown in terminal scrollback, the terminal belongs to the hook or recipe from the moment its step begins.
Type answers to its prompts directly; only the child sees them.
When such a step fails, the report states the exit status and next action, and says “Output was shown on the terminal.”
In concise output, which covers headless and `--plain` updates, the child reads the caller's stdin and writes its output to a private log.
If it fails, the report quotes the end of that log, and `--verbose` also shows the output while it runs.

Should Ctrl-Z suspend `dots` in a session that has no job-control shell, resume it by sending `SIGCONT` to the `dots` process from a second session.

### Update hooks

Update hooks receive three more variables:

- `DOTS_TOOLS_OK`: `1` if this run installed the newly declared tools successfully, otherwise `0`.
- `DOTS_TOOLS_UPGRADED`: `1` if that worked and the upgrade of declared tools worked as well, otherwise `0`.
- `DOTS_CHANGES`: the path of a file.
  Every `ITEM<TAB>BEFORE<TAB>AFTER` line the hook appends there shows up as a change in its report; use `not installed` as `BEFORE` for something newly installed.

For example, `hooks/update.d/50-Fonts@laptop` counts the installed fonts around a font installer.
It needs `fc-list` and an executable `$DOTS_ROOT/scripts/install-fonts` that you provide; `dots init` sets up neither:

```sh
#!/bin/sh
set -eu
[ "$(uname -s)" = Darwin ] || { echo 'macOS only'; exit 4; }
before=$(fc-list | wc -l | tr -d ' ')
"$DOTS_ROOT/scripts/install-fonts"
printf 'fonts\t%s\t%s\n' "$before" "$(fc-list | wc -l | tr -d ' ')" >> "$DOTS_CHANGES"
```

### Bootstrap hooks

Bootstrap hooks receive three more variables:

- `DOTS_PRIVATE_READY`: `1` if every requested bundle was decrypted and validated and every requested restore worked, otherwise `0`.
- `DOTS_RESTORED`: `1` if this run restored settings history, otherwise `0`.
- `DOTS_BUNDLE_FD`: the number of a read-only file descriptor that holds the bundle's `secrets` object as UTF-8 JSON.

Every hook gets a descriptor of its own, positioned at the start.
It holds `{}` when private inputs are not ready, when nobody asked for a bundle, and when a valid bundle carries no secrets.
So test `DOTS_PRIVATE_READY` to learn whether recovery succeeded, and look in the object for each secret the hook relies on.
`dots` itself never prints a value or puts one in a plaintext file, an environment variable or an argument.
What the hook prints, where it writes values, whether it preserves files that already exist, and closing the descriptor before starting long-lived children are all up to the hook.

For example, a hook that only checks that a value is present:

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

`dots` knows nothing about particular apps.
Hooks like this one are where app files, credential helpers and service enrolment get written from bundle values.

## Commands

`dots NAME ARGS...` runs `commands/NAME` when three things hold: `NAME` is not a built-in command, it begins with a letter or digit and goes on with letters, digits, `.`, `_` or `-`, and the file is a regular executable.
`internal` is reserved, so an executable by that name never runs as a command.
Since built-in commands take precedence, a command called `root`, `profile`, `mise` or `brew`, for instance, can never be reached.

- It receives its arguments untouched, `--help` among them.
- Its working directory is the caller's, not the configuration root.
- `DOTS_PROFILE` keeps any value the caller set, and is otherwise the persisted profile or empty.
- It starts before mise loads the configuration, so no sync, tool install or package update runs ahead of it.
- A global flag such as `--plain` in front of a configuration command makes `dots` exit 2.

Plain `dots` and `dots help` both list the configuration's commands next to the built-in ones.

## Installer exceptions

Installer exceptions are for apps that no mise backend can install.
`exceptions.tsv` holds one tab-separated row per app: the profile (`base` meaning every profile), the recipe name, and an optional list of flags separated by commas.
Blank lines and lines that begin with `#` are skipped.

```text
laptop	example-app	install-only
base	example-tool	hold=brew:example
```

- The recipe is the executable `exceptions/NAME`, and a selected row whose recipe is missing is an error.
- `dots exceptions` exits 2 on a row with a fourth column, an unknown profile, an invalid name or an unknown flag.
  Under `dots bootstrap`, the same row fails the exceptions stage; later stages still run, and the bootstrap exits 1 when it finishes.
- `install-only` makes `dots up` pass `--install-only`, so the recipe installs the app only when it is missing; it suits apps that update themselves.
- `hold=MANAGER:NAME` declares that the recipe manages that package, and `dots add` then refuses the package whatever tap prefix or `@version` the request carries.
- `dots bootstrap` and `dots exceptions` pass `--install-only` every time.
- Under `dots up`, recipes receive `DOTS_CHANGES` exactly as update hooks do.
- Recipes use the same exit statuses as the [hook table](#hooks).

Each recipe updates only its own app, through that app's own package-manager command, and never updates the whole environment.
Like a hook, a recipe may prompt.

### Homebrew recipes

Every `dots brew` subcommand acts on a single named package, and none of them runs a global Homebrew update, autoremove or cleanup.

| Subcommand | Does |
| --- | --- |
| `update-cask [--auto-updates] [--admin] TOKEN [PROCESS] [--install-only]` | Installs the cask if it is missing and, without `--install-only`, upgrades it if it is outdated. When a change is due, it defers with exit 3 instead while `PROCESS` is running, or when `--admin` asks for administrator access that is not available |
| `update-formula TOKEN [--install-only]` | Does the same for one formula, with no process or administrator check |
| `hold-formula TOKEN VERSION [REASON]` | Succeeds when exactly that version is installed, else refuses with exit 1 |
| `hold-cask TOKEN VERSION APP_PATH [REASON]` | Does the same, taking the version from the app bundle at `APP_PATH` |
| `path`, `version formula\|cask TOKEN` | Print the Homebrew executable, or a package's installed version |
| `admin-available` | Exits 0 when running as root, when sudo access is cached, or when stdin and stdout are a terminal where authorization can prompt; exits 1 otherwise |
| `defer-for-admin TOKEN` | Says `TOKEN` needs interactive administrator access and exits 3 |
| `record formula\|cask TOKEN installed\|updated [BEFORE]` | Appends a change line if `DOTS_CHANGES` is set; recording an update needs two known versions that differ |

Pass `--auto-updates` to let `update-cask` upgrade a cask that updates itself.
`update-cask` and `update-formula` already record each installation or upgrade they verify.
Outside macOS, the first four subcommands say the package is macOS-only and exit 4, so the recipe is reported as skipped.
The hold subcommands never install, upgrade or remove anything.

`exceptions/example-app`, which updates a cask unless its app is running:

```sh
#!/bin/sh
exec dots brew update-cask --admin example-app 'Example App' "$@"
```

Here `example-app` stands for the cask token and `Example App` for the name of the app's running process.
The recipe needs a Homebrew source that provides that cask, and administrator access when an install or upgrade is due.

`exceptions/example-tool`, which checks a formula's installed version:

```sh
#!/bin/sh
exec dots brew hold-formula example 1.2.3 'example plugin needs 1.2'
```

`example` and `1.2.3` are placeholders, and the formula must already be installed at exactly that version.
`hold-formula` verifies exactly that and installs nothing.
The row with `hold=brew:example` in `exceptions.tsv`, shown under [Installer exceptions](#installer-exceptions), makes `dots add` refuse the formula; neither that row nor this recipe stops Homebrew from upgrading it.

## Retired dotfiles and retired declarations

`retired-dotfiles` holds one `~/PATH` per line, and skips blank lines and `#` lines.
`dots retire-dotfiles` deletes a listed target only while it is a symlink into the configuration root, by logical or physical path, and its destination is gone.
Entries that do not begin with `~/`, or that contain `..`, are skipped with a warning, and every removed link is reported.
Run it from a mise `post-dotfiles` hook, which reaches `dots` the same way the [bootstrap hook](#connecting-mise-bootstrap) does:

```toml
[bootstrap.hooks]
post-dotfiles = '"$(command -v mise || echo "$HOME/.local/bin/mise")" x github:EzraCerpac/dots -- dots retire-dotfiles'
```

`retired-declarations` handles a `[dotfiles.NAME]` table that some setup script once put in machines' `config.local.toml` or `config.host-*.toml`, after this configuration has deleted that table's source.
Left alone, such a table makes `dots sync` on each of those machines reject the configuration.
Each row is `dotfiles.NAME<TAB>SOURCE`.
`SOURCE` is a relative path split by `/`.
It may not contain a backslash, and none of its components may be `.`, `..` or empty, which also rules out a leading `/`, a trailing `/` and `//`.
Any row that does not fit this shape is skipped.
Commit the row together with the removal of its source.

Sync validates the merged configuration as if each machine-local file's matching tables were already gone.
A table is taken out only when it is exactly `[dotfiles.NAME]`, its `source` is exactly `SOURCE`, no other file defines it, and `SOURCE` is absent from the merged configuration.
The machine-local file is rewritten only once the merged files are on disk, and only if it is still the file sync validated.
Everything else in that file, its mode and the deployed file stay as they were; sync keeps a backup and prints where it is.
A rendered template is an ordinary file, and it is never deleted.

## Connecting mise bootstrap

`dots bootstrap` runs mise's native bootstrap, and that bootstrap calls `dots` back at two points.
Each profile file saves the machine's profile before dotfiles are applied, and the base file runs the final completion step:

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

Because the `pre-dotfiles` hook may run before `mise` is on `PATH`, it finds mise on its own and runs the `dots` version pinned in `[tools]`.
`dots bootstrap complete` then reads the private inputs, switches a fresh clone to `dev`, restores settings history if asked, installs installer exceptions, runs `hooks/bootstrap.d/`, and prepares encrypted history when a state repository is configured.
