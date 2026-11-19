---
name: dots
description: Work in a configuration managed by the dots dotfiles manager, meaning a mise config directory that contains a dots/ directory. Use when asked to add or remove a tool, package or dotfile, write a dots hook, command or installer exception, inspect or update a machine with dots, or change files in a mise config repository that has a dots/ directory.
---

# dots

`dots` manages machines from a mise configuration repository.
Unless the task concerns `dots` itself, the work belongs in that configuration.
Before changing anything, locate the configuration and see what this machine selects:

```sh
dots root            # the configuration root
dots profile         # this machine's profile
dots help            # every command, including the configuration's own
dots add --help      # how the built-in add command is used
```

For this configuration's hooks, commands, installer exceptions and retired dotfiles, read `$(dots root)/dots/README.md`.
A README that has hooks source shell libraries from a manager directory was written for v0.4.0 or earlier, so trust `dots help` for the commands that exist now.
`--help` after `dots test`, `dots mise` or a configuration command goes to the program being run, not to `dots`.

## Safe and unsafe commands

The commands below only look at the configuration or preview a change.
A few keep a private report, and any `dots` invocation may refresh agent skill links.

```sh
dots status                     # programs, profile, source, packages, tools, dotfiles, history, skill links
dots mise bootstrap plan        # mise's bootstrap plan for this machine
dots sync --last                # the latest sync receipt, without network or mise
dots up --last                  # the last dots up report
dots remove --dry-run TARGET    # what removing TARGET would do
```

The next group changes the machine, the configuration or a remote repository.
Run one only when the user has asked for that action; otherwise, tell the user which command to run:
`dots up`, `dots sync`, `dots add`, `dots remove`, `dots init`, `dots bootstrap`, `dots remote`, `dots backup`, `dots restore`, `dots profile --profile PROFILE --machine-id ID`, `dots exceptions`, `dots retire-dotfiles`, `dots enroll-history`.
`dots mise ARGS...` and configuration commands do whatever they run; `dots mise bootstrap dotfiles apply TARGET`, for example, writes that target.

When the configuration is shared, `dots sync`, and the sync stage of `dots up`, push eligible edits to `dev` after the secret scan, validation and remote checks all pass.
Incoming changes to the checkout, and dotfile application, wait until the remote has confirmed `dev`.
Do not run them while someone else has uncommitted edits there, and do not leave half-finished edits there yourself.

## Profiles

`[vars] dots_profiles` in `config.toml` lists the profiles, for example `"laptop server"`.
`config.toml` applies everywhere; `config.PROFILE.toml` adds to one profile.
Put a declaration in the narrowest file that fits: shared tools in `config.toml`, machine-type tools in the profile file.
`miserc.toml`, `config.local.toml` and `config.host-*.toml` stay on their machine and out of Git, so never put `[tools]`, `[dotfiles]` or packages there.

## Add or remove software

`dots add` installs software and records it.
When installation fails in a terminal, it can still record the declaration, with your confirmation, so a later run can retry.

```sh
dots add ripgrep                       # the selected profile's config.PROFILE.toml
dots add --base ripgrep                # config.toml, every profile
dots add --profile server ripgrep      # another profile's file
dots add npm:prettier cargo:hexyl      # other mise backends
dots add brew-cask:firefox             # a Homebrew cask, macOS only
dots add https://github.com/OWNER/REPO
dots remove --dry-run ripgrep
dots remove ripgrep
dots remove --keep-installed ripgrep   # drop the declaration, keep the install
```

`--base`, `--profile P` and `--path FILE` each choose the file, and only one may be given.
If the user wants the declaration without the install, write the TOML by hand in mise syntax (`[tools]`, `[bootstrap.packages]`) and let their next `dots up` install it.

## Adopt or restore a dotfile

Arguments that begin with `/`, `~/`, `./` or `../` are files, and every other argument is software.
So `.zshrc` alone names software; to adopt the file from `HOME`, write `./.zshrc`.

```sh
dots add ~/.config/app/config.toml       # move it into dotfiles/ and link it back
dots add --base ~/.gitconfig             # declare it for every profile
dots remove ~/.config/app/config.toml    # restore the file and drop the declaration
```

`dots add FILE` takes a regular file that lives inside `HOME` but outside the configuration.
It copies the file to `dotfiles/REL`, declares `"~/REL" = { source = "dotfiles/REL", mode = "symlink" }` in the selected file, and puts a link where the original was.
Running the same adoption again succeeds.
A new adoption needs Git, and it refuses when a declaration conflicts, when the source path is taken, or when the selected file is neither `config.toml` nor a listed `config.PROFILE.toml`.
`dots remove FILE` turns the link back into a regular file with the source's current bytes and mode, so edits made through the link survive, and then deletes the declaration and the source.

When a file needs per-machine values or should only reach some machines, write its declaration by hand:

```toml
[dotfiles]
"~/.config/app/config.toml" = { source = "dotfiles/.config/app/config.toml.tera", mode = "template", variants = [{ os = "macos" }] }
```

`dots up` applies declared dotfiles whether or not the configuration is shared.
To apply a single target explicitly, run `dots mise bootstrap dotfiles apply TARGET`.

When you remove a declaration, add its target to `dots/retired-dotfiles` so stale links get cleaned up.
When you remove a source that a setup script once declared in machines' `config.local.toml` or `config.host-*.toml`, add `dotfiles.NAME<TAB>SOURCE` to `dots/retired-declarations` in the same commit, or every such machine's sync rejects it.

