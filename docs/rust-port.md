# Rust port architecture

S-02 decisions; [the contract](contract.md) wins wherever this note differs.
The oracle is v0.4.0 at `f5c8a0e`, and [T-01's decision](https://github.com/EzraCerpac/dots/blob/7555208ed116ce70402856cac9040d2bed20017f/spike/decision.md) lists terminal pitfalls.

## Toolchain and release

Root package `dots-cli`, binary `dots`, committed `Cargo.lock`, release `panic = "unwind"`, no `rust-version`; each dependency comes with its first user, at versions checked on 2026-11-18: clap 4.6.8, serde 1.0.229, serde_json 1.0.154, toml_edit 0.25.19+spec-1.1.0, nix 0.32.0, tempfile 3.27.0 (R-01); ureq 3.5.2 on rustls (P-01); ratatui 0.30.2, crossterm 0.29.0 (P-11, P-12).
`mise.toml` pins `rust = "1.100.0"`; its `--locked` tasks `check`, `test -- FILTER`, `build` and `clippy` run `rustup run 1.100.0 cargo`, never bare `cargo`, `format` pipes changed or named files through `rustfmt` on stdin, and `.cargo/config.toml` builds in the ignored `dist/target`.
`rust.yml` runs the suite, clippy and `cargo fmt --check` natively on both macOS and both static musl targets (musl C toolchain); Q-02 adds Fedora, Arch, Alpine and a first-run job required from I-01.
D-01 checks the Linux binaries are static and ships `dots-VERSION-{darwin,linux}-{amd64,arm64}.tar.gz` holding only `dots`, `SHA256SUMS`, and a formula installing completions.

## Modules and shared code

R-01 owns `main`, `cli` (the clap tree), `config`, `process`, `run`, `completion`, `skill`, and `root`, `profile`, `mise`, `completions`, help, version and configuration commands.
P-01 `packages`; P-02 `hooks`, `brew`, `inspect`, helpers; P-03 `init`; P-04 `update`; P-05 `bootstrap`; P-06 `history`; P-08 `sync`, accepted with P-06 integrated; P-09 `receipt`; P-10 `publish`; P-11 `supervisor`; P-12 `ui`.
Handlers are `fn(&Ctx, Args) -> Result<u8, Fail>`; an unported one exits 1 naming its task.

- `Root::resolve(&Env)` is physical; `Vars::read(&Root)` reads base `[vars] dots_*`, `mise.json(args)` effective settings; `selection::{read, persist}`; `Doc::open(path)?` edits the resolved file and `install()` swaps it in atomically, mode kept, unless changed.
- `RepoSetting::parse`, `matches(url)`, `source_mode` and `checkout_shape` are read-only, leaving URL checks, locks and Git writes to P-06 and P-08; `Exceptions::read` fails with status 2 on a bad row and answers `holds(request)` and `selected(profile)`.
- `Targets::read(&Root, Scope)`, the read-only removal inventory of `completion` and P-01, reads active in-root files or the selected file, and trusted app markers; `resolve(request, keep_installed) -> One | Ambiguous | None(suggestion)` applies aliases and holds.
- `ctx.command(program, Cwd::{Root, Caller, Dir})` takes OS-string arguments, drops `MISE_ENV` and hook variables, and sets `DOTS_ROOT`, `DOTS_PROFILE` and `PATH` led by a `dots` link to `current_exe()`; `Mise::new(&Ctx)?.command(args)` adds `MISE_CONFIG_DIR`, `MISE_ENV`.
- `BundlePipe::new(json)?.attach(&mut cmd)` gives only that hook a close-on-exec pipe's read end as `DOTS_BUNDLE_FD`, stdin intact, holding `{}` until private recovery is ready.
- `complete(Request { words, cursor, quote, shell, cwd, root }) -> Path | Dir | Values` feeds three shell adapters that keep native paths, quoting and text after the cursor, drawing on the clap tree, base profiles, configuration commands and `Targets` within [its limits](contract.md#dots-completions-bashzshfish).
- `skill::refresh(&Ctx)` extracts the embedded `skills/dots/SKILL.md` to a stable state path, links only what it recorded as its own, and warns once per foreign path; `init::TEMPLATES` embeds the contract-correct `init/` files and P-03's `src/init/{README.md,config.toml}`.

## Runs and reports (Lead T's interface)

`Run::begin(&ctx, Option<Command>)`, `None` when unreported, offers `stage(label)`, `stage_alongside(label)` for `--mas`, `run(id, cmd, Policy::{Ordinary, Interactive}) -> io::Result<ExitStatus>`, `probe(cmd) -> io::Result<Output>`, `change`, `issue`, `finish(id, Outcome)`, `cancelled()` and `end(status) -> u8`.
`Issue { diagnostic, next, tail, log }` needs `tail` and `log` in update stages; P-12 reads a `Snapshot` of each stage's ID, label, outcome, changes, issues and log from `run.snapshot()`, `report::list(command)` and `report::load(&Saved)`.
After Ctrl-C and cleanup, a valid `dots sync` returns 0 with its receipt, else `end` returns the foreground child's own nonzero status, else 130; v0.4.0 exits 130 for every handled interrupt.
R-01's working direct path gives children the caller's terminal and stdio and keeps the report in memory.
P-11 adds the report store, SIGINT handling and, for `up` alone on a large enough terminal without `--plain`, the dashboard with each stage on its own `openpty`; other commands stay direct.
A handoff relays the caller's terminal, which `dots` keeps, to the child's own PTY: it flushes stale input, resizes, relays bytes blindly, and logs nothing until the next drain.
Ctrl-C, even mid-drain, escalates on every stage group and probe, starts nothing, drains boundedly, restores termios, as does a group outliving its leader; self-detached groups are exempt.
Nested `bootstrap complete` runs direct and unreported in its stage's group.
`--mas` on macOS runs declared `mas` alongside, with cached `sudo`, `sudo -v` on a TTY, or deferral.

## Seams

- `hooks::list(&Root, Dir, &Vars, profile)?`, `hooks::run(&Run, &Hook, HookEnv) -> Outcome`, `exceptions::run(&Run, &Exceptions, Update | InstallOnly)?`.
- `history::checkpoint(&Run, StageId, &Ctx) -> Result<Saved | NothingTracked, Issue>`; `history::{restore, enroll}(…) -> Outcome`.
- `sync::load(&Ctx) -> LocalOnly | Problem | Ready` and `sync::run(&Run, StageId, Loaded, Display) -> Receipt` never fail; only shared runs checkpoint.
- `Receipt` (P-09): checkout, revisions, paths, outcome, issues, no patches; `receipt::emit` records and prints once, warning on record failure; patches need `Display::Standalone` on a terminal.
- `up` and `bootstrap complete` keep the contract's order; `up` exits 1 on a failed first checkpoint or invalid reload, sync issues defer, and the adopted revision is a change.

## Harness (Q-01)

Black-box `tests/SUITE.rs` scenarios are `unchanged_*` (v0.4.0 too) or `new_*`, ignored as `P-NN` until ported; `new_*` covers the cancellation statuses (P-11, P-08), the `plan` report and its `reports` completion (P-02, R-01) and `completions --help` (R-01).
The oracle is `DOTS_BIN=$PWD/bin/dots mise run test -- unchanged_ -- --include-ignored`.
`World::new()` runs children from `env_clear()` plus `PATH`, locale and `TERM`, private `HOME`, XDG, Git and JJ settings, and a fixture copy as `DOTS_ROOT`, skill links off unless `world.skill_links()`.
`DOTS_BIN`, absolute before any `chdir`, defaults to `CARGO_BIN_EXE_dots`; scenarios call `world.dots(&args) -> Output`, `world.git(&args)` and `world.bare_remote(name) -> String`.
Translating PRs report per-suite counts; P-07 maps injected failures to Rust coverage.

## Current files (D-01 deletes ported ones)

| Prefix | Disposition |
| --- | --- |
| root, `plan/` | Kept: AGENTS.md, REVIEW.md A-05; README.md A-02; .coderabbit.yaml, .gitignore, LICENSE, plan/ coordinator |
| `docs/` | Kept: contract.md Lead A; rust-port.md Lead B; sync.md A-05 |
| `.github/` | D-01 replaces it and `packaging/`; R-01 adds rust.yml |
| `bin/` | R-01, as are `completions/` and the kept, embedded `skills/` |
| `init/` | P-03; kept and embedded, but README.md |
| `lib/` | R-01 dots-manager.sh, mise-\*.sh, setup-common.sh; P-01 dots-\*add\*, dots-remove\*, dots-github.py; P-02 brew-exception.sh, retire-dotfiles.sh, dots-packages.sh, dots-hooks; P-08 source-\*.mjs, retired-\*.mjs; P-09 sync-receipt.mjs; P-11 dots-up-events.sh, dots-command-\* but status() (P-02); dropped quiet-probe.sh |
| `tasks/` | P-02 status, exceptions; P-03 init; P-04 update; P-06 backup, restore, README.md; P-10 publish.mjs |
| `bootstrap/` | R-01 profile; P-06 \*history\*; X-01 Atuin, Tailscale; P-05 the rest |
| `dots-up/` | R-01 cmd/, internal/commands/, internal/completion/; P-11 internal/supervisor/ but browse.go; P-12 that, internal/ui/, internal/launcher/; dropped go.mod, go.sum |
| `tests/` | Q-01 lib/, fixtures/ (kept), test_dots_contract.py, test_dots_home.py, test_manager_install.py, test_shell_completions.py; P-01 test_packages.py, test_dots_github.py; P-04 dots-up-\*.sh; P-05 test_fixture_config.py, test_dots_bootstrap_hooks.mjs; P-06 test_native_history.py; P-07 the other .mjs; X-01 test_atuin.py; A-05 test_docs_links.py; dropped run, test_syntax.py |
