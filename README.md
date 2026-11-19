# dots

**One command keeps your machine current.**
`dots up` installs what your configuration declares, updates it, and takes in what your other machines shared.

- **One ordinary [mise](https://mise.jdx.dev) configuration** holds the tools, packages and dotfiles of every Mac and Linux machine you use, with a profile for each kind of machine.
  There is no new language to learn, and if you stop using `dots`, your configuration is still plain mise TOML.
- **Add software from many sources** with `dots add`: mise tools, Homebrew, apt, dnf, pacman, apk, the Mac App Store and apps from GitHub releases, as each platform supports them.
- **Share between machines** with `dots sync`, which scans your changes for secrets before anything leaves the machine.
- **Set up a new machine from your own configuration** with one command, over SSH from a machine you already have.
- **Works with your coding agent.** `dots` links its built-in agent skill when it finds Claude Code or Codex.
  Your own skills are ordinary files: adopt them with `dots add` and they sync like any dotfile; `dots` does not install them for you.

```sh
dots up                                # bring this machine up to date
dots add ripgrep                       # install a tool and record it
dots add ~/.config/app/settings.toml   # adopt an existing file that is safe to share
dots sync                              # scan and share your edits, take the other machines'
dots remote HOST --profile P           # set up another machine from your configuration's repository
```

## Install

<!-- TODO: A-07 installer -->
<!--
Draft of the recommended route; the installer URL is pending until A-07 publishes install.sh at a project-owned endpoint.
curl -fsSL INSTALLER_URL | sh
curl -fsSL INSTALLER_URL | sh -s -- --from OWNER/NAME --profile P
-->

`dots` 0.5.0 is not released yet.
**Planned for 0.5.0:** one line that runs the project's `install.sh`, which installs mise first when it is missing.
The same installer will take `--from REPO --profile P` to set the machine up from your configuration.

<details>
<summary>Other ways to install</summary>

Until 0.5.0, build the current release candidate from source, with Rust 1.100 or newer and a system linker:

```sh
cargo install --locked --git https://github.com/EzraCerpac/dots --branch rust dots-cli
```

Once 0.5.0 is released, any of these will also work; until then the Homebrew tap installs 0.4.0.

```sh
mise use -g github:EzraCerpac/dots@0.5.0
brew install ezracerpac/tap/dots
cargo install dots-cli --locked
```

`dots --version` prints the version and how it was installed.

</details>

`dots` runs on macOS and Linux, including WSL, and needs mise 2026.11.8 or newer and Git.
Sharing changes needs [gitleaks](https://github.com/gitleaks/gitleaks), and encrypted settings need [age](https://age-encryption.org).
Once your configuration declares them, for example with `dots add --base gitleaks age`, `dots up` installs and updates them through mise.
<!-- TODO: A-08 scaffold tools -->
JJ and `gh` are needed only for `dots publish`; with `gh` signed in, `dots init` can also offer to create a GitHub repository.
`dots status` lists the programs it finds, and [Platforms and programs](docs/contract.md#platforms-and-programs) lists what each action needs.

## Start

```sh
dots init
dots up
```

`dots init` creates a configuration in mise's configuration directory, normally `~/.config/mise`, with a Git repository on branches `main` and `dev`.
It keeps an existing `config.toml` and only adds what `dots` needs.
In a terminal with `gh` signed in, a fresh `dots init` offers to create a private GitHub repository; otherwise it prints the commands to add a remote.
Without a remote, everything works on this machine alone; see [adding a remote](docs/sync.md#adding-a-remote) when you want to share.
[`dots init`](docs/contract.md#dots-init-dir) has the details.

## Day to day

- **`dots up`** checkpoints tracked settings and syncs, then updates packages and tools, runs your update hooks and updates mise itself.
  It prints each step and ends with anything that needs attention and what to run next.
  It never removes undeclared software, restarts services or updates project dependencies.
- **`dots add`** installs software and records it, or adopts a file.
  It writes to this machine's profile unless you pass `--base` for every machine or `--profile P` for another profile.
  A path starting with `/`, `~/`, `./` or `../` is a file: it moves into the configuration's `dotfiles/` and a link takes its place.
- **`dots remove`** uninstalls and forgets software, or puts an adopted file back as a regular file.
  `--dry-run` shows what it would do and `--keep-installed` drops only the declaration.
- **`dots sync`** shares your edits on the `dev` branch after a gitleaks scan and applies what the other machines shared.
  Reviewed changes reach `main` through pull requests. [The sync guide](docs/sync.md) explains a run and its receipt.

`dots help` lists every command, including your configuration's own; [the command reference](docs/contract.md#command-reference) describes each.

## More

- [`dots publish`](docs/sync.md#dots-publish) opens a pull request to `main` from a separate JJ workspace of a configuration on GitHub.
- [`dots backup`](docs/contract.md#dots-backup) saves this machine's encrypted app settings and publishes them to a private repository.
- [`dots restore`](docs/contract.md#dots-restore---initialize-history) fetches those settings back onto this machine.
- [`dots bundle`](docs/contract.md#dots-bundle) encrypts private values that [`dots bootstrap`](docs/contract.md#dots-bootstrap) needs to set up a machine.
- [`dots remote HOST --profile P`](docs/contract.md#dots-remote) sets up another machine over SSH from your configuration's repository.

## Why dots

- **Plain mise** already bootstraps a machine and manages tools, packages, dotfiles and their history; if that covers you, keep it.
  `dots` adds one command to update everything, sync between machines that is scanned for secrets, and `dots add` and `dots remove` across package managers.
- **chezmoi or GNU Stow** suit you if dotfiles are all you manage. `dots` keeps tools, packages and dotfiles in one mise configuration.
- **Nix** suits you if you want fully reproducible environments and are happy to learn its language. `dots` stays with mise's TOML and your system's own package managers.

## Development

This repository is the Rust crate `dots-cli`, which builds the `dots` binary; `mise.toml` pins Rust 1.100.0.

```sh
mise install
mise run build
mise run test
```

`dots test` is something else: it runs your configuration's own `tests/run`.
[The contract](docs/contract.md) is the reference for everything `dots` does.

## License

MIT, see [LICENSE](LICENSE).
