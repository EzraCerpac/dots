# Rust port architecture

S-02 decisions; [the contract](contract.md) wins wherever this note disagrees or is silent.

## Toolchain and release

Crate `dots-cli`, binary `dots`, `rust-version = "1.100.0"`, release `panic = "unwind"`.
Dependencies, newest on 2026-11-18, come with their first user: clap 4.6.8, clap_complete 4.6.12, serde 1.0.229, serde_json 1.0.154, toml_edit 0.25.19, nix 0.32.0, tempfile 3.27.0; ureq 3.5.2 on rustls (P-01); ratatui 0.30.2, crossterm 0.29.0 (P-11, P-12).
`mise.toml` pins `rust = "1.100.0"`, sets `CARGO_TARGET_DIR` to the ignored `dist/target`, and has `--locked` tasks `check`, `build`, `clippy`, `test` (`mise run test -- ARGS` reaches `cargo test`) and `format` (changed `.rs` files or given paths; `--check`).
`rust.yml` runs them natively for `{x86_64,aarch64}-apple-darwin` and static `{x86_64,aarch64}-unknown-linux-musl`; D-01 ships `dots-VERSION-{darwin,linux}-{amd64,arm64}.tar.gz`, each holding only `dots`, with `SHA256SUMS` and an updated formula.

## Modules

R-01 owns `main`, `cli`, `config`, `process`, `report`, `skill`; P-01 `packages`; P-02 `hooks`, `brew`, `inspect`; P-03 `init`; P-04 `update`; P-05 `bootstrap`; P-06 `history`; P-08 `sync`; P-09 `receipt`; P-10 `publish`; P-11 `supervisor`; P-12 `ui`.
R-01 writes the clap tree and each signature below.
Until a task lands, its handlers exit 1 naming it, its functions return an issue naming it, and scenarios needing them stay `#[ignore = "P-NN"]`.
A handler is `fn(&Ctx, Args) -> Result<u8, Fail>`, where `Ctx` holds the global flags, the caller's cwd and `MISE_ENV`, and the `Root`, and `Fail { status, line }` prints `dots: LINE`.
R-01's shared code:

- `config::root(env) -> Root` resolves physically, keeping a missing tail for `init`; `config::load(&Root) -> Result<Config, Fail>` needs `config.toml` and reads `dots_*` with toml_edit.
- `config::selection::{read, persist}`; `config::edit::{open, install}` keep untouched bytes, follow symlinks and keep the mode.
- `config::repo`: `Repo::parse`, `matches(url)`, `url()`, `effective_urls(git_dir, remote)`, `source_mode(&Root, &Config) -> LocalOnly | Shared | Missing | Mismatch`, and `shape(&Root) -> Vec<Issue>` for checkout-shape stops.
- `process::command(&Ctx, program, Cwd)` drops inherited `MISE_ENV` and `DOTS_BUNDLE_FD`, sets `DOTS_ROOT` and `DOTS_PROFILE`, and prepends a per-run directory whose `dots` links to `current_exe()`.
  Configuration commands run in the caller's cwd, keep a caller's `DOTS_PROFILE` and start no mise; other children run in the root.
- `Mise::new(&Ctx)` finds mise; `mise.command(args)` adds `MISE_CONFIG_DIR` and `MISE_ENV=PROFILE,host-ID`.
  `dots mise` adds `MISE_AUTO_INSTALL=0`; it and configuration commands inherit stdio and exit with the child's code, or 128 plus its signal.
- `process::attach_bundle(&mut Command, json)` gives that command alone a pipe, as `DOTS_BUNDLE_FD`, fed by a thread.
- `skill` embeds `skills/dots/SKILL.md`, writes it when changed to `skills/dots/SKILL.md` under `${XDG_STATE_HOME:-~/.local/state}/dots/` and links each agent's `skills/dots` there.
  It keeps 0.3.5's `skill-links` (`LINK<TAB>TARGET`) and `skill-notices` in that directory, repointing recorded links whose parent stays inside the agent directory.
- `report`: `Change { item, before: Option, after }`, `Issue { diagnostic, tail, next, log }`, `StageOutcome { Completed, Deferred, Skipped, Failed }`, `read_changes(path)`; `StageOutcome::from_hook` maps 0, 3, 4 for hooks and recipes only.

## Supervisor (P-11)

