# Rust port architecture

S-02 decisions; [the contract](contract.md) wins, the oracle is v0.4.0 at `f5c8a0e`, and [T-01](https://github.com/EzraCerpac/dots/blob/7555208ed116ce70402856cac9040d2bed20017f/spike/decision.md) lists terminal pitfalls.

## Toolchain and release

Root package `dots-cli`, binary `dots`, committed `Cargo.lock`, release `panic = "unwind"`; crates (checked 2026-11-18) arrive with first users: clap 4.6.8, serde 1.0.229, serde_json 1.0.154, toml_edit 0.25.19+spec-1.1.0, nix 0.32.0, tempfile 3.27.0 (R-01); ureq 3.5.2 on rustls (P-01); ratatui 0.30.2, crossterm 0.29.0 (P-11, P-12).
`mise.toml` pins Rust 1.100.0; `--locked` `check`, `test -- FILTER`, `build` and `clippy` use `rustup run 1.100.0 cargo`, the first two as small local checks, and `format` touches only changed or named files.
CI's `rust.yml` runs the full suite, clippy and `cargo fmt --check` on x86_64 and aarch64 for macOS and musl Linux; Q-02 adds distribution and first-run jobs.
D-01 ships four `dots-VERSION-{darwin,linux}-{amd64,arm64}.tar.gz` archives of only `dots`, static on Linux, plus separate `SHA256SUMS` and a Homebrew formula with completions for C-01's tap.

## Modules and shared code

R-01 (Lead B) owns `main`, `cli` (the clap tree), `config`, `process`, `report`, `run`, `completion`, `skill`, and `root`, `profile`, `mise`, `completions`, help, version and configuration commands.
P-01 `packages`; P-02 `hooks`, `brew`, `inspect`, helpers; P-03 `init`; P-04 `update`; P-05 `bootstrap`; P-06 `history`; P-08 `sync`, accepted with P-06 integrated; P-09 `receipt`; P-10 `publish`; P-11 `supervisor`; P-12 `ui`.
Handlers are `fn(&Ctx, Args) -> Result<u8, Fail>`; an unported one exits 1 naming its task.

- `Root::resolve(&Env)` is physical; `Vars::read(&Root)` reads base `[vars] dots_*`, `mise.json(args)` effective settings; `selection::{read, persist}`; `Doc::open(path)?.install()` swaps in a changed file atomically, mode kept.
- `RepoSetting::parse`, `matches(url)`, `source_mode` and `checkout_shape` are read-only, leaving URL checks, locks and Git writes to P-06 and P-08; `Exceptions::read` fails with status 2 on a bad row and answers `holds(request)` and `selected(profile)`.
- `Targets::read(&Root, Scope)`, shared read-only by `completion` and P-01, reads active in-root files or the selected file, and trusted app markers; `resolve(request, keep_installed) -> One | Ambiguous | None(suggestion)` applies aliases and holds.
- `ctx.command(program, Cwd::{Root, Caller, Dir})` takes OS-string arguments, drops `MISE_ENV` and hook variables, and sets `DOTS_ROOT`, `DOTS_PROFILE` and `PATH` led by a `dots` link to `current_exe()`; `Mise::new(&Ctx)?.command(args)` adds `MISE_CONFIG_DIR`, `MISE_ENV`.
- `BundlePipe::new(json)?.attach(&mut cmd)` gives only that hook a close-on-exec pipe's read end as `DOTS_BUNDLE_FD`, stdin intact, holding `{}` until private recovery is ready.
- `complete(Request { words, cursor, quote, shell, cwd, root }) -> Path | Dir | Values` uses the clap tree, base profiles, configuration commands and `Targets` within [its limits](contract.md#dots-completions-bashzshfish); R-01 ships, tests and owns the Bash, Zsh and Fish adapters, loaders and suite, later on Q-01's `World`.
- `skill::refresh(&Ctx)`, skipped by `dots completions` and completion, extracts to `${XDG_STATE_HOME:-~/.local/state}/dots/skills/dots/SKILL.md` and replaces only a link matching its recorded link and target whose parent stays inside the agent's directory, else warns once.
  `init::TEMPLATES` embeds `init/` and P-03's `src/init/{README.md,config.toml}`.

## Runs, reports and seams (Lead T's interface)

`Run::begin(&ctx, Option<Command>)` (`None`: unreported) offers `start(label) -> StageId`, `start_alongside(label)` for `--mas`, `run(id, cmd, Policy::{Ordinary, Interactive})`, `probe(cmd)`, `change(id, Change { item, before, after })`, `issue(id, Issue)`, `finish(id, Outcome)`, `cancelled()` and `end(status) -> u8`.
`Outcome::{Completed, Deferred, Skipped, Failed}` maps hook and recipe statuses 0, 3, 4 and other, their `DOTS_CHANGES` rows becoming `Change`s; `Issue { diagnostic, next, tail, log }` gets `tail` and `log` only in `up`.
R-01's direct path hands children the caller's terminal, stdin and separate stdout and stderr; `end` saves a minimal private report of stages, outcomes, changes, issues and status, so module tests run before P-11.
It holds no transcript or invented log; a failed write warns on stderr, naming the command, and keeps the result.
P-11 adds retention, stage logs, SIGINT, process groups and PTYs; P-12 renders the dashboard, menu, report browser and `up --last` from R-01's `report::{list, load}` and P-11's `run.snapshot()`; queries live with their data's writer.
Only `up` on a large enough interactive terminal, without `--plain` or `--verbose`, gets the dashboard, an `openpty` per stage; other runs stay direct, and P-12's views, `plan`, the receipt and the `sync --last` pager keep their own displays.
Plain `up` on a terminal still lets an `Interactive` stage like `sudo -v` prompt.
A handoff relays the caller's terminal, which `dots` keeps, to the child's own PTY byte for byte after flushing stale input and resizing; the stage's output from then on may echo input, even late or after a drain, and is never logged or reported.
Ctrl-C, even mid-drain, escalates on every stage group and probe, leaderless ones included, self-detached ones exempt, starts nothing and drains boundedly before saving.
Apart from cleanup, a guard restores termios on normal exit, cancellation and unwinding panic.
After Ctrl-C, a valid `dots sync` returns 0 with its receipt, else `end` returns the foreground child's own nonzero status, else 130.
Nested `bootstrap complete` runs direct and unreported in its stage's group; `--mas` on macOS runs declared `mas` alongside, with cached `sudo`, `sudo -v` on a TTY, or deferral.

- `hooks::list(&Root, Dir, &Vars, profile)?`, `hooks::run(&Run, &Hook, HookEnv) -> Outcome`, `exceptions::run(&Run, &Exceptions, Update | InstallOnly)?`.
- `history::checkpoint(&Run, StageId, &Ctx) -> Result<Saved | NothingTracked, Issue>`; `history::{restore, enroll}(…) -> Outcome`.
- `sync::load(&Ctx) -> LocalOnly | Problem | Ready` and `sync::run(&Run, StageId, Loaded, Display) -> Receipt` never fail; only shared runs checkpoint.
- `Receipt` (P-09): checkout, revisions, paths, outcome, issues, no patches; `receipt::emit` records and prints once, warning on record failure; only standalone, non-`--plain` sync with terminal stdin and stdout may show patches, after [screening](contract.md#receipt).

## Harness (Q-01)

Q-01 builds the harness, translating only the contract, install and home suites; module tasks translate theirs, reporting per-suite counts.
`tests/SUITE.rs` scenarios are `unchanged_*` (v0.4.0 too) or `new_*`, ignored as `P-NN` until ported; `new_*` covers cancellation statuses, `plan`'s report and completion, and `completions --help`.
Oracle: `DOTS_BIN=$PWD/bin/dots mise run test -- unchanged_ -- --include-ignored`; `DOTS_BIN` (absolute) defaults to `CARGO_BIN_EXE_dots`.
`World::new()` runs children from `env_clear()` plus `PATH`, locale and `TERM`, private `HOME`, XDG, Git and JJ settings, and a fixture `DOTS_ROOT` copy, no skill links; methods: `dots`, `git`, `bare_remote(name)`.

## Current files (D-01 deletes ported ones)

| Prefix | Disposition |
| --- | --- |
| root | Kept: AGENTS.md, REVIEW.md, docs/sync.md A-05; README.md A-02; docs/contract.md, docs/rust-port.md Leads A, B; .coderabbit.yaml, .gitignore, LICENSE, plan/ coordinator |
| `.github/` | D-01 replaces it and `packaging/`; R-01 adds rust.yml |
| `bin/` | R-01, as are `completions/` and the kept, embedded `skills/` |
| `init/` | P-03; kept and embedded, except README.md |
| `lib/` | R-01 dots-manager.sh, mise-\*.sh, setup-common.sh; P-01 dots-\*add\*, dots-remove\*, dots-github.py; P-02 brew-exception.sh, retire-dotfiles.sh, dots-packages.sh, dots-hooks; P-08 source-\*.mjs, retired-\*.mjs; P-09 sync-receipt.mjs; P-11 dots-up-events.sh, dots-command-\* except status() (P-02); dropped quiet-probe.sh |
| `tasks/` | P-02 status, exceptions; P-03 init; P-04 update; P-06 backup, restore, README.md; P-10 publish.mjs |
| `bootstrap/` | R-01 profile; P-06 \*history\*; X-01 Atuin, Tailscale; P-05 the rest |
| `dots-up/` | R-01 cmd/, internal/{commands,completion}/; P-11 internal/supervisor/ except browse.go; P-12 that, internal/{ui,launcher}/; dropped go.mod, go.sum |
| `tests/` | Q-01 lib/, fixtures/ (kept), test_dots_contract.py, test_dots_home.py, test_manager_install.py; R-01 test_shell_completions.py; P-01 test_packages.py, test_dots_github.py; P-04 dots-up-\*.sh; P-05 test_fixture_config.py, test_dots_bootstrap_hooks.mjs; P-06 test_native_history.py; P-07 the other .mjs; X-01 test_atuin.py; A-05 test_docs_links.py; dropped run, test_syntax.py |
