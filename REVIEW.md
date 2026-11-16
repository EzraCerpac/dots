# Review guidelines

For anyone reviewing a pull request here, human or automated.

## What this project promises

- `dots` is a generic dotfiles manager layered on mise. Nothing here may assume
  one person's configuration: owner- and machine-specific behaviour comes only
  from the configuration contract (`[vars] dots_*` and the `dots/` directory).
- It runs on macOS and Linux. Updates never prune packages or restart services.
- `dots sync` always exits 0 and prints one outcome block. Anything that needs
  attention is a line starting with an upper-case label (`NOT APPLIED`,
  `SAVE REFUSED`, ...). It never force-pushes, never skips validation of the
  merged configuration and never discards uncommitted local edits.
- A tracked history file is never saved unless it is declared `encrypt = true`.
- Tests build their own configuration or copy `tests/fixtures`; they must pass
  from a copy of this repository alone.

A change that breaks one of these is worth a comment at any size.

## Facts to check claims against

- Supported mise is 2026.11.8 or newer. Do not raise behaviour of older
  releases. Before claiming a mise command behaves a certain way, run it or
  say the claim is unverified.
- CI installs every tool the tests use (`tests/run` and
  `.github/workflows/test.yml`).
- `dots` is run by one person on their own machines.

## Not worth raising

- Skipping a test when a tool CI always installs is missing.
- Locks or repeated checks for races between two concurrent runs by the same
  user, or inside a documented manual procedure, unless data can be lost
  without any message.
- Style, naming and comment wording.
- Added abstraction, options or fallbacks the behaviour does not need: flag
  them as unnecessary, do not ask for more.
