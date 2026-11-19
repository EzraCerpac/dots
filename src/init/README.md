# dots/

This directory connects this configuration to `dots`.
Every part is optional; delete what you do not use.
`dots init` writes this file and the others here only when they are absent, and never overwrites them.
`dots help` lists every command, and `dots COMMAND --help` prints a built-in command's usage, except that `dots test` and `dots mise` forward `--help` to the program they run.

<!-- TODO(P-03): Run the font hook and the Homebrew recipe examples on a real macOS machine; they have not been run against the integrated binary. -->

| Path | Purpose |
| --- | --- |
| `hooks/update.d/` | Executables `dots up` runs after packages, tools and installer exceptions |
| `hooks/bootstrap.d/` | Executables `dots bootstrap` runs after installer exceptions |
| `commands/NAME` | An executable run as `dots NAME [ARGS...]` |
| `exceptions.tsv`, `exceptions/NAME` | Apps mise cannot install, each with its own recipe |
| `retired-dotfiles` | Removed dotfile targets whose dangling links may be deleted |
| `retired-declarations` | Stale declarations sync removes from machines' local files |

## Settings

`dots` reads its settings from `[vars]` in the root `config.toml` only.
mise `[vars]` values are strings, so a list is one string of space-separated words.

```toml
[vars]
dots_profiles = "laptop server"
# dots_source_repo = "OWNER/dotfiles"            # pins this configuration's own repository
# dots_state_repo = "OWNER/dotfiles-state"       # private repository for encrypted settings history
# dots_self_updating_casks = "some-cask"         # casks dots up only installs when missing
# dots_publish_on_behalf_of = "Your Name"        # dots publish notes that a model wrote the pull request text
```

- `dots_profiles` is required.
  A profile name uses lowercase letters, digits and `-`, starts with a letter or digit, and is not `base`, `local` or a name starting with `host-`.
  `dots` refuses any profile this setting does not list, in commands, hook suffixes and installer exception rows alike.
- `dots_source_repo` and `dots_state_repo` take GitHub `OWNER/NAME`, a GitHub URL or another Git URL.
  Without `dots_source_repo`, an `origin` remote is enough to share the configuration; without either, it stays local.
- The source and state repositories must be different repositories.

## What children get

Every hook, recipe, configuration command, `dots test` child and `dots mise` child gets:

- `DOTS_ROOT`: the physical configuration root.
- `DOTS_PROFILE`: the selected profile.

A child that `dots` starts directly, such as a hook, a recipe, a configuration command or `tests/run`, has the running `dots` first on `PATH`, after any configuration environment is applied.
A `dots` call inside it is therefore the same executable at the same version.
A program started by an explicit `dots mise exec` or `dots mise run` follows mise's own `PATH` rules instead, so configured tool directories and `[env] _.path` decide which `dots` it finds.
The callbacks mise's native bootstrap runs use the `dots` version the configuration pins; see [Connecting mise bootstrap](#connecting-mise-bootstrap).

A hook, recipe or command needs only its own interpreter, such as `sh` for a shell script or Python for a Python script.
It can call these helper commands instead of sourcing shell code:

