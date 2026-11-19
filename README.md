# dots

**One command keeps your machine current.**
`dots up` installs what your configuration declares, updates it, and takes in what your other machines shared.

- **One ordinary [mise](https://mise.jdx.dev) configuration** holds the tools, packages and dotfiles of every Mac and Linux machine you use, with a profile for each kind of machine.
  There is no new language to learn, and if you stop using `dots`, your configuration is still plain mise TOML.
- **Add software from many sources** with `dots add`: mise tools, Homebrew, apt, dnf, pacman, apk, the Mac App Store and apps from GitHub releases, as each platform supports them.
- **Share between machines** with `dots sync`, which scans your changes for secrets before pushing configuration changes.
- **Set up a new machine from your own configuration** with one command, over SSH from a machine you already have.
- **Extend it with your own scripts.** Executables in your configuration's `dots/hooks/update.d/` run during `dots up`, those in `dots/hooks/bootstrap.d/` during `dots bootstrap`, and an executable `dots/commands/NAME` becomes `dots NAME`, listed in `dots help`.
  [The `dots/` directory](docs/contract.md#the-dots-directory) has the naming rules.
- **Works with your coding agent.** `dots` links its built-in agent skill when it finds Claude Code or Codex.
  Your own skills are ordinary files: adopt them with `dots add` and they sync like any dotfile; `dots` does not install them for you.

```sh
dots up                                # bring this machine up to date
dots add ripgrep                       # install a tool and record it
dots add ~/.config/app/settings.toml   # adopt an existing file that is safe to share
dots sync                              # scan and share your edits, take the other machines'
dots remote HOST --profile P           # set up another machine from your configuration's repository
```

<!-- TODO (A-03): Record the released 0.5.0 binary running `dots init`, `dots add` and a step-by-step `dots up` in the terminal. -->

## Install

<!-- TODO: A-07 installer -->

```sh
curl -fsSL https://raw.githubusercontent.com/EzraCerpac/dots/main/install.sh | sh
```

**Not available yet:** this installer arrives with 0.5.0.
It installs mise if it is missing, installs `dots`, then runs `dots init`.

<details>
<summary>Set up from your existing dotfiles repository</summary>

```sh
curl -fsSL https://raw.githubusercontent.com/EzraCerpac/dots/main/install.sh | sh -s -- --from OWNER/NAME --profile laptop
```

With `--from` and `--profile`, the installer clones that repository instead of running `dots init`, and bootstraps this machine from it with that profile.

</details>

<details>
<summary>Other ways to install</summary>

<!-- TODO (G-2): Verify these routes against the published 0.5.0 artifacts. -->
<!-- TODO (C-01): Add a Cargo install route once dots-cli is published. -->

Once 0.5.0 is released, these will also work; until then the Homebrew tap installs 0.4.0.

```sh
mise use -g github:EzraCerpac/dots@0.5.0
brew install ezracerpac/tap/dots
```

`dots --version` prints the version and how it was installed.

</details>

`dots` runs on macOS and Linux, including WSL, and needs mise 2026.11.8 or newer and Git.
Sharing changes needs [gitleaks](https://github.com/gitleaks/gitleaks), and encrypted settings need [age](https://age-encryption.org).
Once your configuration declares them, for example with `dots add --base gitleaks age`, `dots up` installs and updates them through mise.
<!-- TODO: A-08 scaffold tools -->
[Platforms and programs](docs/contract.md#platforms-and-programs) lists what each action needs.

## Start

```sh
dots init
dots up
```

`dots init` creates a configuration in mise's configuration directory, normally `~/.config/mise`, and when there is no Git repository, one on branches `main` and `dev`.
It keeps an existing `config.toml` and only adds what `dots` needs.
When it creates the repository in a terminal with `gh` signed in, and `dots_source_repo` is unset or names the proposed one, it offers to create a private GitHub repository; the default is no.
Otherwise, without `origin`, it prints how to add a remote.
Until there is a remote, the configuration stays on this machine; see [adding a remote](docs/sync.md#adding-a-remote).
[`dots init`](docs/contract.md#dots-init-dir) has the details.

**Public or private?** Sync runs the same gitleaks scan either way, and the repository `dots init` offers is private.
In a public repository, every synced change is public as soon as it reaches the shared `dev` branch.
gitleaks catches token and key patterns, but personal host names and addresses can pass.
Machine-local files stay out of the repository, and encrypted settings history lives in a separate private repository.

## Day to day

- **`dots up`** checkpoints tracked settings and syncs, then updates packages and tools, runs your update hooks and updates mise itself.
  It prints each step and ends with anything that needs attention and what to run next.
  It applies your declared dotfiles too: through sync when there is a remote, directly in local-only mode.
  It never removes undeclared software, restarts services or updates project dependencies.
  To preview or apply on their own, run `dots mise bootstrap plan` or `dots mise bootstrap dotfiles apply TARGET`.
- **`dots add`** installs software and records it, or adopts a file.
  It writes to this machine's profile unless you pass `--base` for every machine or `--profile P` for another profile.
  A path starting with `/`, `~/`, `./` or `../` is a file: it moves into the configuration's `dotfiles/` and a link takes its place.
- **`dots remove`** uninstalls and forgets software, or puts an adopted file back as a regular file.
  `--dry-run` shows what it would do and `--keep-installed` drops only the declaration.
- **`dots sync`** shares your edits on the `dev` branch after a gitleaks scan and applies what the other machines shared.
  Reviewed changes reach `main` through pull requests. [The sync guide](docs/sync.md) explains a run and its receipt.

`dots help` lists every command, including your configuration's own; [the command reference](docs/contract.md#command-reference) describes each.

## More

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

This repository is the Rust crate `dots-cli`, which builds the `dots` binary.
`mise.toml` pins Rust 1.100.0, and `mise install` installs it.
`mise exec` then runs Cargo and rustc from that toolchain, even when mise is not activated in your shell:

```sh
mise install
mise exec -- cargo build --locked
mise exec -- cargo test --locked -- --test-threads 1
```

The full test suite also needs real scenario tools on your `PATH`; [the test guide](tests/README.md#prerequisites) lists them.

`dots test` is something else: it runs your configuration's own `tests/run`.
[The contract](docs/contract.md) is the reference for everything `dots` does.

## License

MIT, see [LICENSE](LICENSE).