One process, `nix::pty::openpty`, no event protocol.
A report command calls `Run::begin(&ctx, command)` once to pick dashboard or plain, own the terminal and save the report; nested code gets that `&Run` and a `StageId`, and `bootstrap complete`, inside the native-bootstrap stage, runs plain without a report.
On `&self`: `start(label)`, `start_alongside(label)`, `run_child(id, Command, Policy) -> io::Result<ExitStatus>`, `probe(Command) -> io::Result<Output>`, `change`, `issue`, `finish(id, StageOutcome)`, `cancelled()`.
`Policy::Ordinary` gets a PTY the dashboard shows on request, or the caller's stdin in plain output; `Policy::Interactive`, a step known to prompt such as `sudo -v`, gets the terminal itself.
An alongside stage (`--mas`) runs on a scoped thread with null stdin in plain output and no handoff; `up` runs `sudo -v` first.
Ctrl-C cancels every child's group, probes included, and starts no later stage.
`end(status) -> u8` saves the report and returns `status`, or after cancellation the interrupted foreground child's exit code, else 130; `dots test` passes its child's code.
Echo-off input, even across a view change, reaches no log, tail or report; a termios guard covers return, cancellation and unwinding; T-01's `spike/decision.md` (`7555208`) lists the other pitfalls.

## Seams

- `hooks::list(dir, &Config, profile) -> Result<Vec<Hook>, Fail>`; `hooks::run(&Run, &Hook, HookEnv { vars, args, bundle }) -> StageOutcome`, one stage, private `DOTS_CHANGES`, bundle on its command only; `hooks::exceptions(&Run, &Config, Update | InstallOnly) -> bool`.
- `history::checkpoint(&Run, StageId, &Config) -> Result<Saved | NothingTracked, Issue>`; `history::{restore, enroll}(&Run, StageId, options) -> StageOutcome`.
- `sync::load(&Ctx) -> LocalOnly | Problem(Issue) | Ready` and `sync::run(&Run, StageId, Loaded) -> Receipt` never fail: missing configuration, mise or gitleaks, local-only mode and cancellation become receipt lines.
  `run` calls `receipt::emit` once: record, then print, warning if recording failed.
  `Receipt` (P-09) exposes `issues()` and `adopted()`.
  `dots sync` is begin, `run(load)`, `end(0)`, exit 0.
- `up`: `checkpoint`, whose issue exits 1 before any update; `sync::load`, skipping with one line when `LocalOnly`, else `sync::run` with issues `Deferred` and the adopted revision a `Change`; `config::load`; packages; tools; `exceptions(Update)`; hooks; standalone mise; the manager line; `checkpoint`.
- `init::TEMPLATES` (`{ dest, bytes }`, mode 0644, the seed commit's reference) embeds the `init/` files, plus `config.toml` and other 1.0-only ones from `src/init/`.
- `bootstrap complete`: private inputs; source repository; `history::restore` with `--restore`; `exceptions(InstallOnly)`; each hook with `DOTS_PRIVATE_READY`, `DOTS_RESTORED` and the secrets; `history::enroll` with `dots_state_repo` or `SETUP_HISTORY_ORIGIN`.

## Harness (Q-01)

`tests/SUITE.rs` holds black-box scenarios driving only `DOTS_BIN` (default `CARGO_BIN_EXE_dots`); unit tests stay in `src/`.
`World::new()` isolates `HOME`, XDG, Git and JJ, clears `MISE_ENV` and `DOTS_*`, and copies `tests/fixtures/config`.
Scenarios are `unchanged_*` (0.3.5 and Rust) or `new_*` (Rust); the oracle is `DOTS_BIN=$PWD/bin/dots mise run test -- unchanged_ -- --include-ignored`.
Each translating task writes `tests/inventory/SUITE.tsv`, `OLD<TAB>RUST_TEST` or `OLD<TAB>-<TAB>REASON` per scenario; `tests/inventory.rs` checks the names and prints counts.

## Current files

- `bin/dots` R-01; `skills/` R-01 and `init/` P-03, which stay when D-01 deletes the rest.
- `lib/`: `dots-manager.sh`, `mise-*.sh`, `setup-common.sh` R-01; `dots-*-events.sh`, `dots-command-backend*` P-11; `dots-*add*`, `dots-remove*`, `dots-github.py` P-01; `brew-exception.sh`, `retire-dotfiles.sh`, `dots-hooks`, `dots-packages.sh` P-02; `source-*.mjs`, `retired-declarations.mjs` P-08; `sync-receipt.mjs` P-09; `quiet-probe.sh` dropped.
- `tasks/`: `status`, `exceptions` P-02; `init` P-03; `update` P-04; `backup`, `restore`, `README.md` P-06; `publish.mjs` P-10.
- `bootstrap/`: `profile` R-01; `enroll-history`, `history-permissions.sh` P-06; `atuin*`, `enroll-tailscale`, `tailnet` X-01; the rest P-05.
- `dots-up/`: `internal/supervisor/` but `browse.go` P-11; the other Go files P-12; `go.*` dropped.
- `tests/`: `test_{packages,dots_github}.py` P-01; `dots-up-*.sh` P-04; `test_fixture_config.py`, `test_dots_bootstrap_hooks.mjs` P-05; `test_native_history.py` P-06; the other `test_*.mjs` P-07; `test_atuin.py` X-01; `run`, `test_syntax.py` dropped; the rest, `fixtures/` unchanged, Q-01.
- `packaging/`, `.github/workflows/{test,release}.yml`: D-01.
