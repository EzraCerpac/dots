---
name: dots
description: Work in a configuration managed by the dots dotfiles manager (a mise config directory with a dots/ directory). Use when asked to add or remove a tool, package or dotfile, write a dots hook, command or installer exception, inspect or update a machine with dots, or change files in a mise config repository that has a dots/ directory.
---

# dots

`dots` manages machines from a mise configuration repository.
Change that configuration, not `dots` itself, unless the task is about `dots`.
Find the configuration and this machine's selection before changing anything:

```sh
dots root            # the configuration root
dots profile         # this machine's profile
dots help            # every command, including the configuration's own
dots add --help      # usage for the built-in add command
```

`$(dots root)/dots/README.md` documents hooks, commands, installer exceptions and retired dotfiles for this configuration.
If it describes sourcing shell libraries from a manager directory, it predates 1.0; use `dots help` for the current command list.
`dots test` and `dots mise` pass `--help` to the child, as do configuration commands.

> TODO(R-01): Confirm this skill reads correctly once extracted on its own, with nothing else from the dots repository beside it.

## Safe and unsafe commands

These commands inspect the configuration or preview an action. Some save a private report, and invocation can refresh agent skill links.

```sh
dots status                     # programs, profile, source, packages, tools, dotfiles, history, skill links
dots plan                       # mise's bootstrap plan for this machine
dots sync --last                # the latest sync receipt, without network or mise
dots up --last                  # the last dots up report
dots remove --dry-run TARGET    # what removing TARGET would do
```

These change the machine, the configuration or a remote repository.
Run them only when the user asks for that action, and otherwise say which command the user should run:
`dots up`, `dots sync`, `dots apply`, `dots add`, `dots remove`, `dots init`, `dots bootstrap`, `dots remote`, `dots backup`, `dots restore`, `dots publish`, `dots profile --profile PROFILE --machine-id ID`, `dots exceptions`, `dots retire-dotfiles`, `dots enroll-history`.
`dots mise ARGS...` and configuration commands do whatever they run.

In shared mode, `dots sync` and the sync stage of `dots up` can publish eligible edits to `dev` once scanning, validation, checkout and remote checks pass.
Do not run them while someone else has uncommitted edits there, and do not leave half-finished edits there yourself.

## Profiles

`[vars] dots_profiles` in `config.toml` lists the profiles, for example `"laptop server"`.
`config.toml` applies everywhere; `config.PROFILE.toml` adds to one profile.
Put a declaration in the narrowest file that fits: shared tools in `config.toml`, machine-type tools in the profile file.
`miserc.toml`, `config.local.toml` and `config.host-*.toml` are machine-local and ignored; never write `[tools]`, `[dotfiles]` or packages into them.

## Add or remove software

Prefer `dots add`, which installs first and records only what worked:

```sh
dots add ripgrep                       # the selected profile's config.PROFILE.toml
dots add --base ripgrep                # config.toml, every profile
dots add --profile server ripgrep      # another profile's file
dots add npm:prettier cargo:hexyl
dots add brew-cask:firefox             # macOS only
dots add https://github.com/OWNER/REPO
dots remove --dry-run ripgrep
dots remove ripgrep
dots remove --keep-installed ripgrep   # drop only the declaration
```

At most one of `--base`, `--profile P` and `--path FILE` selects the file.
When the user only wants the declaration written, edit the TOML by hand with mise syntax (`[tools]`, `[bootstrap.packages]`) and leave installation to their next `dots up`.

## Adopt or restore a dotfile

An argument starting with `/`, `~/`, `./` or `../` is a file; anything else is software.
A bare `.zshrc` is a software name, so from `HOME` adopt it as `./.zshrc`.

```sh
dots add ~/.config/app/config.toml       # move into dotfiles/ and link it back
dots add --base ~/.gitconfig             # declare it for every profile
dots remove ~/.config/app/config.toml    # put the file back and drop the declaration
```

`dots add FILE` adopts a regular file inside `HOME`, outside the configuration: it copies it to `dotfiles/REL`, declares `"~/REL" = { source = "dotfiles/REL", mode = "symlink" }` in the selected file, and replaces the original with a link.
It refuses a file that is already declared, a source path in use, and a selected file other than `config.toml` or a listed `config.PROFILE.toml`.
`dots remove FILE` replaces the link with a regular file holding the source's current bytes and mode, so edits made through the link stay, then removes the declaration and the source.

For per-machine values or restricted targets, declare by hand:

```toml
[dotfiles]
"~/.config/app/config.toml" = { source = "dotfiles/.config/app/config.toml.tera", mode = "template", variants = [{ os = "macos" }] }
```

