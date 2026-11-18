# dots

`dots` keeps your tools, native packages and dotfiles in one ordinary [mise](https://mise.jdx.dev) configuration, and `dots up` brings a machine up to date with it in one command.
An optional Git remote shares that configuration between your machines, and `dots sync` scans your changes for secrets before it pushes them.

## Requirements

macOS, Linux, or Linux under WSL.
`dots` is one self-contained binary.
To start you need only mise 2026.11.8 or newer and Git; install mise first, since `dots` does not install it.

Other programs are needed only by the features that use them:

| Program | Needed for |
| --- | --- |
| Git 2.43 or newer | `dots sync` and the sync step of `dots up` |
| [gitleaks](https://github.com/gitleaks/gitleaks) | Sharing changes, `dots publish`, and showing patches in `dots sync --last` |
| [age](https://age-encryption.org), age-keygen | Encrypted bootstrap bundles and encrypted settings history |
| `gh` | The optional remote offer in `dots init`; `dots publish` |
| [JJ](https://jj-vcs.github.io/jj/) | `dots publish` |
| ssh | `dots remote` |
| Homebrew, apt, dnf, pacman, apk, mas | Declarations that select that package manager |

`dots status` lists each program it finds, and which commands need the missing ones.

## Install

> **TODO (D-01, C-01, G-2):** No 1.0 release, tap formula or crate exists yet.
> Today the tap installs 0.4.0, and `dots-cli` is not on crates.io.
> Verify each command below against the published 1.0 artifacts, then delete this note.

```sh
mise use -g github:EzraCerpac/dots@1.0.0   # with mise
brew install ezracerpac/tap/dots           # with Homebrew
cargo install dots-cli --locked            # with Cargo
```

`dots --version` prints the version and how it was installed.

> **TODO (A-03):** Demo.
> Record the real 1.0 binary running `dots init`, `dots add` and the `dots up` dashboard on a scratch machine, and put the recording here.
> No placeholder image or link until it exists.

## Quickstart

```sh
dots init
dots up
```

> **TODO (I-01):** Run these two commands with the 1.0 binary in a pristine fixture (empty `HOME`, mise and Git only, no configuration) and paste their actual output here.

`dots init` sets up the configuration:

- It uses mise's configuration directory, normally `~/.config/mise`; `dots init DIR` picks another.
- It adds what `dots` needs next to any files already there and never overwrites one. Running it again changes nothing.
- It selects the `default` profile and a machine ID derived from the hostname, and keeps both in machine-local files that are never shared.
- In a new repository it makes one seed commit on branches `main` and `dev`, with `dev` checked out. An existing repository keeps its branches and history; `init` reports anything sync would stop on, with its fix.
- In a terminal with `gh` signed in, it offers to create a private GitHub repository; the default is no. Otherwise it prints the commands that connect a remote. Use those, rather than your own push: they publish only the verified seed.

`dots up` then installs and updates everything the configuration declares.
Without a remote it skips sync with one line; updating tools and mise itself still uses the network.

## Day to day

```sh
dots status                                  # what is declared, installed and missing
dots plan                                    # mise's bootstrap plan; installs nothing
dots up                                      # sync, then install and update everything
dots up --last                               # reopen the last update report
dots sync                                    # share your edits and take the other machines'
dots sync --last                             # replay the last sync receipt

dots add --base ripgrep                      # install, then declare it for every profile
dots add ~/.config/example/settings.toml     # adopt a file from your home directory
dots remove --dry-run ripgrep                # show what removal would do
dots remove --base --keep-installed ripgrep  # drop only the declaration
```

`dots add` writes to the selected profile's file unless you name another: `--base` for `config.toml`, `--profile P` for `config.P.toml`, or `--path FILE`.
An argument starting with `/`, `~/`, `./` or `../` is a file; anything else is software.
Adopting an existing regular file under your home directory moves it into the configuration's `dotfiles/` and leaves a link in its place; `dots remove` with the same path puts back a regular file with its current contents and mode.

`dots root` prints the configuration directory, `dots profile` the selected profile, and `dots help` every command, including your configuration's own.

## Sharing between machines

Once the configuration has an `origin`, `dots sync`, and `dots up` before it updates anything, share it through two branches: `dev`, which every machine runs, and `main`, which you change through pull requests.

- A sync with valid arguments always exits 0. Its receipt says what was shared, what stayed local and what needs attention.
- Sync scans this machine's new and outgoing commits with gitleaks. A flagged file stays on disk, unshared; without gitleaks nothing is pushed.
- Machine-local files, such as the profile selection and lockfiles, stay out of the repository.
- On the remote, sync only creates or fast-forwards `dev`, and never moves `main`.
- Where edits conflict, `main` wins over the shared `dev`, and `dev` over this machine's work; both sides stay in history.
- Without a remote, sync does nothing and says how to add one.

[The sync guide](docs/sync.md) explains a run and its output; [the contract](docs/contract.md#dots-sync) is the precise rule.

## Optional, later

- `dots publish [--all | --bookmark NAME]` opens or updates a pull request to `main`, from a separate colocated JJ workspace of a configuration hosted on GitHub.
- `dots backup` publishes this machine's encrypted app settings to a separate private repository; `dots restore` fetches them back after safety checks.
- `dots remote HOST --profile P` and `dots bootstrap` set up a new machine from the configuration; see [`dots bootstrap`](docs/contract.md#dots-bootstrap).

## Why dots

- **Plain mise** already bootstraps a machine and manages tools, packages, dotfiles and their history. If that covers you, keep it. `dots` adds one command that updates all of it, and a fixed, scanned Git policy for sharing the configuration between machines.
- **chezmoi or GNU Stow** suit you if dotfiles are what you manage, and tools and packages are handled elsewhere. `dots` keeps all three in one mise configuration.
- **Nix** suits you if you want fully reproducible environments and are happy to learn its language. `dots` stays with mise's TOML and your system's own package managers.

## Development

> **TODO (R-01, G-2):** The Rust crate has not landed on this branch yet.
> Verify these commands once `Cargo.toml`, `Cargo.lock` and `mise.toml` exist, then delete this note.

This repository is the Rust crate `dots-cli`, which builds the `dots` binary; `mise.toml` pins Rust 1.100.0.

```sh
mise install
cargo build --locked
cargo test --locked
```

`dots test` is something else: it runs your configuration's own `tests/run`.

[The 1.0 contract](docs/contract.md) is the reference for everything `dots` does.

## License

MIT, see [LICENSE](LICENSE).
