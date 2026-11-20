# dots

**One command keeps your machine current.**
`dots up` installs what your configuration declares, updates it, and takes in what your other machines shared.

- **One ordinary [mise](https://mise.jdx.dev) configuration** holds the tools, packages and dotfiles of every Mac and Linux machine you use, with a profile for each kind of machine.
  There is no new language to learn, and if you stop using `dots`, your configuration is still plain mise TOML.
- **Add software from many sources** with `dots add`: mise tools, Homebrew, apt, dnf, pacman, apk, the Mac App Store and apps from GitHub releases, as each platform supports them.
- **Share between machines** with `dots sync`, which scans your changes for secrets before pushing configuration changes.
- **Set up another machine from your own configuration** with the installer's `--from`, run on that machine; from then on `dots sync` keeps both in step.
- **Extend it with your own scripts.** Executables in your configuration's `dots/hooks/update.d/` run during `dots up`, those in `dots/hooks/bootstrap.d/` during `dots bootstrap`, and an executable `dots/commands/NAME` becomes `dots NAME`, listed in `dots help`.
  [The `dots/` directory](docs/contract.md#the-dots-directory) has the naming rules.
- **Works with your coding agent.** `dots` links its built-in agent skill when it finds Claude Code or Codex.
  Your own skills are ordinary files: adopt them with `dots add` and they sync like any dotfile; `dots` does not install them for you.

```sh
dots up                                # bring this machine up to date
dots add ripgrep                       # install a tool and record it
dots add ~/.config/app/settings.toml   # adopt an existing file that is safe to share
dots sync                              # scan and share your edits, take the other machines'
```

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/EzraCerpac/dots/main/install.sh | sh
```

It installs `dots` through mise, installing mise first when needed, links `~/.local/bin/dots`, and in a terminal runs `dots init`.

<details>
<summary>Set up this machine from your existing repository</summary>

```sh
curl -fsSL https://raw.githubusercontent.com/EzraCerpac/dots/main/install.sh | sh -s -- --from OWNER/NAME --profile laptop --version X
```

Each option is optional.

- `--from` takes `OWNER/NAME`, an `https://` URL or an SSH address, with no credentials in it.
  Instead of running `dots init`, the installer clones the repository and runs mise's native bootstrap of it.
  It clones `dev` when the remote has that branch, and otherwise `main`; after a `main`-only clone, the first shared `dots up` switches the checkout to `dev` and publishes it.
  Your Git `url.*.insteadOf` rewrites apply to the clone, and the installer then checks that the checkout's saved `origin` is the repository you named.
- If the configuration directory, normally `~/.config/mise`, is already a checkout of that repository, the installer reuses it, so the same command can be rerun after a failure; the rerun runs the native bootstrap again before saving the profile, so work it already did may repeat.
  It leaves any other Git checkout there alone and stops; anything else is first moved aside to the same path with `.before-dots-` and the UTC time appended.
- `--profile` must name a profile that the configuration declares, and match the profile this machine has already saved, if any.
  Without it, the saved profile is used, or else the only declared one; with several, the installer asks in a terminal and refuses without one.
  A refusal exits with status 2, names the declared profiles and prints the command to rerun with a declared profile.
- After the bootstrap succeeds, the installer saves the profile and machine ID on this machine, so the configuration needs no hook for that, and prints a `Next:` line with the `dots up` command to run.
- Without a terminal, it adopts only with `MISE_YES=1` set for the receiving `sh`; otherwise it exits 2 and prints the command to rerun.
- `--version X` installs release `X` rather than the latest; `DOTS_VERSION=X`, set for the receiving `sh`, does the same, and the flag wins.

</details>

<details>
<summary>What the installer does</summary>

- It uses the mise named by `DOTS_MISE_BIN`, which must then be new enough, or else `~/.local/bin/mise`, or else the mise on your `PATH`.
  When there is none, or its version is too old or unreadable, it installs standalone mise at `~/.local/bin/mise` with the official installer, without `sudo`.
  Native packages that a bootstrap installs, for example with apt, may still ask for your password.
- It installs the newest release, or the one `--version` or `DOTS_VERSION` names, as an exact version and verifies the executable.
- After `dots init` or `--from` adoption succeeds, if no configuration file declares `dots`, `dots init` or the installer records one fallback declaration in `conf.d/00-dots-manager.toml` in the configuration directory: `latest`, or the exact version you requested.
  That file stays on this machine, and a declaration in `config.toml` takes precedence over it.
  A configuration that already declares `dots` gets nothing written.
- It links `~/.local/bin/dots` to mise's `dots` shim unless something is already at that path.
- In a terminal without `--from`, it runs `dots init` and passes its output through unchanged.
  Without a terminal, it never runs `dots init`; it prints the command to run in a terminal instead,
  which names the verified executable by its absolute path and sets `DOTS_ROOT`, `DOTS_MISE_BIN` and `DOTS_VERSION`, quoted for the shell.
- Each command it prints for you is plain `dots` when that already reaches the new installation.
  Otherwise it names `~/.local/bin/dots` or the shim by its absolute path, quoted for the shell.
  When the configuration is not in mise's default directory, it sets both `DOTS_ROOT` and `MISE_CONFIG_DIR` and runs the shim through `mise -C` in that directory, all quoted,
  so the shim loads that configuration and the project you run it from cannot select a different `dots` version.
  After `dots init`, the installer then adds its own `Next:` line.
- As it exits, it prints `PATH` advice on stderr, after the command: the line that puts `~/.local/bin` first, for `fish` or other shells, or, when something else is at `~/.local/bin/dots`, how to run the installed `dots` alongside it.
  Follow that advice for new shells; the printed command works without it.

</details>

<details>
<summary>Install with mise instead</summary>

```sh
mise use -g github:EzraCerpac/dots@0.5.1
```

`dots --version` prints the version and how it was installed.

</details>

<details>
<summary>Install with Cargo or Homebrew instead</summary>

```sh
cargo install dots-cli --locked
cargo binstall dots-cli
brew install EzraCerpac/tap/dots
```

The crate is named `dots-cli` and installs the `dots` command.
`cargo install` builds it from source and needs Rust 1.100 or newer;
`cargo binstall`, if you already have that plugin, downloads the release archive instead.
The Homebrew formula installs Git but not mise, so install mise as well.
`dots --version` then prints the installed version.

</details>

`dots` runs on macOS and Linux, including WSL, and needs Git and mise 2026.11.8 or newer; when mise reports an older version, `dots` stops and prints the command that updates it.
Sharing changes needs [gitleaks](https://github.com/gitleaks/gitleaks), and encrypted settings need [age](https://age-encryption.org).
A `config.toml` that `dots init` creates declares both in `[tools]`; `init` does not add them to an existing one, so there declare only the ones that are missing and keep any valid declaration already there.
Once they are declared, `dots up` installs and updates them through mise.
[Platforms and programs](docs/contract.md#platforms-and-programs) lists what each action needs.

## Start

```sh
dots init
dots up
```

`dots init` creates a configuration in mise's configuration directory, normally `~/.config/mise`, and when there is no Git repository, one on branches `main` and `dev`.
It keeps an existing `config.toml` and only adds what `dots` needs.
Given another directory, `dots init DIR` ends with `Next: DOTS_ROOT=DIR dots up`, with `DIR` quoted for the shell;
a `dots` installed through mise instead ends with a `Next:` line that runs `dots up` through `mise -C DIR`, so that configuration selects the `dots` version.
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
  It refuses credential files, since dotfiles are shared through Git: `~/.netrc`, `~/.git-credentials`, `~/.aws/credentials`, `~/.config/gh/hosts.yml`, `~/.docker/config.json`, `~/.pgpass` and private keys `~/.ssh/id_*`, in any letter case.
- **`dots remove`** uninstalls and forgets software, or puts an adopted file back as a regular file.
  `--dry-run` shows what it would do and `--keep-installed` drops only the declaration.
- **`dots sync`** shares your edits on the `dev` branch after a gitleaks scan and applies what the other machines shared.
  Reviewed changes reach `main` through pull requests. [The sync guide](docs/sync.md) explains a run and its receipt.

`dots help` lists every command, including your configuration's own; [the command reference](docs/contract.md#command-reference) describes each.

## More

- [`dots backup`](docs/contract.md#dots-backup) saves this machine's encrypted app settings and publishes them to a private repository.
- [`dots restore`](docs/contract.md#dots-restore---initialize-history) fetches those settings back onto this machine.
- [`dots bundle`](docs/contract.md#dots-bundle) encrypts private values that [`dots bootstrap`](docs/contract.md#dots-bootstrap) needs to set up a machine.
- [`dots remote HOST --profile P`](docs/contract.md#dots-remote) runs mise's remote bootstrap over SSH from your configuration's repository.
  A real enrollment through it has not been verified; to set up another machine, run the installer with `--from` on that machine.

## Why dots

- **Plain mise** already bootstraps a machine and manages tools, packages, dotfiles and their history; if that covers you, keep it.
  `dots` adds one command to update everything, a one-line setup of another machine from your repository, sync between machines that is scanned for secrets, and `dots add` and `dots remove` across package managers.
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