When you remove a declaration, add its target to `dots/retired-dotfiles` so stale links get cleaned up.
When you remove a source that a setup script once declared in machines' `config.local.toml` or `config.host-*.toml`, add `dotfiles.NAME<TAB>SOURCE` to `dots/retired-declarations` in the same commit, or every such machine's sync rejects it.

## Private settings

App-written files that must not be shared in plain text go in encrypted settings history, one stream per machine:

```toml
[dotfiles]
"~/.config/app/state.json" = { mode = "track", encrypt = true, variants = [{ profile = "host-my-laptop" }] }
```

- Every tracked entry is `encrypt = true`; `dots` refuses to save one that is not.
- List each machine that keeps the file as its own `{ profile = "host-my-laptop" }` variant.
  Never rename or remove an existing variant: it names that machine's stream in history.
- `dots restore` fetches private history and uses mise to apply it to the actual tracked files.
  Mise can checkpoint live content locally for undo, encrypted to the configured recipients.
  If both sides changed the same lines, restore exits 1 and keeps the live edit.
  Only an explicit `dots backup` publishes those checkpoints as part of the private history.
- Bootstrap secrets come from an age-encrypted bundle and reach bootstrap hooks only through the file descriptor in `DOTS_BUNDLE_FD`.
  Never print a value, write it to a shared file, or pass it as an argument or environment variable.

## Hooks, commands and installer exceptions

All live in the configuration's `dots/`; read `$(dots root)/dots/README.md` before writing one.

- Update hook: executable `dots/hooks/update.d/NN-Name`, run by `dots up` after packages, tools and installer exceptions.
  Bootstrap hook: `dots/hooks/bootstrap.d/NN-Name`.
  A `@PROFILE` suffix (`30-Backups@server`) limits it to one profile.
- Command: executable `dots/commands/NAME`, run as `dots NAME ARGS...` in the caller's directory, before mise loads; built-in names win.
- Installer exception, for an app no mise backend installs: an executable recipe `dots/exceptions/NAME` that updates exactly that app, plus a row `PROFILE<TAB>NAME[<TAB>FLAGS]` in `dots/exceptions.tsv`.
  Homebrew recipes can call `dots brew update-cask`, `update-formula`, `hold-formula` and `hold-cask`.
- Exit 0 completed, 3 deferred, 4 skipped, anything else failed.
- Children get `DOTS_ROOT` and `DOTS_PROFILE`, and the running `dots` first on `PATH`.
  Update hooks and recipes under `dots up` may append `ITEM<TAB>BEFORE<TAB>AFTER` lines to `$DOTS_CHANGES`.

Write hooks and recipes in any language; use `dots root`, `dots profile` and `dots mise` instead of sourcing shell code.
Make them executable (`chmod +x`), keep them safe to rerun, and check their syntax with their own interpreter.

## Sharing changes

- Small changes (a config line, a hook, instruction text) go into the live checkout; the user then runs `dots up` or `dots sync` to share them through `dev`.
  Sync scans every edit with gitleaks first, keeps flagged files on disk, and refuses to push a local commit that holds a secret or a machine-local file.
- Larger or risky changes go on a task branch or JJ workspace from `origin/main`, with a pull request into `main`, never from the live checkout.
  `dots publish` can open that pull request from a colocated JJ workspace when the repository is on GitHub.
  A merged pull request reaches every machine on its next `dots up`.
- Never force-push, rewrite `dev` or `main`, or merge `dev` wholesale into `main`.
- Never commit secrets, keys, tokens, app state or machine-local files.
- If the configuration has an executable `tests/run`, run it with `dots test`.

## When something fails

- Start from what `dots up` or `dots sync` printed: each issue names its next action.
- `dots up --last` prints the saved update report and returns its recorded exit status.
  Captured stages have private logs under `${XDG_STATE_HOME:-~/.local/state}/dots/`. A failed step that had the terminal gives its exit status and next action, with its output in scrollback.
- `dots sync --last` replays the last sync receipt.
  The [sync guide](https://github.com/EzraCerpac/dots/blob/main/docs/sync.md) explains each kind of sync issue.
- `dots reports [COMMAND]` lists completed reports, newest first, with the command, time, outcome, exit status and location.
- Run `dots` for help, including the configuration's commands.

## This skill

`dots` refreshes this skill on every run except `dots completions` and shell completion, and links it for Claude Code and for clients of `~/.agents/skills`.
Do not edit the extracted copy; the next run replaces it.
`dots` replaces only links it made, leaves a user's own file or directory at those paths alone, and makes none when `DOTS_SKILL_LINKS=0`.
