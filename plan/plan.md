# dots: adoption and the Rust rewrite

Status: Planning; decisions recorded; implementation not started

Generated snapshot from `plan/plan.json`; edit the source and regenerate.

## Outcome

**Outcome.** Someone who has never seen Ezra's setup can find `dots`, install one binary next to mise, and manage a machine in minutes. `dots` 1.0.0 is that binary, written in Rust.

**First useful version: the release candidate** (milestone M3). Ezra's machines run the Rust binary and the old implementation is gone from the repository.

**Public release: 1.0.0** (milestone M4), with the new README, the demo, the template repository and the listings.

Requirements drop from mise, Bash, Git, Node, uv, jq, gitleaks, age and JJ to mise, Git, gitleaks and age. Bash is needed only by a configuration's own hooks, and JJ and `gh` only by `dots publish`.

**Not in this plan.** The launch post (Ezra's, after 1.0.0); native Windows (WSL only); Homebrew core; a self-updater; replacing Git, gitleaks, mise, `gh` or JJ with libraries; any setting beyond those the tasks name.

## Current state

**Verified** in this checkout at `44b626e`, on GitHub and in Ezra's configuration on 2026-11-18:

- About 26,000 lines in five languages: Bash, Python (through uv), Node, Go and TOML.
- The repository is public with 0 stars, no topics, no homepage and Discussions off. The latest release is v0.3.5.
- `dots up` syncs first, and sync refuses a checkout without a GitHub `origin` named as `OWNER/NAME` (`lib/source-repo.mjs:34` and `:73`).
- Bootstrap completion always runs an Atuin stage and a Tailscale stage (`bootstrap/complete.mjs:178` and `:220`).
- `dots init` (`tasks/init`) writes `[vars] dots_profiles` and the `dots/` layout. Selecting the profile is a separate `dots internal profile`.
- `dots add` handles tools and packages only; nothing adopts an existing dotfile.
- Of 15 suite files, the sync, receipt, publish, bootstrap-hook, retired-declaration, GitHub and Atuin suites import implementation modules. The contract, install, home and package suites drive `bin/dots`.
- CI runs on `ubuntu-latest` and `macos-latest` only.
- `REVIEW.md` tells reviewers that `dots` is run by one person on their own machines.
- On crates.io the name `dots` is taken; `dots-cli` is free.
- Ezra's configuration is the only consumer of the contract. It sources `lib/brew-exception.sh`, `lib/setup-common.sh` and `lib/retire-dotfiles.sh`, calls `dots internal home`, and uses Atuin and Tailscale.
- T3 offers `gpt-6.2-astra`, `gpt-6.2-sol` and `claude-opus-5-5`. Grok is disabled, so a council has two substantive members.

**Reported** by the Astra research and not re-verified: the per-language line counts (15,526 implementation, 9,093 tests), current crate versions, 14 to 26 working weeks for a one-cutover port by one maintainer with agents, and the adoption statistics.

**Unknown:**

- Which sync scenarios cannot run against the current implementation without changing it, because they inject failures through imported modules.
- Whether a PTY library gives the supervisor resize, Ctrl-C and an interactive `sudo` handoff on both platforms. T-01 answers this.
- Whether an Astra lead can start Opus children; the capability is reported but not exercised. K-02 tests it.

## Approach

Ezra is the only user, so intermediate releases, fallbacks and compatibility windows protect nobody. Three rules follow.

1. **One rewrite, one cutover.** The Rust code is added beside the current implementation, which stays untouched and releasable until the cutover deletes it. There is no dispatcher into old scripts and no fallback switch.
2. **Contract first.** `docs/contract.md` describes 1.0, including every first-run change. New behaviour is implemented once, in Rust. The contract drops whatever only served the old implementation: the renderer handshake, `DOTS_UP_BIN`, the sourced shell libraries and old receipt formats.
3. **The current implementation is the oracle.** The acceptance suite is written once, in Rust, and drives whichever executable `DOTS_BIN` names. Each scenario is tagged unchanged or new. An unchanged scenario must pass on the current implementation before the module that ports it is accepted.

Each module follows the same steps: translate its scenarios into the harness, run the unchanged ones against the current implementation, implement, run all of them against the Rust binary.

Risk still sets the attention. Sync and restore get xhigh effort, a five-round sol loop and a council, because they can lose data. The PTY supervisor is proven in a spike before the architecture is fixed.

The Astra research recommended against a rewrite on cost and adoption evidence. Ezra chose it; this plan keeps the research's list of what stays an external program.

No task carries a duration: there is no measured baseline for agent teams on this code.

## Target architecture and ownership

- **One Cargo package**: crate `dots-cli`, binary `dots`. Modules: `config`, `process`, `report`, `packages`, `update`, `bootstrap`, `history`, `sync`, `receipt`, `publish`, `supervisor` and `ui`. A second crate appears only when a second consumer exists.
- **One self-contained binary.** The `dots init` templates and the agent skill are embedded. There is no support directory, so `cargo install`, Homebrew and mise all install the same single file.
- **No sourced shell library.** The recipe helpers in `lib/brew-exception.sh`, the dotfile retirement script and what scripts need from `lib/setup-common.sh` become subcommands, so a recipe or hook can be written in any language. S-01 fixes their names; X-01 migrates Ezra's configuration.
- **No personal features in core.** Atuin and Tailscale enrollment move into Ezra's configuration as hooks and commands. Core gains one generic mechanism: values from the encrypted bootstrap bundle reach bootstrap hooks.
- **Supervisor.** The default is one process that runs each stage's child under a PTY and reports in-process, with no event protocol between two programs. S-02 confirms or changes this after the spike.
- **Crates** (Astra's shortlist, confirmed in S-02): clap, serde_json, toml_edit, ureq, nix, and ratatui with crossterm.
- **External programs that stay**: mise, Git, gitleaks, age, `gh`, JJ and the native package managers. Sync keeps calling the Git executable, so its merge, lock and credential behaviour does not change.

| Shared thing | Single writer |
| --- | --- |
| `docs/contract.md` and the command-line surface | Lead A; frozen after the S-01 council, changes go through the coordinator |
| `Cargo.toml`, the command tree, the `config`, `process` and `report` modules, and the acceptance harness | Lead B |
| Sync and receipt types | Lead C |
| Stage and report interface between commands and the supervisor | Lead T |
| `rust.yml` | Lead B; the existing workflows stay untouched until D-01 |
| `plan.json` | Coordinator |

Every task is one pull request from its own worktree, into `main`. That is safe before the cutover because the Rust code only adds files and releases still build the current implementation. A lead sets the merge order for its workstream and serializes history-changing commands in the repository.

## Agents: who does what

| Role | Model and effort | Does | Does not |
| --- | --- | --- | --- |
| Coordinator | Fable 5.2, this thread | Holds `plan.json`, starts each lead, approves gates and tags, performs outward-facing actions | Implement; get spawned as a subagent |
| Lead A: contract, adoption, docs | Astra 6.2, high | Writes the contract, starts the Opus implementers for wording, README, demo, template and docs | Write feature code |
| Lead B: Rust core | Astra 6.2, high; xhigh for S-02 | Owns the architecture, the Cargo package, the harness, the package, update and bootstrap modules, integration and packaging | Write feature code beyond integration fixes |
| Lead C: data paths | Astra 6.2, xhigh | Owns history, sync, receipt and publish | Merge a data-path module without a council |
| Lead T: terminal | Astra 6.2, xhigh | Owns the PTY spike, the supervisor and the dashboard | |
| Taste implementer | Opus 5.5, high | README, demo, template repository, `dots init` flow, help and error wording, dashboard, docs and the agent skill | |
| Implementer | Sol 6.2, high; xhigh on data paths and the supervisor | Modules, scenario translation, CI, packaging | |
| Native subagents | Sonnet and Haiku under Opus; Codex subagents under Sol | Routine implementation, search, inventories, log triage | Fable is never spawned |

**Review on every pull request.** The implementer runs a sol loop before asking its lead to merge: a fresh Sol 6.2 delegate at high effort with no history, three rounds by default and five on data-loss and state paths. `REVIEW.md` and CodeRabbit apply as they do today. The lead checks each finding the implementer set aside, then merges.

**Council on larger things.** The owning lead runs one on the contract (S-01), the architecture note (S-02), history (P-06), sync (P-08) and the supervisor (P-11); the coordinator runs one on the whole repository at G-1. Members are Opus 5.5 and Sol 6.2. Grok 4.7 is disabled in this T3 setup; per the council skill it is reported as unavailable and not replaced.

**Hard diagnosis** goes to the owning lead at xhigh.

**Gates and outward-facing actions.** The coordinator approves gates and tags and performs repository settings, the template repository, the listing pull requests and the crate publication. Ezra supplies what only he can: the go-ahead, two tokens and a week of use. Prose posted on GitHub starts with the disclosure note naming the model.

**When.**

| Wave | Starts when | Running in parallel |
| --- | --- | --- |
| 0 | Ezra's go-ahead | Coordinator: K-02 and A-01 |
| 1 | K-02 passes | Lead A: S-01. Lead T: T-01. Then Lead B: S-02 |
| 2 | S-02 frozen | Lead B: R-01, then Q-01 and Q-02 |
| 3 | Q-01 merged | Lead B: P-01 to P-05. Lead C: P-06 to P-10. Lead T: P-11 and P-12. Lead A: README draft. About ten implementers at the peak |
| 4 | All modules merged | Lead B: I-01, then D-01. Lead A: A-02, A-03 and A-05. X-01 in Ezra's configuration. Then G-1 |
| 5 | G-1 | A-04 and C-01, a week of use, G-2, then A-06 |

## Brief for each lead

The coordinator starts a lead with `delegate_task` and this brief; a child gets no history, so the brief carries everything.

- **Read first**: this plan, `docs/contract.md` and `docs/rust-port.md` once they exist, `REVIEW.md` and `AGENTS.md`.
- **You own**: the listed task IDs and the shared things the ownership table gives you.
- **You may start**: Opus 5.5 at high effort for taste and Sol 6.2 at high effort for implementation (xhigh where the plan says so), each with `delegate_task` in worktree isolation, one task and one pull request per child. Tell each child it may use its own native subagents and must not start Fable.
- **Each child must**: keep to its task's acceptance criteria, leave the current implementation untouched, run the tests, run a sol loop, open a pull request with the disclosure note, and end with the pull request URL, head SHA, and the state of checks and reviews.
- **You must**: freeze a shared interface before two children depend on it, set the merge order, merge task pull requests once CI, the sol loop and `REVIEW.md` pass, run the councils the plan names, and push back on review findings that widen scope.
- **You must not**: tag a release, change repository settings, create a repository or post outside this repository. Report that the gate is ready.
- **Return**: per task the pull request, the sol-loop ledger and the council scores; then open questions and anything that needs a replan.

## Decisions

**Decided by Ezra on 2026-11-18.**

- One rewrite and one cutover; no incremental port.
- Atuin and Tailscale leave core, and Ezra's configuration is updated to match.
- Leads merge task pull requests; the coordinator approves gates and tags.
- Adopting a dotfile is `dots add FILE`.
- The template repository is `EzraCerpac/dots-template`.
- The plan is committed in `plan/` on this branch, so children that start from a commit can read it.

**Settled by this plan.** Say so to change one.

- Atuin and Tailscale move into Ezra's configuration, not into an `extras/` directory here: nobody else uses them, and the repository stays one language.
- The sourced shell libraries become subcommands (confirmed in S-01).
- Git, gitleaks, age, mise, `gh` and JJ stay external programs.
- The crate is `dots-cli` because `dots` is taken on crates.io; the binary stays `dots`.
- `dots publish` stays in core, documented apart from the first run.
- The cutover ships as `1.0.0-rc.1`; 1.0.0 follows a week of use.
- Listings go out with 1.0.0.
- Native Windows is out; WSL is the supported route.

**Open.** Nothing blocks the start except the go-ahead (K-01).

## Risks

| Risk | Mitigation | Owner |
| --- | --- | --- |
| Modules built in parallel do not fit together | R-01 freezes the command tree and shared interfaces first; every module merges to `main` under one CI; I-01 is a task, not an afterthought | Lead B |
| The sync rewrite loses or leaks data | Scenarios translated first (P-07) and run against the current implementation; xhigh; five-round sol loop; council; a week of daily use before 1.0.0 | Lead C |
| The oracle is weaker than it looks, because a scenario is tagged new when it is really unchanged | The tag list is part of each module's review, and the G-1 council reads it whole | Leads B and C |
| Terminal handoff, signals or `sudo` do not work in the Rust supervisor | The spike (T-01) runs before the architecture is fixed | Lead T |
| The simplified contract breaks Ezra's machines at cutover | X-01 migrates the configuration and is tested against the release candidate; the configuration pins the `dots` version, so reverting one pull request restores 0.3.5 | Coordinator |
| `toml_edit` formats differently from `tomlkit` | Golden files generated from the current implementation on the fixture configuration | Lead B |
| Parallel agents collide in shared files | The ownership table; one task, one worktree, one pull request | Each lead |
| Reviews grow the scope | The sol loop's smallest-change rule; leads decline nits in one line | Each lead |

## Acceptance and rollout

**G-1, the cutover**, passes when the whole suite is green against the Rust binary on macOS, Ubuntu and the distribution containers, the old implementation is deleted, the council is satisfied, and Ezra's configuration runs the release candidate.

**G-2, 1.0.0**, passes when Ezra has used the release candidate for a week, including daily sync on two machines and one restore, and the README, demo, template repository and crate are ready.

**Recovery** until 1.0.0: revert the pull request in Ezra's configuration that pins the release candidate. The configuration then runs 0.3.5 with its old hooks again.

## Work packages

| ID | Work / deliverables | Depends on / external prerequisites | Owner / acceptance | Phase / workstream |
| --- | --- | --- | --- | --- |
| K-01 [gate] | Go-ahead to start; outputs: Ezra's word to start the work this plan describes | none; external: none | Ezra; Ezra has said to start | 0 Kickoff / Decisions |
| K-02 | Orchestration smoke test; outputs: One Astra 6.2 lead that starts one Opus 5.5 child and one Sol 6.2 child in worktree isolation on a no-op task | K-01; external: none | Coordinator (Fable 5.2); Each child reports its model and a checkout path different from the lead's; Each child can read plan/plan.md; If nested delegation fails, the agent topology is replanned before any work starts | 0 Kickoff / Decisions |
| A-01 | Repository metadata; outputs: Topics: dotfiles, dotfiles-manager, mise, bootstrap, machine-setup, macos, linux, homebrew, cli, tui, rust; A description that names mise; Discussions enabled | K-01; external: none | Coordinator (Fable 5.2); gh repo view shows the topics and description; Discussions is on | 0 Kickoff / Adoption |
| S-01 | Write the contract for 1.0; outputs: docs/contract.md: configuration layout, [vars] keys, hook and command conventions, environment variables, exit codes and the full command list; The first-run behaviour: local-only mode, any Git remote, the init flow, dots add FILE, the prerequisite check; The subcommands that replace the sourced shell libraries, and how bundle values reach bootstrap hooks; A list of what 0.3.5 has and 1.0 drops, each with the reason | K-02; external: none | Lead A: Astra 6.2 (high); Opus 5.5 reviews the wording; council; Everything under Configuration in README.md and in init/README.md is either covered or on the dropped list; Each use of the manager in Ezra's configuration has a 1.0 equivalent; Both council members are satisfied; the document is marked frozen | 1 Design / Contract |
| T-01 | Spike: PTY, terminal handoff and signals in Rust; outputs: A throwaway prototype: a child under a PTY with resize, Ctrl-C, an interactive sudo handoff and terminal restoration; A written choice of PTY crate, and whether one process can supervise without an event protocol | K-02; external: none | Lead T: Astra 6.2 (xhigh), with one Sol 6.2 implementer; The cases in the Go renderer's manual terminal test pass on macOS and Linux; The prototype is not merged | 1 Design / Terminal |
| S-02 | Architecture note; outputs: docs/rust-port.md: module boundaries, the interfaces between them, crates checked against current releases, the supervisor design, embedded templates and skill, the harness design and the release layout; Every current file mapped to a module task or to the dropped list | S-01, T-01; external: none | Lead B: Astra 6.2 (xhigh); council; Both council members are satisfied; No current file is unmapped; The note fits on two pages | 1 Design / Rust core |
| R-01 | Cargo package, command tree, shared modules and Rust CI; outputs: Crate dots-cli building a dots binary with every command of the contract present and unimplemented ones failing clearly; config (effective settings through mise, profile selection, comment-preserving edits installed atomically), process (argument-array execution with the MISE_* rules) and report (stages, changes, issues); Global flags, DOTS_ROOT resolution, version, help, configuration commands, skill links and completions for bash, zsh and fish; rust.yml running fmt, clippy and tests for the four release targets | S-02; external: none | Sol 6.2 (high) under Lead B; Opus 5.5 writes help and error text; Edits to the fixture configuration match golden files from the current implementation; dots help matches the contract's command list; No existing file or workflow changes | 2 Foundation / Rust core |
| Q-01 | Acceptance harness; outputs: Rust integration-test helpers: a private HOME, the fixture configuration, local bare remotes, isolation from the caller's Git and JJ settings; DOTS_BIN selecting the executable under test, and a tag marking each scenario unchanged or new; The contract, install and home suites translated as the first users | R-01; external: none | Sol 6.2 (high) under Lead B; With DOTS_BIN set to bin/dots, the unchanged scenarios of those three suites pass; A report lists per suite how many scenarios are unchanged, new or dropped | 2 Foundation / Rust core |
| P-11 | Supervisor and report store; outputs: Stage execution under a PTY with cancellation, terminal handoff and restoration; Saved reports and their retention; Plain output when there is no terminal | T-01, R-01; external: none | Sol 6.2 (xhigh) under Lead T; council before merge; The Go supervisor, stream and report tests pass after translation; The manual terminal cases pass on macOS and Linux; Both council members are satisfied | 3 Modules / Terminal |
| P-12 | Dashboard, menu and report browser; outputs: The menu, the live dashboard, the issue view with the agent handoff key, and the report browser, in ratatui | T-01, R-01; external: none | Opus 5.5 (high) under Lead T; Snapshot tests cover each view at narrow and wide widths; Recordings beside the Go renderer are in the pull request for Ezra to see; The coordinator approves the look | 3 Modules / Terminal |
| Q-02 | CI on Fedora, Arch and Alpine, and a first-run job; outputs: Container jobs in rust.yml that run the suite on Fedora, Arch and Alpine; A first-run job: a container with only mise and Git runs the README quickstart | Q-01; external: none | Sol 6.2 (high) under Lead B; The jobs run on every pull request and report per scenario; At I-01 they are green, or the README claims only the package managers that pass | 2 Foundation / Rust core |
| P-01 | Packages: add, remove, adopt a dotfile, GitHub release installs; outputs: The package and GitHub scenarios in the harness; add, remove and uninstall for tools and native packages, release asset selection, archive inspection and macOS app installs; dots add FILE moving a file into dotfiles/, declaring it and applying it; dots remove FILE putting it back | Q-01; external: none | Sol 6.2 (high) under Lead B; Opus 5.5 for the wording; Unchanged scenarios pass on the current implementation and all pass on the Rust binary; A failed removal restores the declarations; After add FILE the target is a link and dots status lists it; after remove FILE it is a regular file with its original content; Comments and order in config.toml survive | 3 Modules / Packages and update |
| P-02 | status, plan, apply, test, the hook and command runner, recipe helpers; outputs: The read-only commands, apply and test; The hook runner: name order, @PROFILE filtering, the four exit meanings and the environment contract; The subcommands that replace brew-exception.sh, retire-dotfiles.sh and setup-common.sh; dots status listing each required program as found with its version or missing, with the commands that need it | Q-01; external: none | Sol 6.2 (high) under Lead B; Unchanged scenarios pass on the current implementation and all pass on the Rust binary; Without gitleaks on PATH, dots status shows it missing for sync and dots sync names it in one line; Each helper in lib/brew-exception.sh has a subcommand with a scenario | 3 Modules / Packages and update |
| P-03 | dots init does the whole first run; outputs: init selects the default profile and a machine name from the hostname; init makes the directory a Git repository with main and dev; In a terminal with gh signed in, init offers to create the remote; otherwise it prints the two commands; The embedded templates for dots/, the gitleaks rules and .gitignore | Q-01; external: none | Opus 5.5 (high) under Lead B; Sonnet subagent for routine code; A second dots init changes nothing; A noninteractive init never creates a remote; The quickstart needs no dots internal command | 3 Modules / Packages and update |
| P-06 | Backup, restore and history enrollment; outputs: A one-page design note reviewed by Lead C before code; The native-history scenarios in the harness; backup, restore and enrollment with the checks in tasks/README.md | Q-01; external: none | Sol 6.2 (xhigh) under Lead C; council before merge; Unchanged scenarios pass on the current implementation and all pass on the Rust binary; An interrupted first restore resumes; --initialize-history keeps the old store beside the new one; Restore never saves or publishes local defaults; tracked files end at mode 0600; Both council members are satisfied | 3 Modules / Bootstrap and history |
| P-07 | Sync, receipt and publish scenarios in the harness; outputs: Every scenario of the sync, receipt, publish and retired-declaration suites restated to drive the executable; A list of the scenarios that inject failures through imported modules, each with how the Rust suite covers it | Q-01; external: none | Sol 6.2 (high) under Lead C; one native subagent per suite; Every current scenario has a counterpart or a reason on the list; counts are reported per suite; The unchanged scenarios pass on the current implementation on macOS and Linux; Lead C has reviewed the list | 3 Modules / Sync |
| P-04 | dots up; outputs: The update stages in the documented order: checkpoint, sync, reload, packages, tools, exceptions, hooks, standalone mise, final checkpoint; The manager's own update rule and its report line; The sync stage skipped with one line when no remote is configured | P-02; external: none | Sol 6.2 (high) under Lead B; The managed-event and snapshot scenarios pass, restated against the report; A failing stage does not stop later stages and the exit code matches the contract; With a configuration that has no remote, dots --plain up exits 0 with nothing under Needs attention | 3 Modules / Packages and update |
| P-05 | Bootstrap, bundles, profiles and remote bootstrap; outputs: bootstrap, its completion step, bundle, profile selection and remote; Bundle values handed to bootstrap hooks as the contract describes; No Atuin or Tailscale code | P-02; external: none | Sol 6.2 (high) under Lead B; The bootstrap-hook and fixture scenarios pass; The pre-dotfiles hook works through mise x before any tool is installed; A minimal fixture bootstrap reports no Atuin or Tailscale stage; A bundle written by 0.3.5 decrypts | 3 Modules / Bootstrap and history |
| P-08 | Sync and guarded apply; outputs: A design note reviewed by Lead C before code; sync, guarded checkout replacement, validation and retired declarations, calling the Git executable; dots_source_repo and dots_state_repo accepting a Git URL as well as GitHub OWNER/NAME; dots sync without a remote saying how to add one and exiting 0 | P-07; external: none | Sol 6.2 (xhigh) under Lead C; council before merge; All sync scenarios pass on the Rust binary; dots sync always exits 0 with a receipt and never force-pushes; Every promise in REVIEW.md about sync holds; The scenarios pass with the remote named by URL; Both council members are satisfied after a five-round sol loop | 3 Modules / Sync |
| P-09 | Sync receipt and dots sync --last; outputs: The receipt written atomically with private permissions, and its replay | P-07; external: none | Sol 6.2 (high) under Lead C; The receipt scenarios pass; Patches appear only on an interactive terminal; pipes and saved reports get metadata; Displayed text and both file versions are screened before display | 3 Modules / Sync |
| P-10 | dots publish; outputs: publish from a JJ workspace through gh | P-07; external: none | Sol 6.2 (high) under Lead C; The publish scenarios pass; Against a non-GitHub remote it exits with one line naming the requirement | 3 Modules / Sync |
| X-01 | Move Atuin and Tailscale into Ezra's configuration and migrate it; outputs: The Atuin and Tailscale scripts and their tests as hooks and commands of that configuration; Recipes and hooks calling the new subcommands in place of the sourced libraries; A pull request against that repository's main that also pins the release candidate | P-02, P-05; external: none | Sol 6.2 (high) in the configuration repository; the coordinator merges at G-1; The configuration's own tests pass against the Rust binary; No file in it sources a manager library or calls dots internal home; A container bootstrap of each profile completes | 4 Cutover / Contract |
| I-01 | Integration: one binary, end to end; outputs: Every module wired into the command tree with no unimplemented command left; The whole suite green against the Rust binary on every CI platform | P-01, P-03, P-04, P-05, P-06, P-08, P-09, P-10, P-11, P-12, Q-02; external: none | Lead B, with one Sol 6.2 (high) implementer; The first-run job passes: in a container with only mise and Git, dots init then dots up succeeds; The binary runs with Bash, Node, uv and jq absent from PATH, hooks aside; The tag list of unchanged and new scenarios is complete | 4 Cutover / Rust core |
| D-01 | Delete the current implementation and package the Rust binary; outputs: bin/, lib/, tasks/, bootstrap/, dots-up/, the Python, Node and shell suites and tests/run removed; A release build of four single-file archives, static on Linux, with checksums and the Homebrew formula; One test workflow in place of the two | I-01; external: none | Sol 6.2 (high) under Lead B; The repository holds no Bash, Python, Node or Go implementation; cargo test is the only test command and CI needs no Python or Node setup; The mise and Homebrew installs pass the packaging job | 4 Cutover / Release |
| A-05 | Reference docs, the agent skill and REVIEW.md; outputs: The dots/ reference, the sync guide and the embedded skill matching 1.0; REVIEW.md without the one-person assumption and with the Rust facts reviewers need; AGENTS.md pointing at cargo test | I-01; external: none | Opus 5.5 (high) under Lead A; Every relative link resolves; No document describes behaviour or a requirement that 1.0 dropped | 4 Cutover / Adoption |
| A-02 | Rewrite the README; outputs: An opening with the pitch and the demo; A quickstart built on dots init; A section on why this and not chezmoi, Stow, Nix or plain mise; Core commands apart from dots publish and settings history; Install by mise, Homebrew and cargo; a development section for cargo | I-01; external: none | Opus 5.5 (high) under Lead A; Requirements and install fit on the first screen; Every quickstart command runs in the first-run job; The coordinator approves the wording | 5 Release / Adoption |
| A-03 | Record the demo; outputs: A checked-in VHS tape that records dots init, dots add and the dots up dashboard from the fixture configuration; The recording embedded in the README | I-01; external: none | Opus 5.5 (high) under Lead A; Running the tape regenerates the recording; Nothing from Ezra's machines appears in it | 5 Release / Adoption |
| C-01 | Distribution; outputs: dots-cli ready for crates.io, installable with cargo install and cargo binstall; The release workflow updating the Homebrew tap | D-01; external: Ezra provides a crates.io token and a token for the tap repository | Sol 6.2 (high) under Lead B; cargo publish --dry-run passes; A release-candidate tag updates the tap without a manual step; Each install method yields a working dots | 5 Release / Release |
| G-1 [gate] | Cutover: tag the release candidate and switch Ezra's machines; outputs: Council review of the whole repository; Tag v1.0.0-rc.1; X-01's pull request merged | D-01, X-01, A-05; external: none | Coordinator runs the council and approves the tag; Both council members are satisfied; CI is green on macOS, Ubuntu, Fedora, Arch and Alpine; dots up succeeds on this machine from the release candidate | 4 Cutover / Release |
| A-04 | Template configuration repository; outputs: EzraCerpac/dots-template: public, two profiles, one hook, one command, one installer exception, a pinned dots version and CI that runs dots test; The dots repository's homepage set to it | G-1; external: none | Opus 5.5 (high) under Lead A; the coordinator creates the repository; A repository created from the template passes its CI unchanged; It contains nothing specific to Ezra's machines | 5 Release / Adoption |
| G-2 [gate] | Release 1.0.0; outputs: Tag v1.0.0; dots-cli 1.0.0 on crates.io; The template repository pinned to 1.0.0 | G-1, A-02, A-03, A-04, C-01; external: Ezra has used the release candidate for a week: daily sync on two machines and one restore, with no recovery step | Coordinator approves the tag; Every fix since the release candidate has passed a sol loop; The template repository's CI passes on 1.0.0; CI is green | 5 Release / Release |
| A-06 | Listings; outputs: Pull requests to the mise registry, awesome-dotfiles and dotfiles.github.io; A drafted show-and-tell post for mise's Discussions that Ezra posts himself | A-01, G-2; external: none | Opus 5.5 drafts; the coordinator submits; Each pull request is open, starts with the disclosure note and is linked in this thread | 5 Release / Adoption |

## Dependency analysis

Structural dependency chain: K-01 → K-02 → S-01 → S-02 → R-01 → Q-01 → P-02 → P-04 → I-01 → D-01 → G-1 → A-04 → G-2 → A-06. Durations are incomplete; this is not a time-based critical path or schedule.

See [text dependencies](dependencies.md) and the interactive dependency view. External prerequisites remain separate from graph readiness.

## Milestone exits

- M1 — Design frozen; owner: Leads A and B; needs: S-01, S-02; acceptance: The contract and the architecture note have passed their councils
- M2 — The Rust binary passes the whole suite; owner: Lead B; needs: I-01; acceptance: Every scenario passes against the Rust binary on macOS, Ubuntu, Fedora, Arch and Alpine
- M3 — Cutover: release candidate on Ezra's machines; owner: Coordinator; needs: G-1; acceptance: The repository holds no Bash, Python, Node or Go implementation; Ezra's machines run 1.0.0-rc.1
- M4 — 1.0.0, findable; owner: Coordinator; needs: G-2, A-06; acceptance: On a machine with only mise and Git, the README quickstart reaches a successful dots up with no manual steps; Topics are set and the listing pull requests are open
