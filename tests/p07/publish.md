# Publish scenario inventory

P-07 accounts for all seven cases in `tests/test_dots_publish.mjs`, identical in
the base checkout and oracle `f5c8a0e`. The suite has nine `unchanged_` cases,
including two shared-dev cases, ten `new_` cases and zero dropped cases.
Each test drives the executable
selected by the absolute `DOTS_BIN`; the tests remain ignored as `P-10` until
that module exists. See `docs/rust-port.md:44–49` and `plan/plan.md:192,196`.

| Legacy case in `tests/test_dots_publish.mjs` | Executable counterpart | Contract evidence and translation |
| --- | --- | --- |
| 96 · default publication scans exactly the unpublished commits before asking | `unchanged_default_publication_scans_exactly_the_unpublished_commits_before_asking` | `docs/contract.md:759,763–764`. Publish a clean outgoing change above a baseline with a synthetic scanner finding. The scanner must ignore the already-published baseline and complete before the PTY confirmation. The fake `gh` records the configured model and person in the PR body. This replaces comparison to a particular serialized Git range and the injected confirmation callback. The next two cases cover findings inside the outgoing range. |
| 111 · the real gitleaks refuses a token committed and then removed | `unchanged_real_gitleaks_refuses_a_token_committed_and_then_removed` | `docs/contract.md:760,763,765`. Real scanner finds the synthetic token in the earlier outgoing commit even though the bookmark tip no longer contains it. Require exit 1, a real redacted github-pat finding for the fixture file and no publication side effects. |
| 131 · the real gitleaks scans content that only a merge resolution adds | `unchanged_real_gitleaks_scans_content_that_only_a_merge_resolution_adds` | `docs/contract.md:760,763,765`. Real scanner finds a token introduced at a merge commit whose parents are individually clean. Require exit 1 and a real redacted github-pat finding for the fixture file. The remote refs and PRs remain clear. |
| 150 · default publication refuses a private stack above main@origin | `unchanged_default_publication_refuses_a_private_stack_above_main_origin` | `docs/contract.md:759,765`. Two described changes cause exit 1 and no publication side effects. |
| 161 · publication uses the JJ workspace the caller is in, not the configuration root | `unchanged_publication_uses_the_callers_jj_workspace` | `docs/contract.md:755,763`. Invoke from a nested directory of a distinct colocated checkout with the plain Git live clone as `DOTS_ROOT`. Check the scanner's actual cwd and the new ref in the local bare remote. |
| 175 · publication outside a JJ workspace of the configuration stops with one line | `unchanged_publication_outside_a_jj_workspace_of_the_configuration_stops_with_one_line` | `docs/contract.md:755,765`. Exercise all three subcases (plain Git live clone, colocated checkout with another origin, directory outside a checkout). Require exit 1, a concise workspace diagnostic and no fetch or publication; the legacy exact message layout is omitted. |
| 193 · the disclosure note is optional and names the configured person | `unchanged_disclosure_note_is_optional_and_names_the_configured_person` | `docs/contract.md:88,764`. Inspect the submitted PR bodies with an absent person, a configured person and model, and a configured person with an unset or whitespace-only model. The contract specifies the person and a model name when set. The legacy helper's exact Markdown and unnamed model wording are omitted. |
| Additional oracle safeguard | `unchanged_shared_dev_outside_main_blocks_each_publication_mode` | Default mode selects the dev tip itself. Stack and bookmark modes select a descendant of local dev. A distinct current working change checks that the bookmark's ancestry controls selection. Each must exit 1 and advise extracting a focused main-based change, before any scan, push or PR. |
| Additional oracle safeguard | `unchanged_shared_dev_already_in_main_allows_each_publication_mode` | Local dev equals freshly fetched main. The same three modes reach confirmation and publish normally. Together the two cases cover both sides of the ancestry gate confirmed from oracle `tasks/publish.mjs:419–425`. |
| New preview rule | `new_publish_never_previews_the_rejected_secret` | The oracle prints outgoing diffs before running gitleaks. The Rust path scans before showing a preview. Require the synthetic token to be absent from the combined TTY stdout and stderr for both rejection fixtures. |
| No legacy case | `new_non_github_repository_exits_before_checking_publication_tools` | `docs/contract.md:249,757–758`; see P-10 in `plan/plan.md:196`. Configure a valid bare local origin. Require exit 1 with one line naming GitHub and leave all instrumented publish tools unused. |
| No legacy case | `new_invalid_repository_setting_exits_two_before_tool_probes` | The shared base configuration rejects invalid TOML and an invalid repository setting with exit 2. No tool probe or remote contact occurs. |
| No legacy case | `new_publish_rejects_absent_unsafe_or_mismatched_raw_origins_before_tool_probes` | Missing, malformed, credential-bearing and mismatched raw origins in either the live checkout or the publish workspace get a one-line status-1 rejection before tool probes. A fixture-local alias rewrite demonstrates that a matching effective URL cannot repair a wrong raw origin. The rejected URL and synthetic credential marker stay out of the diagnostic. |
| No legacy case | `new_publish_checks_effective_fetch_and_every_push_url_before_contact` | `docs/contract.md:243–245`. Independently redirect fetch and push, then set a second mismatched push URL after a matching one. Check real Git effective URLs, status 1 and zero attempts at transport. |
| New archive screening | `new_publish_finds_a_redacted_secret_one_archive_level_deep` | A real Gitleaks `github-pat` finding names the ZIP member and redacts its value. The TTY transcript stays clean and never prompts. The local bookmark, archive bytes, remote refs and PRs stay unchanged. |
| New archive screening | `new_publish_allows_a_clean_one_level_archive_to_reach_confirmation` | A clean compressed ZIP reaches actual confirmation, then the fixture verifies the approved OID in its bare remote. |
| New archive screening | `new_publish_allows_native_success_without_a_skip_signal` | Real Gitleaks exits zero without a depth or unsupported-format signal on opaque binary bytes. Publication reaches confirmation and publishes the expected revision. |
| New archive screening | `new_publish_refuses_explicit_native_format_and_depth_skips_before_confirmation` | A nested ZIP emits a native depth-limit warning. PDF bytes in a `.dat` emit a native unsupported-format record. Each scanner exits zero and publish refuses before preview, confirmation, push or PR mutation. |
| New approval cancellation | `new_publish_cancels_at_approval_without_mutation_and_records_a_report` | Ctrl-C at the real TTY approval prompt, without a newline or EOF, exits 130. The working revision, bookmarks and remote refs stay unchanged and no PR is written. A new private publish report marks cancellation and a cancelled Publish source stage. |

The legacy suite imports the publisher and supplies a custom repository validator, a frozen clock,
a command runner, a confirmation callback and the disclosure inputs directly.
The runner's simulated push failure, failed PR lookup, absent scanner and numeric scanner errors
are unused setup switches.
The executable cases verify that a published ref reaches the fixture's bare local repository.
The tests compare the bare ref to the approved commit ID and check the exact nonempty set of outgoing scanner paths, including baseline exclusion. The ref name is implementation detail.

The production executable sees a synthetic GitHub URL through real Git checkout and effective
URL validation under `docs/contract.md:243–244,249`.
The transport helper checks the real fetch and push URLs immediately before redirecting
the expected operation to the fixture's bare local repository.
The oracle travels through real JJ with a temporary local origin substitution.
Git-based publication uses real Git to resolve both origin and the command-local push alias.
Only the actual transport invocation routes that verified destination to the bare fixture.
The resulting ref and bytes are read from that repository.
Unexpected transport and all network Git protocols are refused.
The `gh` double records PR operations and body text.
Real gitleaks runs in the outgoing-history cases.
Other cases use a scanner double to record the scan directory and completion.
A private PTY drives confirmation and an allowlisted PATH selects the fixture tools.
