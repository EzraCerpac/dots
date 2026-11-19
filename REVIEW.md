# Review guidelines

For anyone reviewing a pull request here, human or automated.
[The contract](docs/contract.md) is the reference for what `dots` 1.0 does; check behaviour claims against it, and [the port architecture](docs/rust-port.md) for who owns which module.

## What this project promises

- `dots` is a generic dotfiles manager layered on mise.
  Nothing here may assume one person's configuration: owner- and machine-specific behaviour comes only from the configuration (`[vars] dots_*` and the `dots/` directory).
- An ordinary `dots up` never prunes software, restarts services or updates project dependencies.
- `dots` guards its own writes and rollbacks: it preserves edits made in the meantime and files that obstruct the operation, and replaces the files it owns atomically where the contract requires it.
  mise's native template and inline apply is outside that guard and can lose a file created between planning and installation.
  When a step of `dots add` or `dots remove` fails, it puts back only what it changed.
- A command checks only the programs its requested action uses; a missing one fails that action with one line naming it, without failing an inspection command or a stage with nothing to do.
- Ctrl-C cancels every active stage's process group, starts no further stage, and waits for cleanup before saving the report or exiting, with the statuses the contract gives.
- In interactive scrollback mode, `dots up` prints each step and its result. Hooks, recipes and known interactive steps own the terminal from the start, and `dots` never reads their replies.
  Ordinary steps keep private logs. A failed step that had the terminal gives its status and next action and points to scrollback without saving a tail or log.
  In concise, headless and `--plain` updates, hooks and recipes keep the caller’s stdin and write to private logs with failure tails. `--verbose` also mirrors their captured output.
  An individual command covered by the contract’s direct-output rules runs its child in the caller's foreground with direct stdin, stdout and stderr, byte for byte, so native prompts such as `sudo` or `ssh` work, and adds no banner or footer around that output.
- A child that `dots` starts directly has the running `dots` first on `PATH`, after the configuration environment.
  An explicit `dots mise exec` or `dots mise run` follows mise's own `PATH` rules, and mise's native bootstrap callbacks use the configuration's pinned `dots`; neither is that guarantee.
- `dots remote` preserves the SSH target’s history settings. With history enabled, mise can store an unencrypted tracked file in plaintext locally.
  Before invoking native sync, `dots backup` checks the tracked payloads in outgoing checkpoints, including files whose declarations have changed, and respects mise’s `allow_plaintext_history` opt-in.
- `dots` keeps plaintext bundle values out of files, arguments, environment variables and its output.
- `dots sync` with valid arguments always exits 0 and prints a receipt.
  The receipt records verified sharing, local saves and checkout updates, and each issue carries a diagnostic and recovery action.
  Patch contents appear only on a terminal, after a clean gitleaks screen; piped, `--plain` and saved receipts carry metadata only.
  Sync never force-pushes, skips validation or writes conflict markers into live files.
  A push whose outcome is uncertain stops the run before any retry, checkout or apply, and is reported as unconfirmed, never as shared.
- Tests build their own configuration or copy `tests/fixtures`; they must pass from a copy of this repository alone.

A change that breaks one of these is worth a comment at any size.

Check that background Mac App Store work logs privately, announces its result at a stage boundary and finishes before the standalone mise stage.
Authorization belongs in the foreground before the worker starts. Expired authorization defers the work with a next action.
`dots reports` prints the same newest-first listing to a terminal, a pipe or a redirected file.
`dots up --last` prints the saved report and returns its recorded status.

## Facts to check claims against

- The binary is `dots`, from the root crate `dots-cli`.
- `mise.toml` pins Rust 1.100.0; Cargo runs with `--locked` against the committed `Cargo.lock`, and release builds keep `panic = "unwind"` so the terminal is restored on a panic.
- `dots` runs on macOS and Linux, including Linux under WSL; native Windows is not supported.
- Supported mise is 2026.11.8 or newer; do not raise behaviour of older releases.
  Before claiming a mise command behaves a certain way, run it or say the claim is unverified.
- Git is needed for `init`, `add FILE`, `sync`, `backup`, `restore`, `bootstrap` and `publish`; sync needs Git 2.43 or newer.
  gitleaks, age, `gh`, JJ, ssh and native package managers are needed only by the actions that use them.
- CI installs every tool the tests use.

## Not worth raising

- Skipping a test when a tool CI always installs is missing.
- Style, naming and comment wording.
- Added abstraction, options or fallbacks the behaviour does not need: flag them as unnecessary, do not ask for more.
