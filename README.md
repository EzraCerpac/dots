# dots

`dots` keeps your tools, native packages and dotfiles in one ordinary [mise](https://mise.jdx.dev) configuration, and `dots up` brings a machine up to date with it in one command.
An optional Git remote shares that configuration between your machines, and `dots sync` scans your changes for secrets before it pushes them.

> **TODO (A-03):** Record the 1.0 binary running `dots init`, `dots add` and the `dots up` dashboard.

## Install

For the local quickstart on macOS, Linux or WSL, install mise 2026.11.8 or newer and Git.
Choose one way to install the self-contained `dots` binary.

> **TODO (D-01, C-01, G-2):** Verify these alternatives against the published 1.0 artifacts. The current tap installs 0.4.0.

**With mise**, activate mise in your shell or add its shims to your `PATH`.

```sh
mise use -g github:EzraCerpac/dots@1.0.0
```

**With Homebrew**

```sh
brew install ezracerpac/tap/dots
```

**With Cargo**, install the Rust toolchain and a system linker first.

```sh
cargo install dots-cli --locked
```

Run `dots --version` and check that it reports 1.0.0 before starting the quickstart.
It also names the detected install method.

## Requirements

Beyond the local quickstart, the programs you need depend on what you run.

| Program | Needed for |
| --- | --- |
| Git | `init`, `backup`, `restore`, `bootstrap` and `publish`. Sync and the sync step of `up` need Git 2.43 or newer. |
| [gitleaks](https://github.com/gitleaks/gitleaks) | Sharing changes, `dots publish`, and showing patches in `dots sync --last` |
| [age](https://age-encryption.org), age-keygen | Encrypted bootstrap bundles and encrypted settings history |
| `gh` | The optional remote offer in `dots init`; `dots publish` |
| [JJ](https://jj-vcs.github.io/jj/) | `dots publish` |
| ssh | `dots remote` |
| Homebrew, apt, dnf, pacman, apk, mas | Declarations that select that package manager. The `dots brew` helpers also use Homebrew on macOS. |

`dots status` lists each program it finds, and which commands need the missing ones.

## Quickstart

```sh
dots init
dots up
```

> **TODO (I-01):** Run these two commands with the 1.0 binary in a pristine fixture (empty `HOME`, mise and Git only, no configuration) and paste their actual output here.

`dots init` sets up the configuration:

- It uses mise's configuration directory, normally `~/.config/mise`. For a separate directory, use `dots init "$HOME/dotfiles"`, then `DOTS_ROOT="$HOME/dotfiles" dots up`.
- It creates missing files and adds `dots_profiles = "default"` under `[vars]` if the existing `config.toml` lacks that setting. It preserves the rest of the file.
- It keeps a valid saved profile and machine ID. For a fresh setup, it selects `default` if listed (otherwise the first listed profile) and derives the machine ID from the hostname. Both selections stay on this machine.
- In a new repository it makes one seed commit on branches `main` and `dev`, with `dev` checked out. An existing repository keeps its branches and history; `init` reports anything sync would stop on, with its fix.
- In the run that creates the seed, a terminal with `gh` signed in gets an offer to create a private GitHub repository. The default is no. For a pristine seed, the printed push uses the verified commit ID. For any other repository without an `origin`, follow the printed remote-add and `dots sync` instructions so outgoing commits are scanned.

`dots up` checks for tracked settings to checkpoint, then syncs and updates eligible software.
In local-only mode (no `origin` and no `dots_source_repo`), it skips sync. Updating tools or standalone mise can still use the network.

## Day to day

```sh
dots status                                  # what is declared, installed and missing
dots plan                                    # mise's bootstrap plan; installs nothing
dots up                                      # checkpoint, sync and update eligible software
dots apply                                   # apply declared dotfiles and templates locally
dots up --last                               # reopen the last update report
dots sync                                    # share your edits and take the other machines'
dots sync --last                             # replay the last sync receipt

dots add --base ripgrep                      # install, then record it in the base config
dots add ~/.config/example/settings.toml     # adopt an existing file that is safe to share
dots remove --dry-run ripgrep                # show what removal would do
dots remove --base --keep-installed ripgrep  # drop only the declaration
```

`dots add` writes to the selected profile's file unless you name another: `--base` for `config.toml`, `--profile P` for `config.P.toml`, or `--path FILE`.
For file adoption or removal, a selected file must resolve to the root’s base config or one of its listed profiles’ config files.
An argument starting with `/`, `~/`, `./` or `../` is a file; anything else is software.
Adopting an existing regular file under your home directory moves it into the configuration's `dotfiles/` and leaves a link in its place; `dots remove` with the same path puts back a regular file with its current contents and mode.

`dots root` prints the configuration directory, `dots profile` the selected profile, and `dots help` every command, including your configuration's own.

## Sharing between machines

When sharing is configured, `dots sync` and the sync stage of `dots up` use two branches: `dev`, which every machine runs, and `main`, which you change through pull requests.

- A sync with valid arguments always exits 0. Its receipt says what was shared, what stayed local and what needs attention.
- Sync scans this machine's new and outgoing commits with gitleaks. A flagged file stays on disk, unshared; without gitleaks nothing is pushed.
- Machine-local files, such as the profile selection and lockfiles, stay out of the repository.
- On the remote, sync only creates or fast-forwards `dev`, and never moves `main`.
- Where edits conflict, `main` wins over the shared `dev`, and `dev` over this machine's work; both sides stay in history.
- In local-only mode, sync skips the entire pipeline and says how to add a remote.

See [the 1.0 sync rules and receipt](docs/contract.md#dots-sync) for the order of operations and how to read the outcome.

## Optional, later

- `dots publish [--all | --bookmark NAME]` opens or updates a pull request to `main`, from a separate colocated JJ workspace of a configuration hosted on GitHub.
- `dots backup` publishes this machine's encrypted app settings to a separate private repository; `dots restore` fetches them back after safety checks.
- `dots remote HOST --profile P` and `dots bootstrap` set up a new machine from the configuration; see [`dots bootstrap`](docs/contract.md#dots-bootstrap).

## Why dots

- **Plain mise** already bootstraps a machine and manages tools, packages, dotfiles and their history. If that covers you, keep it. `dots` adds one command that updates all of it, and a fixed, scanned Git policy for sharing the configuration between machines.
- **chezmoi or GNU Stow** suit you if dotfiles are what you manage, and tools and packages are handled elsewhere. `dots` keeps all three in one mise configuration.
- **Nix** suits you if you want fully reproducible environments and are happy to learn its language. `dots` stays with mise's TOML and your system's own package managers.

## Development

This repository is the Rust crate `dots-cli`, which builds the `dots` binary; `mise.toml` pins Rust 1.100.0.

```sh
mise install
mise run build
mise run test
```

`dots test` is something else: it runs your configuration's own `tests/run`.

[The 1.0 contract](docs/contract.md) is the reference for everything `dots` does.

## License

MIT, see [LICENSE](LICENSE).