| Helper | Does |
| --- | --- |
| `dots root` | Prints the configuration root |
| `dots profile` | Prints the selected profile |
| `dots mise ARGS...` | Runs mise in this machine's configuration environment |
| `dots retire-dotfiles` | Deletes dangling links to retired dotfiles |
| `dots exceptions` | Installs missing installer exceptions |
| `dots brew SUBCOMMAND` | Homebrew steps for one package; see [Homebrew recipes](#homebrew-recipes) |

## Hooks

A hook is a regular executable file whose name starts with a letter or digit and continues with letters, digits, `.`, `_` or `-`, optionally followed by `@PROFILE`.
Other names are ignored.
Hooks run in byte order of their names, from the configuration root, in the configuration's mise environment, each as its own reported stage.
A failed hook does not stop later ones.

The stage label is the name without its `@PROFILE` suffix, a leading number followed by `-` or `_`, and a final extension, with dashes and underscores as spaces.
`hooks/update.d/20-Login-shell.sh@laptop` reports as `Login shell`, and only on machines whose profile is `laptop`.
A suffix naming a profile outside `dots_profiles` fails the listing before any hook in that directory runs.

The exit status is the outcome:

| Status | Outcome |
| --- | --- |
| 0 | completed |
| 3 | deferred: needs attention without failing the run |
| 4 | skipped |
| other | failed |

During an interactive update in terminal scrollback, a hook or recipe owns the terminal from the start of its step.
Answer its prompts directly. Only the child reads the reply.
If the step fails, its report gives the exit status and next action and says “Output was shown on the terminal.”
In concise output, including headless and `--plain` updates, the child reads the caller's stdin and its output goes to a private log.
A failure includes the last lines of that log. With `--verbose`, the output is also displayed as it arrives.

If Ctrl-Z suspends `dots` in a session without a job-control shell, send `SIGCONT` to the `dots` process from another session to resume it.

### Update hooks

Update hooks also get:

- `DOTS_TOOLS_OK`: `1` when installing newly declared tools succeeded in this run, else `0`.
- `DOTS_TOOLS_UPGRADED`: `1` when that succeeded and upgrading declared tools succeeded too, else `0`.
- `DOTS_CHANGES`: a file.
  Each `ITEM<TAB>BEFORE<TAB>AFTER` line appended to it appears as a change in the hook's report, with `not installed` as `BEFORE` for a new installation.

For example, `hooks/update.d/50-Fonts@laptop`:

```sh
#!/bin/sh
set -eu
[ "$(uname -s)" = Darwin ] || { echo 'macOS only'; exit 4; }
before=$(fc-list | wc -l | tr -d ' ')
"$DOTS_ROOT/scripts/install-fonts"
printf 'fonts\t%s\t%s\n' "$before" "$(fc-list | wc -l | tr -d ' ')" >> "$DOTS_CHANGES"
```

### Bootstrap hooks

Bootstrap hooks also get:

- `DOTS_PRIVATE_READY`: `1` when every requested bundle decrypted and validated and every requested restore succeeded, else `0`.
- `DOTS_RESTORED`: `1` when this run restored settings history, else `0`.
- `DOTS_BUNDLE_FD`: the number of a read-only file descriptor holding the bundle's `secrets` object as UTF-8 JSON.

Each hook gets its own descriptor, readable from the start.
The object is `{}` when recovery is not ready, when no bundle was requested, or when a valid bundle has no secrets.
Check `DOTS_PRIVATE_READY` for readiness and check the object for each secret your hook needs.
`dots` never writes a value to a plaintext file, an environment variable or an argument, and never prints one.
The hook is responsible for what it prints, where it writes values, keeping files that already exist, and closing the descriptor before it starts long-lived children.

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

`dots` has no built-in behaviour for any app.
Writing app files, credential helpers or service enrollment from bundle values belongs in hooks like this one.

## Commands

`dots NAME ARGS...` runs `commands/NAME` when `NAME` is not a built-in command, starts with a letter or digit and continues with letters, digits, `.`, `_` or `-`, and the file is a regular executable.
The name `internal` is reserved even when an executable has that name.
Built-in commands win, so a command named `root`, `profile`, `mise` or `brew`, for example, is never reached.

- The arguments reach it unchanged, `--help` included.
- It runs in the caller's directory, not the configuration root.
- `DOTS_PROFILE` keeps a value the caller set; otherwise it is the persisted profile, or empty.
- It runs before mise loads the configuration: no sync, tool installs or package updates happen first.
- Global flags such as `--plain` before a configuration command exit 2.

Both `dots` and `dots help` list the configuration's commands alongside the built-ins.

## Installer exceptions

An installer exception covers an app that no mise backend installs.
`exceptions.tsv` has one row per app, tab-separated: the profile (`base` for every profile), the recipe name, and optional comma-separated flags.
Empty lines and lines starting with `#` are ignored.

```text
laptop	example-app	install-only
base	example-tool	hold=brew:example
```

- The recipe is the executable `exceptions/NAME`; a selected row without one is an error.
- A row with a fourth column, an unknown profile, an invalid name or an unknown flag makes `dots exceptions` exit 2. During bootstrap the exceptions stage fails, later stages can run, and the completed bootstrap exits 1.
- `install-only` makes `dots up` pass `--install-only`, so the recipe only installs a missing app; use it for apps that update themselves.
- `hold=MANAGER:NAME` records that the recipe pins that package, so `dots add` refuses it, ignoring any tap prefix or `@version` in the request.
- `dots bootstrap` and `dots exceptions` always pass `--install-only`.
- Under `dots up`, a recipe also gets `DOTS_CHANGES`, as an update hook does.
- Recipe exit statuses follow the [hook table](#hooks).

A recipe updates exactly its own app with its own package-manager command, never a whole-environment update.
A recipe can prompt, as a hook can.

### Homebrew recipes

`dots brew` subcommands each work on one named package and never run a global Homebrew update, autoremove or cleanup.

| Subcommand | Does |
| --- | --- |
| `update-cask [--auto-updates] [--admin] TOKEN [PROCESS] [--install-only]` | Installs a missing cask, or upgrades an outdated one unless `--install-only`; when a change is needed, defers with exit 3 while `PROCESS` runs or when `--admin` requires unavailable administrator access |
| `update-formula TOKEN [--install-only]` | The same for one formula, without the process and administrator checks |
| `hold-formula TOKEN VERSION [REASON]` | Succeeds when exactly that version is installed, else refuses with exit 1 |
| `hold-cask TOKEN VERSION APP_PATH [REASON]` | The same, reading the version from the app bundle at `APP_PATH` |
| `path`, `version formula\|cask TOKEN` | Print the Homebrew executable, or a package's installed version |
| `admin-available` | Exits 0 for root, cached sudo access, or terminal stdin and stdout where authorization can prompt. Otherwise exits 1 |
| `defer-for-admin TOKEN` | Says `TOKEN` needs interactive administrator access and exits 3 |
| `record formula\|cask TOKEN installed\|updated [BEFORE]` | Appends a change line when `DOTS_CHANGES` is set. An update requires two known, different versions |

`--auto-updates` opts into upgrading a self-updating cask.
`update-cask` and `update-formula` record each verified installation or upgrade themselves.
On a system other than macOS, the first four print that the package is macOS-only and exit 4, which reports the recipe as skipped.
Hold commands never install, upgrade or remove anything.

`exceptions/example-app`, updating a cask unless its app is running:

```sh
#!/bin/sh
exec dots brew update-cask --admin example-app 'Example App' "$@"
```

`exceptions/example-tool`, pinning a formula:

```sh
#!/bin/sh
exec dots brew hold-formula example 1.2.3 'example plugin needs 1.2'
```

## Retired dotfiles and retired declarations

`retired-dotfiles` lists one `~/PATH` per line; empty lines and `#` lines are ignored.
`dots retire-dotfiles` deletes each listed target only while it is a symlink into the configuration root, by its logical or physical path, whose destination no longer exists.
It ignores, with a warning, an entry that does not start with `~/` or that contains `..`, and reports each link it removes.
Call it from a mise `post-dotfiles` hook, the same way the [bootstrap hook](#connecting-mise-bootstrap) reaches `dots`:

```toml
[bootstrap.hooks]
post-dotfiles = '"$(command -v mise || echo "$HOME/.local/bin/mise")" x github:EzraCerpac/dots -- dots retire-dotfiles'
```

`retired-declarations` is for a `[dotfiles.NAME]` table that a setup script once wrote into machines' `config.local.toml` or `config.host-*.toml`, whose source this configuration has since removed.
Such a table would make every `dots sync` on those machines reject the configuration.
Rows are `dotfiles.NAME<TAB>SOURCE`, with `SOURCE` a plain relative path without backslashes, `.`, `..` or empty parts.
Other rows are ignored.
Add the row in the same commit that removes the source.

Before validating a merged configuration, sync validates it with each machine-local file's matching tables taken out.
A table goes only when it is exactly `[dotfiles.NAME]`, its `source` is exactly `SOURCE`, nothing else defines it, and `SOURCE` does not exist in the merged configuration.
The machine-local file changes only after the merged files are on disk, and only while it is still the file sync validated.
The rest of that file, its mode and the deployed file stay as they were, and sync keeps a backup and prints where.
A rendered template is a regular file and is never deleted.

## Connecting mise bootstrap

`dots bootstrap` runs mise's native bootstrap, which calls back into `dots` in two places.
Each profile file records the machine's profile before dotfiles are applied, and the base file runs the completion step last:

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

The `pre-dotfiles` hook can run before `mise` is on `PATH`, so it locates mise itself and runs the `dots` version that `[tools]` pins.
`dots bootstrap complete` then reads private inputs, moves a fresh clone to `dev`, restores settings history when asked, installs installer exceptions, runs `hooks/bootstrap.d/`, and prepares encrypted history when a state repository is set.

<!-- TODO(P-05): Replace the pinned version with the first released 1.0 version and confirm a cold install fetches it for both callbacks. -->