## Private settings

Files an app writes that must not be shared as plain text belong in encrypted settings history, which keeps one stream per machine:

```toml
[dotfiles]
"~/.config/app/state.json" = { mode = "track", encrypt = true, variants = [{ profile = "host-my-laptop" }] }
```

- Give every tracked entry `encrypt = true`.
  `dots remote` leaves the SSH target's own mise history settings in charge there, and with history on, that target stores an unencrypted tracked file's payload locally as plain text.
  Encrypt the declaration, or turn history off on the target, to keep plain text off its disk.
- Before `dots backup` hands over to mise's native sync, it checks the tracked payloads in every checkpoint it cannot prove the destination host branch already has.
  Behind an opaque SSH launcher, that means the whole local ancestry.
  Encrypting a declaration later does not encrypt what an older checkpoint already holds.
  This check honours mise's `allow_plaintext_history` opt-in.
- Give each machine that keeps the file its own `{ profile = "host-my-laptop" }` variant.
  Never rename or remove an existing variant: it names that machine's stream in history.
- `dots restore` fetches the private history and has mise apply it to the real tracked files.
  Before a fetch, or anything else that could capture live content, restore needs plaintext history turned off and a nonempty recipient list in system or global configuration that native mise trusts.
  mise picks the recipients of every new age-encrypted undo checkpoint, and the identity in use must be able to decrypt it.
  Incoming checkpoints may name other recipients, which lets restore work with a replacement identity once the old private key is gone.
  When both sides changed the same lines, restore keeps the live edit and exits 1.
  Those checkpoints join the private history only through an explicit `dots backup`.
- Bootstrap secrets come from an age-encrypted bundle and reach bootstrap hooks only through the file descriptor in `DOTS_BUNDLE_FD`.
  Never print a value, write it to a shared file, or pass it as an argument or environment variable.

## Hooks, commands and installer exceptions

Each of these lives in the configuration's `dots/` directory, and `$(dots root)/dots/README.md` describes them; read it before you write one.

- Update hook: executable `dots/hooks/update.d/NN-Name`, run by `dots up` after packages, tools and installer exceptions.
  Bootstrap hook: `dots/hooks/bootstrap.d/NN-Name`.
  A `@PROFILE` suffix (`30-Backups@server`) limits it to one profile.
- Command: executable `dots/commands/NAME`, run as `dots NAME ARGS...` in the caller's directory, before mise loads; built-in names win.
- Installer exception, for an app that no mise backend can install: an executable recipe `dots/exceptions/NAME` that updates that one app, and a `PROFILE<TAB>NAME[<TAB>FLAGS]` row in `dots/exceptions.tsv`.
  Homebrew recipes can call `dots brew update-cask`, `update-formula`, `hold-formula` and `hold-cask`.
- Exit codes: 0 completed, 3 deferred, 4 skipped, and any other value failed.
- Every child receives `DOTS_ROOT` and `DOTS_PROFILE`, with the running `dots` at the front of `PATH`.
  Under `dots up`, update hooks and recipes can append `ITEM<TAB>BEFORE<TAB>AFTER` lines to `$DOTS_CHANGES`.

Any language works for hooks and recipes; call `dots root`, `dots profile` and `dots mise` rather than sourcing shell code.
Mark them executable (`chmod +x`), make sure a second run is harmless, and check their syntax with the interpreter they use.

## Sharing changes

- Small changes (a config line, a hook, instruction text) go into the live checkout; the user then runs `dots up` or `dots sync` to share them through `dev`.
  Sync runs gitleaks over every edit first, leaves flagged files on disk, and will not push a local commit that contains a secret or a machine-local file.
- Larger or risky changes belong on an ordinary task branch made from `origin/main`, reviewed through a pull request into `main` and never made in the live checkout.
  Once merged, the change reaches every machine at its next `dots up`.
- Never force-push, rewrite `dev` or `main`, or merge `dev` wholesale into `main`.
- Never commit secrets, keys, tokens, app state or machine-local files.
- If the configuration has an executable `tests/run`, run it with `dots test`.

## When something fails

- Begin with what `dots up` or `dots sync` printed, since every issue names the next action.
- `dots up --last` shows the saved update report and exits with the status that report recorded.
  Stages whose output was captured keep private logs under `${XDG_STATE_HOME:-~/.local/state}/dots/`.
  A failed step that had the terminal reports its exit status and next action, and its output is in the scrollback.
- `dots sync --last` shows the most recent sync receipt again.
  If it calls a push unconfirmed, check `refs/heads/dev` at `origin`'s push URL against the candidate and expected commits it lists, and only then sync again.
  Every kind of sync issue is covered in the [sync guide](https://github.com/EzraCerpac/dots/blob/main/docs/sync.md).
- `dots reports [COMMAND]` lists finished reports, newest first, giving each one's command, time, outcome, exit status and location.
- Plain `dots` prints help, the configuration's own commands included.

## This skill

Every `dots` run refreshes this skill, except `dots completions` and shell completion, and links it for Claude Code and for clients that read `~/.agents/skills`.
Leave the extracted copy alone, because the next run overwrites it.
`dots` replaces only links it made itself, never a user's own file or directory at those paths, and makes no links when `DOTS_SKILL_LINKS=0`.
