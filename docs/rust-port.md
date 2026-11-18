# Rust port architecture

Status: S-02, for council; once frozen, each seam changes only through its owning lead.
[The contract](contract.md) wins wherever this note disagrees or is silent.

## Package, toolchain, release

Package `dots-cli`, binary `dots`, `rust-version = "1.100.0"`, `Cargo.lock` committed, release `panic = "unwind"` so terminal restoration runs.
Each dependency arrives with its first user: clap 4.6.8, clap_complete 4.6.12, serde 1.0.229, serde_json 1.0.154, toml_edit 0.25.19+spec-1.1.0, nix =0.32.0, tempfile 3.27.0; ureq 3.5.2 on rustls (P-01); ratatui 0.30.2, crossterm 0.29.0 (P-11, P-12).
R-01's `mise.toml` pins rust 1.100.0, the authoritative pin, with tasks `check`, `test` (forwards filters; the whole locked suite in CI), `build`, `format` (changed files by default) and `clippy`, all through `rustup run 1.100.0 cargo`, so no Homebrew `cargo` is picked up.
Builds stay in the worktree's `target/` and scenarios use temporary homes and repositories, so tasks need no lock; a host may wrap them in its own build limiter.
Release: `dots-VERSION-{darwin,linux}-{amd64,arm64}.tar.gz`, each `dots-VERSION-PLATFORM/bin/dots` and `LICENSE`, plus `SHA256SUMS`, built natively for `{x86_64,aarch64}-apple-darwin` and static `{x86_64,aarch64}-unknown-linux-musl`, then packaged by one later job.

## Modules and dispatch

| `src/` module | Task | Owns |
| --- | --- | --- |
| `main.rs`, `cli.rs`, `skill/`, `config/`, `process/`, `report/` | R-01 (B) | clap tree, flags, help, `--version`, `completions`, `root`, `profile`, `mise`, configuration commands, skill links; shared modules |
| `supervisor/` | P-11 (T); R-01 lands a plain-mode body | stages, PTY, cancellation, report store, `up --last` |
| `ui/` | P-12 (T) | menu, dashboard, issue view, `reports` |
| `packages/` | P-01 | `add`, `remove`, GitHub releases, `install-release-app` |
| `hooks/`, `brew/`, `inspect/` | P-02 | hook and recipe runner, `exceptions`, `retire-dotfiles`; `brew`; `status`, `plan`, `apply`, `test` |
| `init/`, `update/`, `bootstrap/` | P-03, P-04, P-05 | `init`; `up`; `bootstrap`, its callbacks, `bundle`, `remote` |
| `history/`, `sync/`, `receipt/`, `publish/` | P-06, P-08, P-09, P-10 (C) | `backup`, `restore`, `enroll-history`; sync; receipts; `publish` |

R-01 writes every `mod` line, one clap args struct per contract command, and one stub per handler that exits 1 naming its task.
A handler is `pub fn NAME(ctx: &Ctx, args: NameArgs) -> Result<u8, Fail>`; module tasks fill stubs and add files only in their own directory.
`Fail { status, line }` prints `dots: LINE` and exits `status`; `Ctx` carries global flags, caller cwd, `Root` and the caller's `MISE_ENV`.
An unknown name reaches the configuration-command lookup through clap's external subcommand, else exits 2.

## Shared modules (Lead B)

`config::root()` resolves and canonicalizes the root without reading it; `config::load(&Root)` requires `config.toml` (exit 2) and parses `dots_*` from it alone with toml_edit, never via mise.
Effective views (tools, dotfiles, packages, history entries) come only from mise in the selected environment, read by the module that needs them.
`selection::read(&Root)` returns the selection or the contract's ambiguity error; `selection::persist(&Root, &Selection, origin)` builds both files, validates the pair with mise, then installs each.
`edit::open(path) -> Doc` and `edit::install(path, Doc)` keep untouched bytes, follow a symlink to its referent, and rename a same-directory temporary with the referent's mode.
`process::Mise::new(&Ctx, &Config)` resolves `DOTS_MISE_BIN` (not executable: exit 2), `~/.local/bin/mise`, then `PATH`.
`mise.command(args) -> std::process::Command` takes OS-string arguments and sets cwd and `MISE_CONFIG_DIR` to the root, `MISE_ENV=PROFILE,host-ID` or none, `DOTS_ROOT`, `DOTS_PROFILE` and the self-`PATH`; `process::command(program, &Ctx)` sets the same minus mise variables.
The self-`PATH` prepends a private per-run directory whose `dots` links to the canonical `current_exe()`.
`process::require(program, action)` gives the one-line exit-1 failure for a missing program.
Configuration commands take a fast path: root and selection files only, caller cwd, no mise; only `bootstrap persist-selected` reads `Ctx`'s `MISE_ENV`.
`process::attach_bundle(&mut Command, json) -> BundleFd` opens a close-on-exec pipe, clears that flag only in this child's `pre_exec`, sets `DOTS_BUNDLE_FD`, and writes from a thread; stdin, arguments, environment and logs carry no value.
`report` holds `Change { item, before, after }` (`before: None` is "not installed"), `Issue { diagnostic, tail, next, log }`, `StageOutcome { Completed, Deferred, Skipped, Failed }` with `from_status` for 0/3/4/other, and `read_changes(path)` for `DOTS_CHANGES`.

