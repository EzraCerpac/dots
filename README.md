# dots

`dots` keeps one or more machines in the state your [mise](https://mise.jdx.dev) configuration declares: tools, native packages, dotfiles and app settings.
It wraps mise's native bootstrap and dotfile commands with a daily update, Git-based sharing of the configuration between machines, encrypted backup of app-written settings, and hooks for whatever mise cannot do itself.

Your configuration is an ordinary mise config directory in its own Git repository, by default `~/.config/mise`.
This repository, the manager, holds no personal settings and lives apart from the configuration.

## Requirements

- mise with `mise bootstrap` and native dotfile history; the tests need 2026.9.7 or newer. No install method installs mise for you; `dots` stops with a message when it cannot find it.
- Bash, Git 2.43 or newer, Node.js, [uv](https://docs.astral.sh/uv/) and jq.
- [gitleaks](https://github.com/gitleaks/gitleaks) for `dots sync` and `dots publish`, and [age](https://age-encryption.org) for encrypted bundles and settings history.
- [JJ](https://jj-vcs.github.io/jj/) only for `dots publish`, which runs in a colocated JJ workspace of the configuration repository, not in the live checkout.

You can declare all of these as tools in the configuration itself, so `dots up` keeps them current.

## Install

With mise:

```sh
mise use -g github:EzraCerpac/dots
dots --version
```

Each release has one archive per platform (macOS and Linux, arm64 and amd64) with `dots`, the files it runs from, and the prebuilt `dots-up` renderer; mise puts both on `PATH`.
Pin a release with `github:EzraCerpac/dots@0.3.0`.

With Homebrew:

```sh
brew install ezracerpac/tap/dots
```

The formula installs the same archive and depends on Homebrew's Git, but not on mise: install mise yourself, from [mise.jdx.dev](https://mise.jdx.dev) or with `brew install mise`.

From a checkout, for development:

```sh
git clone https://github.com/EzraCerpac/dots ~/.local/share/dots
ln -s ~/.local/share/dots/bin/dots ~/.local/bin/dots
(cd ~/.local/share/dots/dots-up && go build -o ../bin/dots-up ./cmd/dots-up)   # optional renderer
```

`dots --version` prints the version and how it was installed (`mise`, `homebrew`, `checkout` or `archive`); `dots internal home` prints the directory it runs from, and `DOTS_HOME` overrides it.

The `dots-up` renderer adds the interactive menu, live progress and saved reports; without it every command still works with plain output.
`dots` uses the first renderer that reports the capability `dots-shared-cli/2` from `DOTS_UP_BIN`, its own `bin/dots-up`, `mise which dots-up` or `PATH`.

## Updating

`dots up` updates `dots` only when the configuration declares it, as a mise tool or a Homebrew formula (see below); otherwise update it the way you installed it: `mise upgrade github:EzraCerpac/dots`, `brew upgrade dots`, or `git pull` in the checkout.
`dots up` never updates a checkout.

A `dots up` that installs a new `dots` finishes on the version it started with and reports `dots X -> Y, used from the next run`.
The old version stays in place for that run: mise keeps it (`dots up` upgrades with `--no-prune`), and `dots up` sets `HOMEBREW_NO_INSTALL_CLEANUP=1`, so Homebrew keeps the old keg until your next `brew cleanup`.

## Using dots from a configuration repository

Pin the manager in the configuration, so every machine runs the same release and `dots up` installs the next one when you change the pin:

```toml
# config.toml
[tools."github:EzraCerpac/dots"]
version = "0.3.0"
```

`mise bootstrap` installs `[tools]` late, after the `pre-dotfiles` hook (see `mise bootstrap --help`), and hooks run with the caller's `PATH` only: neither `dots` nor, on a new machine, `mise` itself is on it.
Run `dots` in a hook through `mise x`, which installs the pinned version first when it is missing, and fall back to the standalone mise path that `dots remote` installs; the `bootstrap` task runs after the tools step with the tools on its `PATH` and can call `dots` directly:

```toml
# config.laptop.toml
[bootstrap.hooks]
pre-dotfiles = 'SETUP_PROFILE=laptop "$(command -v mise || echo "$HOME/.local/bin/mise")" x github:EzraCerpac/dots -- dots internal persist-selected'

# config.toml
[tasks.bootstrap]
description = "Finish the dots bootstrap"
run = 'dots internal complete'
raw = true
```

A configuration that still uses `"$DOTS_HOME/bin/dots"` works when the machine is bootstrapped with `dots bootstrap`, which exports `DOTS_HOME`, but not with `dots remote`, whose target runs `mise bootstrap` without it.

## Quickstart

Create a configuration, or add the `dots` settings to an existing one.
`dots init` never overwrites a file:

```sh
dots init            # ~/.config/mise, or: dots init DIR
```

It adds a `[vars]` table with `dots_profiles = "default"` to `config.toml`, a `dots/` directory for hooks, commands and installer exceptions, a `.gitleaks.toml` for secret scanning, and a `.gitignore` for the files each machine writes for itself.

Select this machine's profile and a name for it, once per machine:

```sh
dots internal profile --profile default --machine-id my-laptop
```

Declare a tool and a dotfile in `config.toml` with ordinary mise syntax:

```toml
[tools]
ripgrep = "latest"

[dotfiles]
"~/.config/git/config" = { source = "dotfiles/.config/git/config", mode = "symlink" }
```

Put the file at `dotfiles/.config/git/config` in the configuration, then link it and look at the result:

```sh
dots apply
dots status
```

`dots up` installs and updates everything the configuration declares.
It starts by syncing the configuration's Git repository, so first push the configuration to GitHub with a `main` and a `dev` branch, make that repository the checkout's `origin`, and name it in `[vars]` as `dots_source_repo = "OWNER/NAME"`.
Then run:

```sh
dots up
```

## Commands

Run `dots help` for the full list with examples.
In a terminal, `dots` alone opens a menu of the same commands.
`--plain`, `--verbose` and `--no-wait` before a command choose how its output is shown, for example `dots --plain status`.

Read-only:

| Command | What it does |
| --- | --- |
| `dots status` | Show the profile, source repository, declared packages, tools, dotfiles, history and installer exceptions |
| `dots plan` | Preview mise's bootstrap plan for the declared setup |
| `dots help` | List the commands, including the configuration's own |
| `dots reports [COMMAND]` | Browse saved command reports (needs the renderer) |
| `dots internal home` | Print the manager directory (`DOTS_HOME`) |
| `dots --version` | Print the manager version and how it was installed |

Everyday:

| Command | What it does |
| --- | --- |
| `dots up [--mas]` | Sync the configuration, then install and update packages, tools, installer exceptions and hooks, and update mise last; `--mas` includes Mac App Store updates; `--last` reopens the last report |
| `dots sync` | Share local configuration edits through `dev` and apply the merged result, without software updates |
| `dots apply [TARGET...]` | Apply dotfiles and render templates |
| `dots add [--base \| --profile P \| --path FILE] TOOL...` | Install tools or packages and record them in the configuration |
| `dots remove [--dry-run] [--keep-installed] TOOL...` | Uninstall software and delete its declaration (`dots uninstall` is the same) |
| `dots publish [--all \| --bookmark NAME]` | From a JJ workspace of the configuration repository, scan, push and open or update a pull request for a main-based stack |
| `dots test [SUITE...]` | Run the configuration's `tests/run` |

New machines and private settings:

| Command | What it does |
| --- | --- |
| `dots init [DIR]` | Create a minimal configuration |
| `dots bootstrap [--bundle FILE --identity KEY] [--restore ID]` | Run `mise bootstrap` for the whole declared setup, then the completion step |
| `dots remote HOST --profile P [--dry-run]` | Bootstrap a machine over existing SSH from `dots_source_repo` |
| `dots bundle create ...` | Encrypt private bootstrap inputs with age |
| `dots backup` | Save app-written settings and publish them to the private history repository |
| `dots restore [--initialize-history]` | Restore app-written settings from that repository |
| `dots atuin-bundle`, `dots atuin-login` | Prepare and finish Atuin shell-history enrollment |

### Adding software

`dots add` installs first and records only what installed:

```sh
dots add watchexec                        # current profile: config.PROFILE.toml
dots add --base watchexec                 # config.toml, every profile
dots add --profile server ripgrep         # another profile's file
dots add --path ~/other/config.toml jq    # an explicit file
dots add npm:prettier cargo:hexyl         # a backend prefix; mise still installs and updates it
dots add brew:libmagic brew-cask:firefox  # native packages
dots add https://github.com/OWNER/REPO    # a GitHub release
```

A bare name uses mise's preferred backend and the latest version.
`brew:` prefers a formula and selects a cask only when the formula is confirmed absent; `brew-cask:` always selects a cask.
New Homebrew declarations are restricted to macOS, so they never become Linux requirements.
`apt:`, `dnf:`, `pacman:`, `apk:` and `mas:` select the other native package managers.

For a GitHub URL, a prebuilt command-line release uses mise's GitHub backend.
A macOS app shipped as a ZIP or tar archive is recorded with an install hook that also replaces it in `/Applications` on upgrade, and never overwrites an app `dots` did not install.
DMG and pkg installers are refused.
When a release has several candidate downloads, name one with `--asset 'tool-*-macos-arm64.zip'` (exactly one URL).
No script from the repository runs, so a source-only repository cannot be installed.

On a failure `dots add` shows the declaration and asks whether to record it for a later retry; the default is no, and noninteractive runs never record it.
With several requests, earlier successes stay recorded and the command stops at the first failure.
A package that an installer exception holds (`hold=` in `dots/exceptions.tsv`) is refused.

### Removing software

```sh
dots remove watchexec
dots remove --dry-run brew-cask:firefox   # show the plan only
dots remove --keep-installed npm:prettier # delete only the declaration
```

`dots remove` (or `dots uninstall`) searches every active config file, including the machine-local ones; `--base`, `--profile` and `--path` narrow the search.
Name the target by its exact declaration (`brew-cask:firefox`), by a short name that matches exactly one declaration, or by a GitHub URL; `mise:jq` selects a mise tool when another manager also has `jq`.
If another active file still declares the tool, only the selected declaration goes and the software stays.

Each kind is removed by its own installer, without pruning anything else:
mise tools with `mise unuse` and then `mise prune --tools TOOL`, which keeps versions other projects use;
native packages with one targeted uninstall after checking dependents, never with zap, purge or force;
GitHub apps by moving the app to the Trash and removing their release installation.
When a removal cannot be confirmed safe, the command stops; when a step fails, it restores the declarations and any staged app.
Settings and app data stay where they are.

`dots up`, `dots sync`, `dots apply`, `dots add`, `dots remove`, `dots bootstrap`, `dots backup`, `dots restore` and `dots publish` change the machine, the configuration or a remote repository.

## Configuration

`DOTS_ROOT` selects the configuration; it defaults to `${XDG_CONFIG_HOME:-~/.config}/mise`.
Everything `dots` reads there:

| Path | Purpose |
| --- | --- |
| `config.toml` | Base mise configuration, always loaded; its `[vars]` holds the `dots` settings |
| `config.PROFILE.toml` | Additions for one profile, loaded with `mise -E PROFILE` |
| `dots/` | Hooks, commands, installer exceptions, history files and retired dotfiles: see [the `dots/` reference](init/README.md) |
| `.gitleaks.toml`, `.gitleaksignore` | Secret-scan rules for `dots sync` |
| `encrypted/bootstrap.PROFILE.json.age` | That profile's private bootstrap bundle; `dots bootstrap` uses it when present |
| `encrypted/atuin.json.age` | Atuin enrollment bundle from `dots atuin-bundle` |
| `tests/run` | The configuration's own tests, for `dots test` |
| `miserc.toml`, `config.local.toml`, `config.host-ID.toml` | Written on each machine (profile, machine name, history settings) |
| `.gitignore` | Must ignore those files and mise lockfiles; `dots sync` refuses to share them |

[The `dots/` reference](init/README.md) is also what `dots init` copies to `dots/README.md`.

### Profiles and settings

A profile is a mise environment: `config.laptop.toml` applies when the machine selects the `laptop` profile.
List the profiles in `[vars]`; `dots` refuses any other name, in hooks, `dots add --profile` and installer exceptions alike.
Each machine selects one profile with `dots internal profile`, or during bootstrap (below).

mise `[vars]` values are strings, so a list is one string of space-separated words:

```toml
[vars]
dots_profiles = "laptop server"              # required
dots_source_repo = "OWNER/dotfiles"          # sync, publish, dots remote
dots_state_repo = "OWNER/dotfiles-state"     # backup, restore, bootstrap
dots_self_updating_casks = "some-cask"       # casks dots up only installs when missing
dots_publish_on_behalf_of = "Your Name"      # dots publish adds a note that a model (DOTS_PUBLISH_MODEL) wrote the PR text
dots_atuin_username = "you"                  # the only Atuin account enrollment accepts; unset, it keeps the
                                             # account already logged in, else logs in as the one the bundle names
```

### Bootstrap

`dots bootstrap` runs `mise bootstrap` with `DOTS_HOME` and `DOTS_ROOT` exported.
Two pieces of mise configuration connect it to `dots`: a hook in each profile file that records the profile, and a `bootstrap` task, which mise runs as the last bootstrap step, for the completion step (private inputs, installer exceptions, `dots/hooks/bootstrap.d`, Atuin, history enrollment and Tailscale).
[Using dots from a configuration repository](#using-dots-from-a-configuration-repository) shows both.

## Sync, dev and main

The configuration repository has two shared branches.
`dev` is what every machine runs: `dots sync`, and `dots up` before its updates, commit local edits to public files onto `dev`, scan them with gitleaks, merge the other machines' work and reviewed `main`, push without force and apply the result.
`main` changes only through reviewed pull requests, so a merged PR reaches each machine with its next `dots up`.
Machine-local files and secrets never enter the repository.
[The sync guide](docs/sync.md) explains a run, every line of its output and what to do about it, and the requirements, including Git 2.43 or newer.

## Agent skill

[`skills/dots/SKILL.md`](skills/dots/SKILL.md) teaches a coding agent to work in a `dots` configuration: which commands are read-only, how to add tools, hooks and commands, and which changes need a pull request.

`dots` links it for the agents you use, every time it runs, so installing or updating `dots` by any method also installs or updates the skill:

| Link | Read by | Made when |
| --- | --- | --- |
| `~/.claude/skills/dots` | Claude Code | `~/.claude` (or `CLAUDE_CONFIG_DIR`) exists |
| `~/.agents/skills/dots` | Codex and other clients of the shared directory | `~/.agents` or `~/.codex` (or `CODEX_HOME`) exists |

Each link points at `skills/dots` in the running installation: Homebrew's stable `opt` path, so a `brew upgrade` needs no relinking, or the version directory for mise and a checkout, which the next `dots` command or `dots up` moves to the new version.
`dots` replaces only links it made and that still point where it left them (it records them in `~/.local/state/dots/skill-links`); a directory, a file or any other link at those paths is yours, and `dots` leaves it alone with a one-time notice.
`dots status` shows each link.
Set `DOTS_SKILL_LINKS=0` to turn linking off.
To remove the skill, set that and delete the two links; after `brew uninstall dots` or removing a mise version, the links dangle until you delete them or install `dots` again.

## Development

| Path | Contents |
| --- | --- |
| `bin/dots` | The command-line entry point |
| `packaging/` | The release build and the Homebrew formula |
| `docs/` | [The sync guide](docs/sync.md) |
| `lib/`, `tasks/`, `bootstrap/` | The commands' implementation; [`tasks/README.md`](tasks/README.md) details `dots up` and `dots restore` |
| `init/` | Templates for `dots init` |
| `dots-up/` | The Go renderer for menus, progress and reports |
| `skills/` | The agent skill |
| `tests/` | The test suites and a fixture configuration |

Run the tests from the repository root; they need nothing outside it and never touch your machine:

```sh
tests/run              # Python, Node and shell suites; tests/run python, node or shell
(cd dots-up && go vet ./... && go test ./...)
```

A suite whose runtime is missing is skipped (exit status 77), and so is a test whose tool is missing.
The run ignores your Git and JJ configuration.
`tests/fixtures/config` is a small configuration with every part of the contract.

To try a change without installing it, point `DOTS_HOME` at your checkout; every child of that run uses the same manager:

```sh
DOTS_HOME=~/src/dots ~/src/dots/bin/dots status
DOTS_ROOT=/tmp/other-config dots help        # and another configuration
```

Pushing a `vX.Y.Z` tag runs the release workflow: it runs the tests and Go checks on Linux and macOS, then [`packaging/build-release`](packaging/build-release) builds `dots-X.Y.Z-{darwin,linux}-{arm64,amd64}.tar.gz`, `SHA256SUMS` and `dots.rb`, and publishes them as a GitHub release.
The tag is the only place the version is written; the archives carry it in a `VERSION` file.
`dots.rb` is [`packaging/homebrew/dots.rb`](packaging/homebrew/dots.rb) with the release's checksums. Nothing updates the tap automatically: copy the release's `dots.rb` by hand to `Formula/dots.rb` in `EzraCerpac/homebrew-tap` to publish the release to Homebrew.

## License

MIT, see [LICENSE](LICENSE).
