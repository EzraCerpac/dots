# Review guidelines

For anyone reviewing a pull request here, human or automated.
[The contract](docs/contract.md) is the reference for what `dots` 1.0 does; check behaviour claims against it, and [the port architecture](docs/rust-port.md) for who owns which module.

## What this project promises

- `dots` is a generic dotfiles manager layered on mise.
  Nothing here may assume one person's configuration: owner- and machine-specific behaviour comes only from the configuration (`[vars] dots_*` and the `dots/` directory).
- An ordinary `dots up` never prunes software, restarts services or updates project dependencies.
- During guarded writes and rollback, preserve concurrent edits and files that obstruct the operation. Replace owned files atomically where the contract requires it.
  When a step of `dots add` or `dots remove` fails, it puts back only what it changed.
- A command checks only the programs its requested action uses; a missing one fails that action with one line naming it, without failing an inspection command or a stage with nothing to do.
- Ctrl-C cancels every active stage's process group, starts no further stage, and waits for cleanup before saving the report or exiting, with the statuses the contract gives.
- `dots up` prints each step and its result in terminal scrollback. Hooks, recipes and known interactive steps own the terminal from the start, and `dots` never reads their replies.
  Ordinary steps keep private logs. A failed step that had the terminal gives its status and next action and points to scrollback without saving a tail or log.
  An individual command covered by the contract’s direct-output rules runs its child in the caller's foreground with direct stdin, stdout and stderr, byte for byte, so native prompts such as `sudo` or `ssh` work, and adds no banner or footer around that output.
- A child that `dots` starts directly has the running `dots` first on `PATH`, after the configuration environment.
  An explicit `dots mise exec` or `dots mise run` follows mise's own `PATH` rules, and mise's native bootstrap callbacks use the configuration's pinned `dots`; neither is that guarantee.
- A tracked history file is never saved unless it is declared `encrypt = true`, and `dots` keeps plaintext bundle values out of files, arguments, environment variables and its output.
- `dots sync` with valid arguments always exits 0 and prints a receipt.
  The receipt records verified sharing, local saves and checkout updates, and each issue carries a diagnostic and recovery action.
  Patch contents appear only on a terminal, after a clean gitleaks screen; piped, `--plain` and saved receipts carry metadata only.
  Sync never force-pushes, skips validation or discards uncommitted edits.
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
- Git is needed for `init`, `sync`, `backup`, `restore`, `bootstrap` and `publish`; sync needs Git 2.43 or newer.
  gitleaks, age, `gh`, JJ, ssh and native package managers are needed only by the actions that use them.
- CI installs every tool the tests use.

## Not worth raising

- Skipping a test when a tool CI always installs is missing.
- Style, naming and comment wording.
- Added abstraction, options or fallbacks the behaviour does not need: flag them as unnecessary, do not ask for more.