## Stages and supervisor (Lead T)

`Run::begin(&ctx, command)` picks dashboard or plain and owns the caller's terminal; dashboard, plain output and store render the same report.
`start(label) -> StageId`, `run_child(id, Command, Policy) -> io::Result<ExitStatus>`, `probe(Command) -> io::Result<Output>`, `change(id, Change)`, `issue(id, Issue)`, `finish(id, StageOutcome)` and `cancelled()` take `&self`, so `up --mas` runs its App Store stage on a scoped thread.
The caller chooses each stage's outcome; child status and cancellation stay separate.
`end(status) -> u8` saves the report and returns `status`, or after cancellation the interrupted foreground child's status, else 130; `sync` always passes 0.
`Policy` is declared before spawn: `Ordinary` (dashboard: own PTY, manual view on request; plain: caller's stdin, even a pipe), `Interactive` (a step `dots` knows prompts, like its own `sudo -v`: automatic handoff), `Alongside` (null stdin, never handed off); prompts are never guessed.
One process uses `nix::pty::openpty` with no event protocol, as T-01 showed on macOS and Linux; reports and logs live under `${XDG_STATE_HOME:-~/.local/state}/dots/`, private.
Spike pitfalls to keep:

- PTY descriptors close-on-exec; the parent's slave closed right after spawn.
- Each child leads its own session and group; record the group before signalling and keep the leader unreaped through SIGINT, SIGTERM, SIGKILL.
- Ctrl-C cancels every active group and starts no later stage; at stage end surviving group members are retired, helpers in their own session or group are not.
- Relay prompt bytes unchanged, flush queued keys before a handoff, and keep echo-off input out of every sink: log, tail, plain output, dashboard.
- Resize sets the child's size, defers the footer mid escape or UTF-8 sequence, and clears margins on tiny terminals.
- EIO is EOF; retry EINTR and short writes; bound the final drain; truncate a stalled log with a marker.
- Restore termios in a guard independent of display writes on return, cancellation and unwinding.

## Seams between modules

`hooks::list(dir, profile, &Config)` applies name, order and `@PROFILE` rules and fails whole on an unknown suffix.
`hooks::run(&Run, &Hook, HookEnv) -> StageOutcome` runs every hook and recipe as its own stage, with a private `DOTS_CHANGES` read even after failure, extra variables and arguments, and an optional `BundleFd`.
`hooks::exceptions(&Run, &Config, mode)` reads `exceptions.tsv` and runs selected recipes for `exceptions`, `up` and `bootstrap complete`.
`history::checkpoint(&Run, id, &Config)` is skipped when nothing is tracked; `up` calls it first and last, `sync` as step 1.
`history::restore(…, Restore)` and `history::enroll(…, Enroll)` serve their commands and `bootstrap complete`; exit 3 becomes `Deferred`.
`sync::run(&Run, id, &Config) -> Receipt` never fails; guarded apply and retired declarations stay inside `sync`, every problem an `Issue` in the receipt.
`Receipt` (Lead C) is metadata only: mode, revisions, path groups, push, adopted revision, issues; `up` maps its problems to `Deferred` and the adopted revision to a `Change`.
Standalone `dots sync` runs it, prints and records the receipt, and exits 0 for valid arguments, a missing configuration or mise included.
`update` calls `history`, `sync` and `hooks` around its own package and tool stages; `bootstrap::complete` calls `hooks::exceptions`, `history::restore`, `history::enroll`, then each bootstrap hook through `hooks::run` with the bundle.
P-09 owns `receipt::record`, `sync --last` replay and patches screened at display time; no durable type holds patch bytes.

## Embedded files

P-03 derives `assets/init/{config.toml,README.md,exceptions.tsv,retired-dotfiles,retired-declarations,gitleaks.toml,gitignore}` from `init/`, which 0.3.5 keeps until D-01.
`init::TEMPLATES` lists `Template { dest, mode: 0o644, bytes: include_bytes!(…) }` for `config.toml`, `dots/README.md`, `dots/exceptions.tsv`, `dots/retired-{dotfiles,declarations}`, `.gitleaks.toml`, `.gitignore`; the seed commit compares against it.
R-01 embeds a copy, `assets/skill/SKILL.md`, with `include_str!`; A-05 rewrites it.
Each run writes it atomically, when changed, to `${XDG_STATE_HOME:-~/.local/state}/dots/skills/dots/SKILL.md` and links `skills/dots` per the contract's Agent skill rules.
Owned links stay recorded as `LINK<TAB>TARGET` in `dots/skill-links`, notices in `dots/skill-notices`, the 0.3.5 formats, so 0.3.5 links are repointed.

## Harness (Q-01)

One `tests/SUITE.rs` per suite, helpers in `tests/support/`, driving only the executable.
`DOTS_BIN` is made absolute before any scenario changes directory; it defaults to `env!("CARGO_BIN_EXE_dots")`.
`World::new()` gives a temporary `HOME` and XDG directories, a copy of `tests/fixtures/config`, Git and JJ isolated with a test identity, `MISE_ENV` and `DOTS_*` cleared, skill links off, and a cwd inside it.
`world.bare_remote(name)` returns a `file://` URL; `world.dots(args)` returns the output.
Names start `unchanged_` or `new_`; `DOTS_BIN=$PWD/bin/dots mise run test unchanged_ -- --include-ignored` runs the oracle on 0.3.5.
`tests/scenarios.tsv` maps every old scenario to its Rust test or drop reason; a test checks the names and prints per-suite unchanged, new and dropped counts.
Module-injected failures become fake programs on `PATH`, else `new_` unit tests; P-07 lists them.
Old counts: contract 54, install 20, home 4, docs links 1, packages 10, GitHub 2, fixture 3, history 2, bootstrap hooks 8, sync 99, receipt 9, publish 7, retired 11, shell 2, Go supervisor 29, Go UI 49; Atuin 3 and syntax 1 drop.
Green by phase: R-01 unit and golden edits; Q-01 its three suites on 0.3.5, R-01's scenarios on Rust, the rest `#[ignore = "P-NN"]`; each module translates first, proves `unchanged_` on 0.3.5, then removes its ignores; I-01 none left.

## Current files

| Files | Task or drop |
| --- | --- |
| `bin/dots`, `lib/{dots-manager.sh,mise-bin.sh,mise-config.sh,setup-common.sh}`, `bootstrap/profile`, `skills/dots/SKILL.md` | R-01 (A-05 rewrites the skill); `DOTS_HOME`, `internal`, renderer selection, `atuin-*` drop |
| `lib/{dots-up-events.sh,dots-command-events.sh,dots-command-backend,dots-command-backend.py}` | P-11; event protocol drops |
| `lib/quiet-probe.sh` | Drops: scripts capture in their own language |
| `lib/{dots-add-common.sh,dots-add-config.py,dots-package-add.sh,dots-tool-add.sh,dots-remove.py,dots-remove-packages.py,dots-github.py}`, `tests/test_{packages,dots_github}.py` | P-01 |
| `lib/{brew-exception.sh,retire-dotfiles.sh,dots-hooks,dots-packages.sh}`, `tasks/{status,exceptions}` | P-02 |
| `tasks/init`, `init/{README.md,exceptions.tsv,gitignore,gitleaks.toml,retired-declarations,retired-dotfiles}` | P-03 |
| `tasks/update`, `tests/dots-up-{managed-events,snapshots}.sh` | P-04 |
| `bootstrap/{launch,persist-selected,complete.mjs,bundle.mjs,remote,common.sh}`, `tests/{test_fixture_config.py,test_dots_bootstrap_hooks.mjs}` | P-05; private files, `GH_TOKEN`, fixed keys drop |
| `tasks/{backup,restore,README.md}`, `bootstrap/{enroll-history,history-permissions.sh}`, `tests/test_native_history.py` | P-06 |
| `tests/test_{source_dev,sync_receipt,dots_publish,retired_declarations}.mjs` | P-07 |
| `lib/{source-repo.mjs,source-dev.mjs,source-apply.mjs,retired-declarations.mjs}`; `lib/sync-receipt.mjs`; `tasks/publish.mjs` | P-08; P-09; P-10 |
| `dots-up/internal/supervisor/{run,stream,report}{,_test}.go`, `dots-up/internal/supervisor/{handoff,manual_terminal}_test.go` | P-11 |
| `dots-up/internal/{ui,launcher,commands}/*.go`, `dots-up/internal/supervisor/browse.go`, `dots-up/cmd/dots-up/main{,_test}.go` | P-12; `--capabilities`, `DOTS_UP_BIN`, `--backend` drop |
| `dots-up/{go.mod,go.sum}`, `tests/{run,test_syntax.py}` | D-01 deletes: no Go or shell sources |
| `bootstrap/{atuin-bundle,atuin.py,enroll-tailscale,tailnet}`, `tests/test_atuin.py` | Drop from core; X-01 moves them to Ezra's configuration |
| `tests/test_{dots_contract,manager_install,dots_home,docs_links}.py`, `tests/lib/prereq.{py,mjs}`, `tests/fixtures/config/**` (every file, including the hidden `tests/fixtures/config/dotfiles/.config/example/settings.toml`) | Q-01; fixtures unchanged |
| `packaging/build-release`, `packaging/homebrew/dots.rb`, `.github/workflows/{test,release}.yml` | D-01, tap step C-01; R-01 adds `rust.yml` |
| `.gitignore`; `README.md`; `docs/sync.md`, `REVIEW.md`, `AGENTS.md` | R-01 adds `/target/`, D-01 drops Go lines; A-02; A-05 |
| `docs/{contract,rust-port}.md`, `plan/**`, `LICENSE`, `.coderabbit.yaml` | Kept |
