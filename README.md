# dots

`dots` keeps your tools, native packages and dotfiles in one ordinary [mise](https://mise.jdx.dev) configuration, and `dots up` brings a machine up to date with it in one command.
An optional Git remote shares that configuration between your machines, and `dots sync` scans your changes for secrets before it pushes them.

<!-- TODO (A-03): Record the integrated 1.0 binary running `dots init`, `dots add` and a step-by-step `dots up` in the terminal. -->

## Install

For the local quickstart on macOS, Linux or WSL, install mise 2026.11.8 or newer and Git.
Choose one way to install the self-contained `dots` binary.

<!-- TODO (D-01, C-01, G-2): Verify these alternatives against the published 1.0 artifacts. The current tap installs 0.4.0. -->

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
| Git | `init`, `add FILE`, `backup`, `restore`, `bootstrap` and `publish`. Sync and the sync step of `up` need Git 2.43 or newer. |
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

<details>
<summary>Output of a first run on a fresh machine</summary>

From the hosted first-run check of `rust` at `4f39a55`: `dots` 1.0.0-rc.1 on Alpine with mise 2026.11.9 and Git, no terminal, with an empty home directory under `/tmp`.

```text
created configuration directory /tmp/dots-first-run.XXDblMdB/home/.config/mise
created config.toml
created dots/
created dots/hooks/
created dots/hooks/update.d/
created dots/hooks/bootstrap.d/
created dots/commands/
created dots/exceptions/
created dots/README.md
created dots/exceptions.tsv
created dots/retired-dotfiles
created dots/retired-declarations
created .gitleaks.toml
created .gitignore
created profile default
created machine ID 9413e1032403
created Git repository
created seed commit bef27f09a2e3e81d7af8a8fe8b5a9bedccef54c3 on main and dev

No remote yet. To share this configuration, create an empty repository and run
git -C '/tmp/dots-first-run.XXDblMdB/home/.config/mise' remote add origin URL
git -C '/tmp/dots-first-run.XXDblMdB/home/.config/mise' push --no-follow-tags --recurse-submodules=no origin 'bef27f09a2e3e81d7af8a8fe8b5a9bedccef54c3^{commit}:refs/heads/main' 'bef27f09a2e3e81d7af8a8fe8b5a9bedccef54c3^{commit}:refs/heads/dev'
These publish only the seed. Later changes are shared with dots sync, which scans them first.
==> Pre-update settings checkpoint
Pre-update settings checkpoint: skipped
Sync configuration (no remote configured): skipped
==> Reload configuration
Reload configuration: completed
==> Inspect declared packages
Inspect declared packages: completed
==> Install declared mise tools
Install declared mise tools: completed
==> Upgrade declared mise tools
Upgrade declared mise tools: completed
==> Standalone mise
Standalone mise: completed
==> dots manager
dots manager: skipped
==> Post-update settings checkpoint
Post-update settings checkpoint: skipped
dots up: Complete (exit 0)
Details and logs · dots up --last
```

</details>

<!-- TODO (I-01): Replace this output with the released 1.0 binary's once it exists. -->

`dots init` sets up the configuration:

- It uses mise's configuration directory, normally `~/.config/mise`. For a separate directory, use `dots init "$HOME/dotfiles"`, then `DOTS_ROOT="$HOME/dotfiles" dots up`.
- It creates missing files and adds `dots_profiles = "default"` under `[vars]` if the existing `config.toml` lacks that setting. It preserves the rest of the file.
- It keeps a valid saved profile and machine ID. For a fresh setup, it selects `default` if listed (otherwise the first listed profile) and derives the machine ID from the hostname. Both selections stay on this machine.
- In a new repository it makes one seed commit on branches `main` and `dev`, with `dev` checked out. An existing repository keeps its branches and history; `init` reports anything sync would stop on, with its fix.
- In the run that creates the seed, while the seed is untouched, a terminal with `gh` signed in gets an offer to create a private GitHub repository. The default is no. For a pristine seed, the printed push uses the verified commit ID. For any other repository without an `origin`, follow the printed remote-add and `dots sync` instructions so outgoing commits are scanned.

`dots up` checks for tracked settings to checkpoint, then syncs and updates eligible software.
In local-only mode (no `origin` and no `dots_source_repo`), it skips sync. Updating tools or standalone mise can still use the network.

## Day to day

```sh
dots status                                  # what is declared, installed and missing
dots plan                                    # mise's bootstrap plan; installs nothing
dots up                                      # checkpoint, sync and update eligible software
dots apply                                   # apply declared dotfiles and templates locally
dots up --last                               # print the saved report and return its exit status
dots reports                                 # list recent completed reports, newest first
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

Run `dots` or `dots help` to list the built-in commands and your configuration's own commands.
`dots root` prints the configuration directory and `dots profile` prints the selected profile.

`dots up` writes ordinary terminal scrollback: a `==>` line when each step starts, one line with its result, and a final line such as `dots up: Complete (exit 0)`.
Answer a hook’s or installer’s prompt directly in the terminal.
When something went wrong, “Needs attention” lists each issue with a `Next:` action, and the run still exits 0 unless a step failed.
For a failed step that owned the terminal, its output is in your scrollback. With `--plain` or without a terminal, hook and recipe failures include a private log path.
`dots up --last` replays only that final report, not the live step output.

## Sharing between machines

When sharing is configured, `dots sync` and the sync stage of `dots up` use two branches: `dev`, which every machine runs, and `main`, which you change through pull requests.

- A sync with valid arguments always exits 0. Its receipt says what was shared, what stayed local and what needs attention.
- Sync scans this machine's new and outgoing commits with gitleaks. A flagged file stays on disk, unshared; without gitleaks nothing is pushed.
- Machine-local files, such as the profile selection and lockfiles, stay out of the repository.
- On the remote, sync only creates or fast-forwards `dev`, and never moves `main`.
- Where edits conflict, `main` wins over the shared `dev`, and `dev` over this machine's work; both sides stay in history.
- In local-only mode, sync skips the entire pipeline and says how to add a remote.

For example, this receipt followed an edit to `config.toml` next to a new file holding a secret.
It comes from `dots` 1.0.0-rc.1 at `rust` `9a35df0`, in a disposable home directory with a local bare `origin`:

```text
NOT SHARED: gitleaks flagged a local file; it stays on disk: "leak.toml:2" (generic-api-key)
  Remove or rotate the secret before sharing this path.
dots sync: saved 1 locally · kept 1 files on disk · pulled dev +0, main +0 · pushed dev
Shared on dev
  config.toml
Saved locally
  config.toml
```

`dots sync --last` printed the same receipt under a `Latest sync` heading.

See [the 1.0 sync rules and receipt](docs/contract.md#dots-sync) for the order of operations and how to read the outcome.

## Optional, later

- `dots publish [--all | --bookmark NAME]` opens a pull request to `main` from a separate colocated JJ workspace of a configuration hosted on GitHub. Updating an existing pull request keeps its base.
- `dots backup` saves a local checkpoint of this machine's encrypted app settings, then publishes it to a separate private repository. It reports publication only when the remote host branch contains the saved checkpoint. If reconciliation is still pending, the checkpoint stays local and backup points to `mise dot status`; if the readback cannot verify publication, whether it was published is unknown, even if a push happened, and backup says to check the private history remote. Both exit 1.
- `dots restore` fetches those settings back after safety checks and never pushes. It may save encrypted undo checkpoints of the files it replaces locally; a later explicit `dots backup` that succeeds publishes them.
- [`dots bootstrap`](docs/contract.md#dots-bootstrap) sets up the local machine. `dots remote HOST --profile P` sets up a machine over SSH and leaves its mise history settings alone.
  With history enabled, the target stores unencrypted tracked files in plaintext in its local history. Set `encrypt = true` on those files or disable history on the target to avoid that.
  Before publishing, `dots backup` checks the outgoing checkpoints for plaintext and respects mise's `allow_plaintext_history` opt-in.

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
