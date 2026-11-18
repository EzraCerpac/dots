# Rust port architecture

S-02 decisions; [the contract](contract.md) wins, the oracle is v0.4.0 at `f5c8a0e`, and [T-01](https://github.com/EzraCerpac/dots/blob/7555208ed116ce70402856cac9040d2bed20017f/spike/decision.md) lists terminal pitfalls.

## Toolchain and release

Root package `dots-cli`, binary `dots`, committed `Cargo.lock`, release `panic = "unwind"`; crates (checked 2026-11-18) arrive with first users: clap 4.6.8, serde 1.0.229, serde_json 1.0.154, toml_edit 0.25.19+spec-1.1.0, nix 0.32.0, tempfile 3.27.0 (R-01); ureq 3.5.2 on rustls (P-01); ratatui 0.30.2, crossterm 0.29.0 (P-11, P-12).
`mise.toml` pins Rust 1.100.0; tasks `check` and `test -- FILTER` (small local checks), `build` and `clippy` run `rustup run 1.100.0 cargo … --locked`; `format` touches only changed or named files.
CI's `rust.yml` runs the full suite, clippy and `cargo fmt --check` on x86_64 and aarch64 macOS and musl Linux; Q-02 adds distribution and first-run jobs.
D-01 ships four `dots-VERSION-{darwin,linux}-{amd64,arm64}.tar.gz` archives of only `dots`, static on Linux, plus separate `SHA256SUMS` and a Homebrew formula with completions for C-01's tap.

## Modules and shared code

R-01 owns `main`, `cli`, `config`, `process`, `report`, `run`, `completion`, `skill`, and `root`, `profile`, `mise`, `completions`, help, version and configuration commands.
P-01 `packages`; P-02 `hooks`, `brew`, `inspect`, helpers; P-03 `init`; P-04 `update`; P-05 `bootstrap`; P-06 `history`; P-08 `sync` (accepted with P-06); P-09 `receipt`; P-10 `publish`; P-11 `supervisor`; P-12 `ui`.
Handlers are `fn(&Ctx, Args) -> Result<u8, Fail>`; tasks own their `src/` directories, R-01 the root dispatch and `mod` lines.
Unported handlers exit 1 naming their task, and their seams below are typed stubs failing likewise via `Err` or `Outcome::Failed`, else unreachable, never faking success; P-04 and P-05 build on them, running history-dependent acceptance as P-06 and P-08 land.
`Root::resolve(&Env)` is physical; `Vars::read(&Root)` reads base `[vars] dots_*`, `mise.json(args)` effective settings; `selection::{read, persist}`; `Doc::open(path)?.install()` installs a changed file atomically, mode kept.
`RepoSetting::parse`, `matches(url)`, `source_mode` and `checkout_shape` are read-only (URL checks, locks, Git writes: P-06, P-08); `Exceptions::read` fails with 2 on a bad row and answers `holds(request)`, `selected(profile)`.
`Targets::read(&Root, Scope)`, shared by `completion` and P-01, and `resolve(request, keep_installed) -> One | Ambiguous | None(suggestion)` apply aliases and holds.
`ctx.command(program, Cwd::{Root, Caller, Dir})` takes OS-string arguments, drops `MISE_ENV`, hook variables and `DOTS_STAGES`, and sets `DOTS_ROOT`, `DOTS_PROFILE` and `PATH` led by a `dots` link to `current_exe()`; `Mise::new(&Ctx)?.command(args)` adds `MISE_CONFIG_DIR`, `MISE_ENV`.
`BundlePipe::new(json)?.attach(&mut cmd)` gives only that hook a close-on-exec pipe as `DOTS_BUNDLE_FD`, stdin intact.
`complete(Request { words, cursor, quote, shell, cwd, root }) -> Path | Dir | Values` uses the clap tree, base profiles, configuration commands and `Targets` within [its limits](contract.md#dots-completions-bashzshfish); R-01 owns the Bash, Zsh and Fish adapters, loaders and executable tests, on fixture helpers Q-01 grows into `World`.
`skill::refresh(&Ctx)` extracts to `${XDG_STATE_HOME:-~/.local/state}/dots/skills/dots/SKILL.md` and replaces only a link matching a `dots/skill-links` row whose parent stays inside the agent's directory, else warns once; `init::TEMPLATES` embeds `init/` and P-03's `src/init/{README.md,config.toml}`.

## Runs, reports and seams (Lead T's interface)

`Run::begin(&ctx, Sink::{Report(Command), Fragment(PathBuf), Unreported})` offers `start(label) -> StageId`, `start_alongside(label)`, `run(id, cmd, Policy::{Ordinary, Interactive}) -> io::Result<ExitStatus>`, `probe(cmd) -> io::Result<Output>`, `change(id, Change { item, before, after })`, `issue(id, Issue)`, `finish(id, Outcome)`, `cancelled()`, `snapshot() -> Snapshot`, `view(Option<StageId>)` and `end(status) -> u8`.
`run` returns spawn and I/O errors as `Err` and the child's real status even after an interrupt; `Run` tracks cancellation and `end` derives the [contracted status](contract.md#exit-status) after cleanup.
`Outcome::{Completed, Deferred, Skipped, Failed}` maps hook and recipe statuses 0, 3, 4, other, `DOTS_CHANGES` rows becoming `Change`s; `Issue { diagnostic, next, tail, log }` has `tail` and `log` only in `up`; `Snapshot` lists stage IDs, labels and report fields.
R-01's direct path gives children the caller's terminal and stdio, no screen switch, and `end` saves a minimal private report of stages and status, no transcript or invented log, warning if unwritable; so module tests, and P-12's against `snapshot` and `view`, run before P-11 adds retention, stage logs, SIGINT, group cancellation, PTYs and the transitions `view` requests (`Some(id)` manual view, `None` dashboard).
P-12's views read `snapshot()` and R-01's private `report::{list, load}`; queries live with their data's writer.
Only the contract's `up` dashboard uses PTYs, an `openpty` per stage; other runs stay direct, where an `Interactive` stage like `sudo -v` still prompts.
A handoff flushes stale input, resizes and relays the caller's terminal, which `dots` keeps, to the child's PTY byte for byte; output from then on may echo input, even late, and is never logged or reported.
Ctrl-C, even mid-drain, escalates on every stage group and probe, leaderless included, self-detached exempt, and drains boundedly before saving; a separate guard restores termios on exit, cancellation and unwinding panic.
Native mise runs `dots bootstrap complete` as a separate, direct process in its stage's group and cancellation scope; `DOTS_STAGES`, set only on that stage, names a unique private file its `Sink::Fragment` run alone atomically rewrites with its finished report stages, each hook its own, metadata only, no secrets or transcript.
After the native stage, even failed or cancelled, the parent folds them into its single report; standalone `complete` prints outcomes and saves none; no event stream, stdout parsing, daemon or second supervisor.
`--mas` runs `mas` alongside with cached `sudo`, `sudo -v` on a TTY, or deferral.
`hooks::list(&Root, Dir, &Vars, profile)?`, `hooks::run(&Run, &Hook, HookEnv) -> Outcome`, `exceptions::run(&Run, &Exceptions, Update | InstallOnly)?`.
P-05 selects, validates and persists the profile, machine or restore ID, history origin and `host-ID` branch, and recovery identity, checks bundle recipients and private readiness, then calls `history::restore(&Run, StageId, Restore { machine, origin, identity, initialize })` and `enroll(&Run, StageId, Enroll { profile, machine, origin, identity, recipients })` `-> Outcome`; only restore's `Outcome` feeds `DOTS_RESTORED` and its share of `DOTS_PRIVATE_READY` and hook secrets; P-06 re-derives nothing.
`history::checkpoint(&Run, StageId, &Ctx) -> Result<Saved | NothingTracked, Issue>`; `sync::load(&Ctx) -> LocalOnly | Problem | Ready` and `sync::run(&Run, StageId, Loaded, Display) -> Receipt` never fail; only shared runs checkpoint.
`Receipt` (P-09): checkout, revisions, paths, outcome, issues, no patches; only `sync::run` calls `receipt::emit`, once, to record and print it, warning if that fails; patches need standalone, non-`--plain` sync, terminal stdin and stdout, and [screening](contract.md#receipt).

## Harness (Q-01)

Q-01 builds the harness, translating only the contract, install and home suites; module tasks translate theirs, with per-suite counts.
`tests/SUITE.rs` scenarios are `unchanged_*` (v0.4.0 too) or `new_*` (e.g. cancellation statuses), ignored as `P-NN` until ported.
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
