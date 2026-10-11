# Cross-agent feature parity report (Claude, agy, Codex): beta audit and 3.3.0 status

Before: agent-terminal 3.3.0 beta (unreleased), HEAD `8502282`. Date: 2026-10-10. After: coming 3.3.0, master at `324d5e0e`, updated 2026-10-11.
CLIs: claude 2.1.293, agy 1.3.3, codex-cli 0.161.0. The beta audit was report only: no source was edited, committed, installed or built in release.

How to read this report: every finding, drift item and matrix cell keeps its beta-audit text unchanged. A line starting **3.3.0:** (or a `3.3.0:` suffix in a cell) is the new status. The 15-turn parity scenario was NOT re-run for 3.3.0; see section 10 for what the 3.3.0 statuses rest on.

Evidence sources (all under `target/parity/` unless noted):
- `inventory.md` (T0 claims and T1 tests, Phase 1).
- `live-claude.md`, `live-agy.md`, `live-codex.md` (T2, driven in the GUI through `preview_app` and Playwright). Screenshots are in `shots/<agent>/NN-*.png`, named in the cells below by their number.
- `logs/cargo-test.log`, `logs/test-list.txt`, `logs/gate-native.log` (first, failed run), and `target/qa-artifacts/` (`gate.json`, `native-1.log` to `native-7.log`, `workspace.log`, `clippy.log`, `format.log`, `screenshots/`) for the passing gate re-run.
- `ui-audit.md` (UI robustness audit, section 3b).
- Code references are file:line from reading the source. Anything marked "suspected" was not proven by an experiment.
- For 3.3.0: the commits in `8502282f..HEAD` (named by short hash), the regression tests they added (named by test function), `plans/2026-10-10_seamless-cross-agent-experience.md` (waves, Log, W0 findings), `tests/fixtures/real/` (real frames recorded in W0), and the `tests/qa/live_smoke.py` runs of 2026-10-11.

Rule applied throughout: a live result overrides a claim. Where a runner reported BLOCKED or "not exercised", the cell is NOT-RUN and is not upgraded.

Verdict key: PASS (live), PASS-SYNTH (tests only), CLAIMED (docs or code only), GAP-ok (absent and correctly greyed or hidden), GAP-not-greyed (absent, offered anyway), FAIL, DRIFT, BLOCKED, NOT-RUN.
3.3.0 status key: **Fixed** (commit and regression test named), **Fixed, live-smoke** (the contract also held in a real CLI run of `live_smoke.py`), **Changed by design** (behaviour or claim changed on purpose; says what), **Deferred** (not done, with the reason), **Not reproduced** (the code reads correctly and no repro was found), **Unverified** (no commit or test found for it; says what is missing). In matrix cells, `3.3.0: PASS-SYNTH` means the new behaviour is proven by a test only, and `3.3.0: PASS (smoke)` means a live_smoke contract run covers it.

## 1. Executive summary

Beta audit (unchanged text):

1. **Are thoughts clearly captured per agent? Partly, and not evenly.** agy shows 23 collapsible "Thought process" blocks, persisted and replayed, although caps says it does not stream reasoning; it arrives as one whole chunk per step, read from agy's on-disk transcript. Codex shows a collapsed block only on a model that produces reasoning, and it holds a single summary title with literal `**` markers, not progressive text. Claude shows nothing: the CLI sends the thinking block with empty text and the app never asks for a display mode, so 19 empty reasoning items render nothing. Verdict: agy yes (undocumented), Codex thin, Claude no.
2. **Do actions work? Yes, with cosmetic gaps.** Read, edit, write, shell, diff panel, per-turn checkpoints and undo passed live on all three agents, and undo left `git status` clean each time. Card titling is inconsistent on Codex (raw `/usr/bin/zsh -lc` argv, creates titled "Edit") and Codex has no search kind; Claude's ExitPlanMode and Codex's approval cards dump raw JSON.
3. **Does the approval system work? Not reliably, and several failures are security-relevant.** Claude "Allow for session" silently switches the thread to Accept edits, and a non-matching `mkdir` then ran unprompted. Codex Accept edits runs shell commands (including `cat secrets.env`) without asking. Codex deny sends `cancel` and kills the turn. Codex "Save for session" wedges the card and fails the turn, and its saved always-allow rule never matched. agy reads outside the project directory are not gated. Approve, plan-mode blocking, and agy's hook-backed Ask all worked; the Codex reconnect scenario from the blocker doc also held.
4. **Is the experience shared? Mostly for queueing, interrupt, model switch, compact, redaction in the store and cross-agent continuation. Not for fork, Codex re-entry, `@` files, questions, plan panel or the live redaction.** The queue, interrupt, model switch and `/compact` worked on all three (agy's via restart and handoff). Fork did not fork, returning to Codex after Claude to agy to Codex to Claude broke the thread with "no rollout found", and the live view shows the canary unmasked while persisted copies are scrubbed.

Findings: 5 High, 6 Medium, 12 Low/cosmetic (section 3).

**3.3.0 (from code, regression tests, the QA gate and live smoke runs; not a new parity run):**

1. **Thoughts.** Claude now gets `--thinking-display summarized` on versions known to take it (>= 2.1.293), with one retry without the flag and an explicit "not shared" line if it is refused (`ef367527`); live smoke confirms the flag is accepted. All three agents map into one canonical `Reasoning { Streaming | Complete | Replayed | Unavailable }` event and one four-state widget (`4767acc1`); Codex summaries carry the label "Summary" (`e02be258`). Not re-checked live in the GUI: the Codex `**` literals (F17) and replay after reopen.
2. **Actions.** One title grammar (`Read`, `Search`, `Run`, `Edit`, `Create`, `Fetch`, `<server>/<tool>`) in agent-core feeds the card, the approval line and the sub-agent shelf (`acf6a603`, `88efb13d`, `b13a419e`). Codex `zsh -lc` is unwrapped and `commandActions` is used (`e02be258`); live smoke confirms the wrapped command and `commandActions` arrive as the code expects. Approval cards show the bare command and folder, cap the input, and render ExitPlanMode as a plan (`426a1bee`).
3. **Approvals.** The app now owns every approval decision for every agent: a typed, fail-closed rule engine (`14d0e669`), no `updatedPermissions` to Claude (`7ca166e9`), a plain `accept` for Codex session and always (`c3bde6ac`), `decline` for Codex Deny and a separate Stop turn (`bd226ef8`, `0c28918f`), Codex Accept edits on `untrusted` plus `workspaceWrite` with the app accepting contained edits (`18d99112`), and agy reads outside the project, sensitive or via symlink asking in every mode (`d5d4e20b`, `395267db`). Three red-team style rounds hardened the matcher (`982b3f92`, `522a6ff9`, `848aea14`). Live smoke confirms the Codex decline and the untrusted + `workspaceWrite` behaviour, and Claude's `can_use_tool` deny.
4. **Shared experience.** Live redaction now covers every event kind before the view sees it (`fee2d0ca`, `c61ca0c6`); Codex re-entry continues in a new thread with the handoff (`e47aeeee`); fork carries its handoff and keeps mode and model (`10574844`, `4eb88c57`), and the F8 live failure did not reproduce from the code; `@` files come from the app's own index for agy and Codex and Claude's context gauge is polled (`a0f1b58a`); every capability is a control or an explained absence (`dabc62bc`). The Codex question polyfill is deferred (separate proposed plan 2026-10-09). The diff panel stale after undo (F18) was not reproduced.

Findings 3.3.0 tally (F1 to F23): 19 Fixed, 2 Fixed with an Unverified remainder (F17, F20), 2 Not reproduced (F8, F18), 0 Changed by design, 0 Deferred. Counts for the drift list and the UI audit are in section 10.

## 2. Automated proof (T1)

| Run | Result | Evidence |
|---|---|---|
| `cargo test --workspace` | exit 0. **811 passed, 0 failed, 11 ignored** (822 listed). agent-core 258, agent-kit 142, agent-terminal bin 405 (+11 ignored), `tests/icons.rs` 6, doc-tests 0 | `logs/cargo-test.log`, `logs/test-list.txt` |
| `gate.py --native-only`, first run | FAILED before any test: `native-1: exit=127`, `Could not start test command ... 'xvfb-run'` (not installed at the time) | `logs/gate-native.log` |
| `gate.py --native-only`, after xvfb was installed | **PASSED 7/7 native tests** at `8502282` (`working_tree_dirty: true`, `delivery: false`, `native_only: true`) | `target/qa-artifacts/gate.json`, `native-1.log` to `native-7.log` |

What the 7 native tests cover (`tests/qa/native-tests.json`): long tool titles stay in the viewport, newest row stays in view while following, jump-to-latest, interruption shelf reveal and collapse, small window fit, completed plans retire to history, and `native_ux_matrix` (6 widths and scales, built on a Codex status backend). All are layout tests. None exercises an agent-specific feature, so none of the findings below could have been caught by them.
The 4 ignored tests not run by the gate: `coloured_buttons_keep_their_text_colour`, `a_thread_streamed_on_another_tab_shows_what_a_visible_one_does`, `stream_cost_of_a_long_thinking_block` (measurement only), `clear_rows_ends_and_keeps_an_open_menu`.
Codex tests (the 7 `scripted_tests.rs` cases that spawn `tests/support/scripted_codex.py`, plus the adapter tests) all passed, but they run against synthetic fixtures (`codex-synthetic-turn.ndjson`, `codex-schema-workers.ndjson`) and a fake `app-server`. Several live Codex failures below (deny, session scope, always-allow, re-entry) sit exactly where these fixtures could not reflect the real server.

**3.3.0:**
- The QA gate now runs **14 native cases** (plan Log: "Gate green at b2701e56 (14 native cases)"), up from 7. The new ones are click, toast, queue, welcome-button, dropdown-truncation, `@`-popover and context-gauge cases: `native_click_matrix`, `native_toasts_clear_the_composer`, `native_queue_stays_pinned_and_contained` (`src/chat/view/ux_tests.rs`), `native_at_popover_lists_files_for_all_three_drivers` and `native_the_context_gauge_is_polled_after_a_turn_for_agents_that_do_not_push_it` (`src/chat/view/gap_tests.rs`), `native_welcome_button_is_inside_the_dialog` (`src/window/imp/setup.rs`), `native_dropdown_rows_show_their_whole_value` (`agents_prefs.rs`). The gate was last recorded green at `b2701e56`, not at the current HEAD `324d5e0e`, and the current `cargo test --workspace` totals were not re-recorded in this report. (Unverified: gate result and test counts at `324d5e0e`.)
- Real recorded frames now exist: `tests/fixtures/real/{codex,claude,agy}/` (W0, `8e522d9c`), so the Codex column is no longer tested only against synthetic fixtures. Fixture-replay tests cited below name the frame they replay.
- `tests/qa/live_smoke.py` (opt-in, `dc26e88a`) was run against the real CLIs on 2026-10-11: Claude 9/9, Codex 16/16, agy 5/5 contract checks passed (details in section 10).

## 3. Ranked findings

Severity: High = approval or secret-handling correctness, or the product does the opposite of what the UI says. Medium = feature fails or is misleading. Low = cosmetic or documentation. Security and approval items come first.

### High

**F1. Claude "Allow for session" broadens to Accept edits; a non-matching command ran unprompted.** Agents: Claude.
- Repro: Ask mode, trigger a `touch` approval, More, "Allow for session", choose `touch *` in "Confirm Session Approval Rule", "Save for Session & Allow". Next turn: `touch c.tmp` then `mkdir dd` both ran with no card; the mode chip had flipped from "Ask before edits" to "Accept edits".
- Evidence: `live-claude.md` step 5; store event 365 (permission suggestions were `addRules(Bash "touch b.tmp", localSettings)`, `addDirectories(/tmp/parity-claude, session)`, `setMode acceptEdits (session)`; resolution `allow_for_session`); screenshots `05a` to `05e`. The user's edited pattern was ignored.
- Suspected: `crates/agent-core/src/claude.rs:1312-1326` (turns `AllowForSession` into the CLI's own `updatedPermissions`, including its `setMode acceptEdits` suggestion) and `src/chat/session.rs:1553-1558` (only agy and Codex consume `custom_rule`); the dialog itself is driver-blind (`src/chat/view/cards.rs:1668-1696`).
- Tests: `claude::tests::always_allow_writes_only_rules_to_local_settings_and_session_stays_in_memory` and `encode_approve_allow_deny_cancel_session` assert the `setMode acceptEdits` output, so the tests lock in the broadening. No test covers "a command that does not match the chosen pattern still asks".
- **3.3.0: Fixed.** `7ca166e9` sends a plain allow for Allow for session and Always, never `updatedPermissions` (no `addRules`, no `setMode`), so Claude's mode and `.claude/settings.local.json` stay untouched. `14d0e669` makes the app record the user's chosen pattern in its own typed rule store and answer later matching requests itself. Tests: `allow_for_session_and_always_send_no_updated_permissions` (replays the W0 `claude/thinking-and-touch.jsonl` `can_use_tool` frame) and `claude_allow_for_session_is_app_owned_and_exact` (the F1 repro: `touch b.tmp` auto-approves, `mkdir dd` and a compound command ask, the mode stays Ask). The two old tests that locked in the broadening were rewritten. Not covered by live smoke (the smoke asserts the deny path only).

**F2. Codex Accept edits runs shell commands without asking.** Agents: Codex.
- Repro: Accept edits mode; ask for `ls`, `sleep 25` and `cat secrets.env`. All ran with no card (the scenario expects shell to still ask). In Ask mode even read-only commands asked.
- Evidence: `live-codex.md` step 7; screenshots `12`, `18`.
- Suspected: `crates/agent-core/src/codex.rs:1996` maps AcceptEdits to `on-request` + `workspaceWrite`, so the sandbox runs commands unasked; the adapter and README still promise "commands ask" (`crates/agent-core/src/adapter.rs:129`).
- Tests: `codex::tests::prompt_encodes_turn_start_with_the_mode_policy` and `deferred_modes_preserve_accept_edits_cancel_and_apply_only_to_the_next_turn` assert this mapping. Nothing checks the user-visible promise.
- **3.3.0: Fixed, live-smoke.** `18d99112`: Accept edits now sends `untrusted` + `workspaceWrite`, and the session answers `accept` itself for a file change whose every path (and rename target) is inside the workspace and not sensitive; shell commands, out-of-workspace and sensitive edits show a card. Test: `codex_accept_edits_auto_accepts_workspace_edits_and_shell_still_asks` (replays W0 `codex/untrusted-workspacewrite.jsonl`; Ask and an `/etc` edit also ask). `e5495687` adds `codex_accept_edits_chosen_mid_turn_does_not_auto_accept_that_turn`. Live smoke (Codex, 2026-10-11): with `untrusted` + `workspaceWrite` the edit asks and `accept` applies it.

**F3. Codex deny is `cancel`, which ends the whole turn.** Agents: Codex.
- Repro: Ask mode, any command approval, press the only negative button, "Cancel". Card shows "Declined", then "Interrupted"; the agent never sees a denial and cannot continue.
- Evidence: `live-codex.md` step 4a; store row 127 `approval_resolved cancel`, row 133 `turn_completed interrupted`; screenshot `05b`.
- Suspected: `crates/agent-core/src/codex.rs:1385-1411` builds the option list from the server's `availableDecisions`; when `decline` is not in that list only Cancel is shown. `decision_wire` maps Deny to `decline` (`codex.rs:2015`) but it is never offered.
- Tests: `codex::tests::available_decisions_limit_the_options` and `a_decision_the_server_did_not_offer_is_refused_and_the_request_stays_open` exercise the limiting with synthetic frames. No test models what the real server offers.
- **3.3.0: Fixed, live-smoke.** `bd226ef8`: Deny is offered on every Codex approval and goes out as `decline` even when `availableDecisions` omits it (the refusal of unoffered decisions is relaxed for `decline` only); `cancel` is a separate button labelled **Stop turn**. `0c28918f` puts Stop turn on Claude's and agy's cards too (D4: the same five buttons everywhere; Codex shows Stop turn only when the server offers `cancel`, since an unoffered `cancel` was not verified). Tests: `w0_deny_sends_decline_and_stop_sends_cancel` (replays W0 `codex/ask-shell-decline.jsonl` and `ask-edit-decline.jsonl`), `stop_turn_on_an_approval_ends_the_turn_interrupted` (Claude), `agy_stop_turn_denies_the_hook_and_interrupts_the_turn`. Live smoke (Codex): an unoffered `decline` is accepted (item ends declined, turn continues).

**F4. agy reads outside the project directory are ungated in Ask mode.** Agents: agy.
- Repro: Ask mode, deny `ls -la`; agy then guessed paths for about two minutes and read `/home/jdesroches/.gemini/config/hooks.json` and the sibling sandbox `/tmp/parity-claude/.git/*` with no approval card.
- Evidence: `live-agy.md` row 4c and "Other findings" 1; screenshots `02c` to `02e`. The runner notes this is by design for reads.
- Suspected: `src/approval_server.rs:72-110` (`classify` and `mutates`): read tools are not queried at all, so no path containment applies to reads.
- Tests: `approval_server::tests::policy_table` and the symlink and workspace-containment tests cover edits only. Nothing covers read scope.
- **3.3.0: Fixed (the by-design read policy was changed).** `d5d4e20b`: in every mode, Plan included, reads outside the workspace, of sensitive targets, or through a symlink ask; in-workspace reads stay automatic. `395267db` fails closed on odd read arguments and guards recursive search from home or above. Tests: `policy_table_read_containment`, `w0_agy_fixture_read_outside_the_project_asks_in_every_mode` (uses the W0 agy fixture), `read_containment_fails_closed_on_odd_arguments`, `recursive_search_from_home_or_above_asks_but_list_dir_does_not`. Not re-run live as a multi-minute agy session; the live_smoke agy run does not exercise the approval hook.

**F5. The live view shows the canary unmasked; only persisted and replayed copies are masked.** Agents: agy (user bubble), Codex (tool output and echo). Claude: not observed.
- Repro: put `sk-parity-canary-0000000000000000` in a prompt (agy) or have Codex print `secrets.env` verbatim. Live view shows the full value. After reopening, the replayed view and the DB show `****0000`.
- Evidence: `live-agy.md` step 10 (store: 0 rows for the value, user bubble live unmasked; replay after reopen not re-checked); `live-codex.md` step 10 (live render `PARITY_CANARY_TOKEN=sk-parity-canary-0000000000000000`, replay `****0000`; screenshots `21`, `27`).
- Suspected: scrubbing happens only in the store, `crates/agent-kit/src/store.rs:171` (`scrub_envelope`), while `src/chat/session.rs:1470-1485` delivers the raw prompt snapshot to the view first and live envelopes are not scrubbed before display.
- Tests: `store::tests::a_planted_token_is_masked_in_raw_and_text` and `redact::tests::*` prove the store path. No test asserts that the live view masks.
- **3.3.0: Fixed (one known ceiling).** `c61ca0c6` masks approval titles, input and detail and the prompt snapshot before the view sees them; `fee2d0ca` extends it to every event kind via `approval_server::masked_for_view`, which reuses the store's `scrub_envelope` (fail closed if it cannot mask). The answer path still carries the real call. Test: `chat::session::tests::a_secret_in_an_approval_or_prompt_reaches_the_view_masked` (an `sk-` token in an approval, the prompt, and tool start/complete, content delta, question, notice, error and turn-completed events reaches the sink as `****6789`; the approval offers no Session or Always button). `848aea14` also masks credential-bearing CLI options and headers (`redact::tests::credential_options_and_headers_are_masked`). Ceiling, stated in the code: a secret split across two `ContentDelta` chunks is shown as typed until the store heals the item at completion. Not re-checked in the live GUI.

### Medium

**F6. Codex "Save for Session" is offered, then wedges the card and fails the turn.** Agents: Codex.
- Repro: Ask mode, command approval, More, Always allow dialog, Save for Session. Result: red `Turn failed: invalid command: Codex did not offer acceptForSession for approval ...`; card stuck on "Allow for session..." with greyed buttons until Stop (Stop recovers cleanly).
- Evidence: `live-codex.md` step 5; screenshots `07a` to `07d`.
- Suspected: `crates/agent-core/src/codex.rs:1739` rejects a decision the server did not offer, while the UI offers it regardless (`src/chat/view/cards.rs:1700`, `src/chat/session.rs:1559-1599`).
- Tests: `codex::tests::a_decision_the_server_did_not_offer_is_refused_and_the_request_stays_open` covers the refusal but not the wedged UI, and no test covers the UI offering it.
- **3.3.0: Fixed.** `c3bde6ac`: Allow for session and Always go out as a plain `accept`; `acceptForSession` and `acceptWithExecpolicyAmendment` are never sent, so Codex keeps no second rule store and fileChange requests (which carry no `availableDecisions`) work too. The scope is app-owned (`14d0e669`). Tests: `w0_session_and_always_answer_a_plain_accept` (W0 shell and edit frames) and `codex_allow_for_session_replays_the_w0_frame` (the same request then auto-approves; `ls -la; id` asks).

**F7. Codex "Always allow" does not persist as an effective rule, and the dialog is pre-filled with the wrong text.** Agents: Codex.
- Repro: Always allow, Save Always & Allow on `git status --short`. Dialog pattern field read a footer sentence ("`...atus --short` in /tmp/parity-codex") and "Permit all tools on ``". The rule saved ("Always allowed here"), but the identical command asked again in a new turn. A non-matching `git log --oneline` asked as expected.
- Evidence: `live-codex.md` step 5; screenshots `08a` to `08c`. The runner's mismatch theory (the `zsh -lc` wrapper) is UNVERIFIED.
- Suspected: `src/chat/view/cards.rs:1660` pre-fills the dialog from `a.remembers`, which `crates/agent-core/src/codex.rs:1378` formats as the sentence `` `cmd` in cwd ``; that string becomes the rule pattern. Matching then happens in `src/chat/session.rs:1048-1094`.
- Tests: `chat::session::tests::codex_session_approval_rule_auto_approves_matching_commands` passes with synthetic input. It does not use the dialog's pre-fill or a `zsh -lc` command.
- **3.3.0: Fixed.** `71ac2613`: one strict `unwrap_shell_wrapper()` serves both rule creation (`dialog_prefill()`, typed by action kind) and matching, so a rule saved from `/usr/bin/zsh -lc 'ls -la'` is `ls -la` and matches that request again. Tests: `rule_dialog_prefills_the_bare_w0_command` (entry text built from the W0 frame), `codex_always_rule_from_the_prefill_matches_the_w0_request_next_time` (saved rule, new session, auto-approved), `only_the_strict_wrapper_shape_is_unwrapped`. `6c5d6f04` adds inline validation in the dialog (`rule_dialog_validates_the_pattern_inline`). The runner's `zsh -lc` mismatch theory is consistent with the fix but was never proven live, so the cause stays UNVERIFIED.

**F8. Fork does not fork.** Agents: Codex, Claude (the chain used both).
- Repro: sidebar context menu "Continue in a new thread", Claude then Default model, and Codex then Default model: both switched the same thread in place (store keeps one thread, notice "Continuing in Claude/Codex with 39 of 68 earlier messages"). Typing `/fork` was sent to Codex as an ordinary prompt.
- Evidence: `live-claude.md` step 11 "Fork"; screenshots `11a`, `11o` to `11u`. The runner notes the popover was driven by keyboard and hover, so a mis-target of "Switch this thread to" is possible; the submenu header read "Continue in a new thread".
- Suspected: the code reads as if `/fork` should work (`src/chat/view.rs:925`, `src/window/imp/threads.rs:3459-3473`); why the live run differs is unknown. UNVERIFIED cause.
- Tests: none call `fork_thread`.
- **3.3.0: Not reproduced.** `10574844` records that the live failure did not reproduce from the code: the sidebar path creates a new thread, so the live run was most likely the in-place Switch item. What did change in the same area: a fork now passes its handoff in `NewThread`, reads the source transcript through `store_job` (off the main thread) and no longer attaches the handoff to whichever page is selected; `4eb88c57` keeps the source thread's mode and model. Tests added: `window::imp::threads::tests::a_fork_makes_a_second_thread_and_leaves_the_original_exactly_as_it_was` (every agent; the source's events, active provider thread and summary are unchanged) and `chat::view::tests::slash_fork_is_a_local_builtin_for_every_agent` (the `/fork` symptom). Not re-driven in the GUI.

**F9. Re-entering Codex after Claude to agy to Codex to Claude fails with "no rollout found".** Agents: Codex (via the Claude chain).
- Repro: switch Claude, agy, Codex, back to Claude, then Codex again. Every later prompt fails with `Could not switch to gpt-6.1-sol: no rollout found for thread id 01a1282c-...` and `Turn failed: no rollout found for thread id ...`; the chip shows gpt-6.1-sol. The thread is unusable. No workaround attempted.
- Evidence: `live-claude.md` step 11; screenshot `11u`.
- Suspected: a stale Codex thread id is resumed (`crates/agent-core/src/codex.rs` handshake with `thread/resume`; stale id kept by `src/chat/session.rs` provider-thread handling). UNVERIFIED.
- Tests: `chat::session::scripted_tests::scripted_codex_claude_codex_switch_routes_replies_and_recovers_a_failed_switch` covers one round trip with a fake server. No test goes through a second Claude to Codex re-entry or a missing rollout.
- **3.3.0: Fixed.** The cause was found: Codex writes no rollout until a turn completes, so an id stored at `thread/start` could not be resumed. `e47aeeee`: the session keeps a new Codex thread id provisional and stores it only once a turn on it completes (`e5495687`: or produces any agent item); a `thread/resume` "no rollout found" starts a new thread with the same settings plus a notice, and the stored history is carried into the next prompt as a handoff. Tests: `a_resume_with_no_rollout_starts_a_new_thread` (adapter), `scripted_resume_with_no_rollout_continues_in_a_new_codex_thread` (persisted stale id, next prompt succeeds in a new thread, id stored after the turn), `scripted_a_turn_with_an_item_confirms_the_new_codex_thread`. The error text match is evidence from this report (Codex 0.161 seen live), not from a W0 frame. The five-leg chain was not replayed live.

**F10. Claude reasoning never appears.** Agents: Claude.
- Repro: ask a "think step by step" prompt. No thinking block. Store has 19 `item_started kind=reasoning` plus bare `item_completed` with zero reasoning `content_delta`. Raw frames carry `"thinking_display":"updates"` and `{"type":"thinking","thinking":"","signature":""}`.
- Evidence: `live-claude.md` step 1; screenshots `01a` to `01d`.
- Suspected: the app never requests a thinking display mode (`crates/agent-core/src/claude.rs:1034-1060` argv has none; the runner's grep for `thinking-display` or `show-thinking` found nothing). The adapter handles `thinking_delta` (`claude.rs:504`) but the CLI sends none. Empty reasoning items are emitted and render nothing.
- Tests: no test names thinking. Fixtures `claude-interrupt`, `claude-turn-approval`, `claude-controls-slash` contain `thinking` frames, but none checks that text reaches the UI.
- **3.3.0: Fixed, live-smoke.** `ef367527` passes `--thinking-display summarized` only to Claude versions known to accept it (>= 2.1.293, learned from `claude --version` by the probe and the session's init frame); if the CLI exits at start refusing it, the session restarts once without the flag (no error shown) and the thread shows a closed, dim "not shared" reasoning line (`4767acc1`). `b13a419e` tightens the retry (needs a failed exit before any init frame). Tests: `w0_thinking_blocks_stream_then_complete_with_their_text` and `a_thinking_block_with_no_text_is_unavailable` (adapter, W0 `thinking-and-touch.jsonl`), `thinking_display_is_gated_on_known_versions`, `the_probe_learns_the_version_that_gates_thinking_display`, `claude_refusing_the_thinking_flag_restarts_once_without_it`, `the_thinking_retry_needs_a_failed_exit_before_init`, and `w0_claude_thinking_renders_complete_live_and_replayed_from_history` (view model). Live smoke (Claude, 9/9): the thinking flag is accepted by the real CLI. Replay of a thinking block after reopen in the GUI was not re-checked.

**F11. Claude context gauge is absent although caps says `context_usage: true`.** Agents: Claude.
- Evidence: `live-claude.md` step 9 (usage shown; no gauge like Codex's `29k / 258k`; context only appears as the compact divider "45k -> 5.9k"); screenshot `09n`. The runner marks it unverified (may need a control round trip). Verdict DRIFT, low confidence.
- Tests: `claude::tests::context_usage_reply_updates_the_gauge` passes with synthetic input.
- **3.3.0: Fixed.** `a0f1b58a`: Claude and agy report context occupancy only when asked, so the view now requests `ContextUsage` after each turn (Codex pushes it). Test: `native_the_context_gauge_is_polled_after_a_turn_for_agents_that_do_not_push_it` (polled for Claude and agy, not Codex). The original cause (a control round trip that never ran) matches the fix, but the gauge was not re-seen in a live GUI session.

### Low and cosmetic

| ID | Agent | Issue | Evidence | 3.3.0 status |
|---|---|---|---|---|
| F12 | Claude, agy | A denial shows the card label "Denied" in green | `live-claude.md` step 4 (`04b`), `live-agy.md` 4a (`06c`) | **Fixed.** `bd226ef8`: a resolved Deny or Stop turn is styled red (`.approval-card.resolved.denied` in `style.css`, shared by every agent's card). Test `codex_approval_card_checks` (Deny -> "Denied", `denied` class); the test drives a Codex card, the class is agent-neutral, and Claude and agy cards were not separately asserted |
| F13 | agy | Interrupt renders a red "Turn failed: interrupted" card | `live-agy.md` 8b (`13a`) | **Fixed.** `49801c70`: agy's stderr `error: interrupted` is dropped, since the turn already ends Interrupted. Test: `agy::tests::interrupt_keeps_partial_text_and_reports_interrupted` asserts no `Event::Error` |
| F14 | Claude | Interrupt shows two notices ("[Request interrupted by user]" and "Interrupted") | `live-claude.md` step 8 (`08c`) | **Fixed.** `d159b90d` collapses duplicate model-switch and interrupted notices. Test: `chat::view::model::tests::a_model_switch_and_an_interrupt_are_each_said_once` |
| F15 | Claude | ExitPlanMode approval card is raw JSON, not a rendered plan | `live-claude.md` step 6 (`06c`) | **Fixed.** `426a1bee` renders the plan as markdown. Test: `approval_cards_show_the_command_and_cap_the_rest` (includes an ExitPlanMode plan) |
| F16 | Codex | Approval cards dump the full raw JSON request (`availableDecisions`, ids, `startedAtMs`) | `live-codex.md` step 4a (`05b`); `live-claude.md` step 11 | **Fixed.** `426a1bee`: the card shows the bare command (strict `zsh -lc` unwrapped), the working directory and description, other input is capped at 12 lines and one line at 400 characters, and the JSON sits behind Show raw. `82d8840d` (rust-reviewer blocker): a cut card always has Show raw, so the user never approves text they cannot see. Same test, with a 200-key input, a 30-line heredoc and a 5000-character line |
| F17 | Codex | Command cards are titled with raw `/usr/bin/zsh -lc ...` argv; creates are titled "Edit"; reasoning shows literal `**` | `live-codex.md` steps 1, 2, 3 (`04c`, `06a`, `12` to `17`) | **Fixed (titles), Unverified (`**`).** `acf6a603` (canonical `ToolKind` and title grammar in agent-core), `e02be258` (commandActions: `Read <path>`, `Search <query>`, else the unwrapped command, e.g. `Run ls -la` for the W0 `/usr/bin/zsh -lc 'ls -la'`), `88efb13d` (one wrapper definition), `b13a419e` (card, approval line and sub-agent shelf share one title source). Tests: `golden_titles_codex_w0` (also `golden_titles_claude_w0`, `golden_titles_agy_w0`), and the Codex W0 command-title tests in `e02be258`. The create-titled-"Edit" case is covered by the `Create <path>` grammar in `acf6a603` but I found no Codex-specific create test. The literal `**` markers: no commit or test strips them (a Codex adapter test still expects `**Planning** the listing` unchanged); the summary is now labelled "Summary" (`e02be258`) but whether `**` still shows needs a live check |
| F18 | Claude | Diff panel stays on the previous turn's two files after an undo, until a later change | `live-claude.md` step 3 (`03i`, `04b`) | **Not reproduced.** Plan Log: "diff-panel-after-undo not reproduced". No fix commit and no regression test; the item stays on the W4 list (4.6) as open evidence |
| F19 | Claude | Model switch shows two banners; the question card keeps "Sending..." after completion | `live-claude.md` step 9 (`09c`, `09l`) | **Fixed.** Two banners: `d159b90d` (`a_model_switch_and_an_interrupt_are_each_said_once`). Question card: `3b2e3889` emits `QuestionResolved { answered: true }` once the adapter has accepted and written the answer, moving the card to Answered; a refused answer resolves nothing. Test: `scripted_an_accepted_answer_moves_the_question_card_to_answered` (Codex question over the scripted app-server) |
| F20 | Claude | GTK markup errors on startup and in Settings > Agents: unescaped `&` in "In-App Approvals & Pattern Matching" and "Confirm & Customise Patterns on Save" (empty row title), and `<folder>/<repo>/<branch>` in the worktree subtitle | `live-claude.md` harness notes (`11c`) | **Fixed (`&` titles), Unverified (worktree subtitle).** `709731e1` escapes the ampersands; `907b05af` escapes the diff-tool error title. Test: `window::imp::agents_prefs::tests::pages_have_renderable_titles` (Settings -> Agents and the setup walkthrough: every row and group title parses). The worktree subtitle is plain text at `src/window/imp.rs:3700`, but that came from commit `2a7d207c`, which is not in `8502282f..HEAD`, and no 3.3.0 commit or test names it, so I cannot say the live error is gone |
| F21 | agy | `/usage` panel is a raw key and path dump | `live-agy.md` 9f (`16b`) | **Fixed.** `49801c70`: agy's quota windows show as grouped rows (percent used, time to reset); any other shape keeps the plain page. Test: `agys_usage_document_becomes_readable_rows_and_anything_else_stays_plain` |
| F22 | agy | Compact-by-handoff and model switch silently adopt the profile default model and mode (after an accidental "Make Default" the new thread jumped to gemini-3.8-flash-high and Plan); the "Make Default" toast also captures clicks on the composer | `live-agy.md` Other findings 2, 6 (`15a`, `15c`) | **Fixed.** Mode: `5a2a71e4` (a continued thread keeps its mode) and `4eb88c57` (a fork keeps the source's mode and model; `start_mode` extracted). Tests: `agy_model_switch_and_compact_keep_the_threads_mode` (Ask stays Ask through a model switch and a compact-style restart; no mode change announced; no `--mode plan`), `a_thread_starts_in_the_chosen_mode_before_any_default`. Toast: `709731e1` routes toasts through the selected thread's transcript overlay, test `native_toasts_clear_the_composer`. The adopted default *model* is covered only for fork (`4eb88c57`); I found no agy model-switch test for the model half |
| F23 | agy | No positive hook proof anywhere: the journal only logs `approval socket listening`, no "hook verified" line and no UI badge | `live-agy.md` Hook status, row 11 | **Fixed.** `7e583b6e`: a three-state hook badge in Settings (verified, missing, unreadable) with a journal verdict. Tests: `hook_config::tests::badge_has_three_states_and_unreadable_is_never_healthy`, `probe_separates_missing_from_unreadable`. Not seen in a live GUI session |

## 3b. UI audit findings (`ui-audit.md`)

The audit (Hive #305 to #308) is a separate set from F1 to F23. Original rank and severity are kept; the 3.3.0 status is new. Evidence is Broadway screenshots in `shots/ui/`.

| # | Severity | Finding | 3.3.0 status |
|---|---|---|---|
| U1 | BLOCKER | Selectable labels inside buttons swallow the click (tool-card title, "Thought process", usage meter, sub-agent counter, question options, rule-dialog suggestions, shelf rows) | **Fixed.** `d159b90d`: `cards::label()` is non-selectable by default, `content_label()` opts in. Tests: `chat::view::tests::whole_view_has_no_dead_click_targets`, `cards::tests::click_targets_hold_no_selectable_labels` (widget-tree walk via `testutil::selectable_labels_in_click_targets`), and `native_click_matrix` (clicks each surface and asserts the effect) |
| U2 | SHOULD | A bare `&` in an adw row title blanks the title | **Fixed.** `709731e1`; test `pages_have_renderable_titles` (see F20) |
| U3 | SHOULD | Header overflows at 360 px | **Fixed.** `d159b90d` drops the activity words at the compact breakpoint. Covered by the header case in the new native tests (`native_ux_matrix`); I did not read the exact assertion |
| U4 | SHOULD | Toasts sit on the composer | **Fixed.** `709731e1` (`add_toast` targets the selected thread's transcript overlay). Test: `native_toasts_clear_the_composer` |
| U5 | SHOULD | Codex approval card dumps uncapped raw JSON | **Fixed.** `426a1bee`, `82d8840d` (see F16) |
| U6 | SHOULD | Non-wrapping labels can force the transcript wider (divider, question options and chip, plan step) | **Fixed.** `d159b90d` wraps divider, chip, option, step and heading text. Test: the long-text matrix in `native_ux_matrix` |
| U7 | NICE | No copy affordance on tool input/output, diffs, approval details, errors; output capped at 40 lines without "show full" | **Fixed.** `d159b90d`: Copy on every tool section, diff, approval detail and error, with Show full for capped output. I found no dedicated test name for the Copy buttons |
| U8 | NICE | Small hit targets, unlabelled icon buttons | **Fixed in part.** `d159b90d`: icon buttons get accessible names and focus rings. Hit-target size tests: the new native cases cite a 34 px threshold (`907b05af`); no test named for the 24 px code-block Copy button |
| U9 | NICE | Truncated Settings dropdown value | **Fixed.** `907b05af`; test `native_dropdown_rows_show_their_whole_value` (reproduced "Automatic" truncated at 360 px before the fix) |
| U10 | (surface table) | Welcome dialog: "Get Started" sits on the dialog edge | **Fixed.** `709731e1` moves it to a bottom bar. Test: `native_welcome_button_is_inside_the_dialog` |
| U11 | (surface table) | Queue not exercised by the audit | **Fixed (coverage).** `d159b90d` adds a queue demo scene; test `native_queue_stays_pinned_and_contained`. The audit's other uncovered cases (600 and 1400 px, diff panel width) were not separately claimed |

## 4. Matrix

Cell format: VERDICT, then evidence. "T2" is live, "T1" test name, "T0" claim. Screenshot numbers refer to `shots/<agent>/NN-*.png`. A `3.3.0:` suffix marks a cell whose status changed; cells without one are unchanged because nothing in `8502282f..HEAD` touched them or the change is not proven.

### 4a. Thoughts

| Feature | Claude | agy | Codex |
|---|---|---|---|
| Reasoning captured and shown | **DRIFT**: nothing rendered; empty thinking text (F10); `live-claude.md` s1. **3.3.0: PASS (smoke)**: `--thinking-display summarized` accepted by the real CLI; PASS-SYNTH render from W0 frames (`w0_claude_thinking_renders_complete_live_and_replayed_from_history`) | **PASS**: 23 "Thought process" blocks; `live-agy.md` 1a (`02c`, `02h`, `02i`). 3.3.0: still PASS, now through the canonical Reasoning event (PASS-SYNTH `w0_agy_thinking_is_one_whole_block_never_streamed`) | **PASS** on `gpt-6-sol` only; `gpt-6-luna` gave 0 reasoning tokens; `live-codex.md` s1 (`11`, `27`). 3.3.0: PASS-SYNTH, label "Summary" (`e02be258`) |
| Streamed progressively | **DRIFT**: caps true, nothing arrives. **3.3.0: PASS-SYNTH** (Streaming state from non-empty `thinking_delta`, W0 frames) | **GAP-ok**: one whole chunk per step (23 items, 23 deltas vs 298 assistant deltas); caps false is accurate but see D1. 3.3.0: caps now say true and the code never fakes streaming (D1) | **DRIFT**: caps true, but one summary title per item (single `summaryTextDelta`). 3.3.0: **Changed by design**: summaries only, labelled "Summary"; streaming beyond summaries is on the plan's cut list |
| Collapsible | NOT-RUN (no block). 3.3.0: PASS-SYNTH (one four-state widget, `reasoning_ui_checks`) | **PASS** (`02h`) | **PASS** |
| Persisted and replayed after close and reopen | NOT-RUN: nothing to replay; reopen cycle not completed. 3.3.0: PASS-SYNTH (stored history marked Replayed; `w0_claude_thinking_renders_...replayed_from_history`) | **PASS** (`03d`) | **PASS** (27 blocks replayed after reopen) |
| Not carried in hand-off | PASS-SYNTH: `store::tests::transcript_leaves_out_reasoning`; chain notice "Reasoning and tool state are not carried" (`11f`) | same | same |
| Native import of thinking | PASS-SYNTH: `import::tests::assistant_text_and_thinking_use_the_live_item_kinds` | PASS-SYNTH: same family | PASS-SYNTH: `import::tests::codex_transcript_imports_*` |

### 4b. Actions

| Feature | Claude | agy | Codex |
|---|---|---|---|
| Read and search cards | **PASS**: `Read`, `Run`, `Search` titled; no Grep or Glob in this CLI build; `02` | **PASS** read; **GAP-ok** no search kind (agy uses `run_command`); `02g`, `04a`. 3.3.0: PASS-SYNTH, steps titled from decoded transcript calls (`Read /etc/hostname`; `golden_titles_agy_w0`) | **DRIFT**: raw `zsh -lc` titles, no semantic Read or Search (F17); `04c`, `06a`, `06c`. **3.3.0: PASS-SYNTH**: `Read`/`Search` from `commandActions`, else the unwrapped command (`golden_titles_codex_w0`); live smoke confirms the wrapped command and `commandActions` arrive |
| Edit and new-file card with diff | **PASS** `03a` to `03d` (approval card with unified diff) | **PASS** `05a` to `05e` (`Edit` and `Write`, `+2 -0`, `/dev/null` to notes.txt) | **PASS** minor DRIFT: create titled "Edit"; `12` to `15`. 3.3.0: `Create <path>` exists in the grammar; no Codex-specific create test found (F17) |
| Diff vs turn baseline ("Changes this turn") | PASS `03f` | PASS `05e` | PASS `14`, `15` |
| Diff panel bases | **PASS** (Uncommitted, Target branch, Last turn, This tab) `03e`; stale after undo (F18). 3.3.0: F18 not reproduced | PASS `05f`-`05h` | PASS `16` |
| Checkpoint refs | PASS: `refs/agent-terminal/<id>/0001`, `0002` | PASS: journal `Checkpoint ... 0001..0003` | PASS: hidden refs `0001`, `0002`, plus `pre-restore-*` |
| Undo (restore, itself undoable) | **PASS** `03g`-`03i` (`git status` clean) | **PASS** `05i`, `05j` | **PASS** `17` |
| Plan panel (steps) | CLAIMED/DRIFT: no emitter in `claude.rs`; not seen live. 3.3.0: **Changed by design**: no emitter, panel reveals only on a non-empty plan (D4) | CLAIMED/DRIFT: no emitter. 3.3.0: same | PASS-SYNTH: `codex.rs:961`, `codex::tests::plan_updates_errors_and_reroutes`; not seen live |
| Sub-agents | PASS-SYNTH: `claude::tests::a_background_sub_agent_runs_past_its_launch_result_and_the_turn_end` | CLAIMED: card only (`agy.rs:773`) | PASS-SYNTH: `codex::tests::collaboration_frames_publish_workers_and_explicit_waits`. 3.3.0: the shelf row titles come from the shared grammar (`b13a419e`) for all three |
| Worktrees | PASS-SYNTH: `worktree::tests::*`; NOT-RUN live | PASS-SYNTH; NOT-RUN | PASS-SYNTH; NOT-RUN |

### 4c. Approvals

| Feature | Claude | agy | Codex |
|---|---|---|---|
| Ask: approve and deny, agent sees result | **PASS** (deny text reached the agent; `04a`-`04d`). 3.3.0: PASS (smoke): `can_use_tool` deny is honoured | **PASS** (agent quoted the hook denial; `06c`, `07a`) | **FAIL** deny = cancel ends the turn (F3); approve **PASS** `06b`, `06c`. **3.3.0: PASS (smoke)**: Deny sends `decline`, an unoffered `decline` is accepted; Stop turn is separate |
| Accept edits: edit applies, shell asks | **PASS**: edit unprompted, `python3 -c` asked (`07`) | **PASS**: edit unprompted, `echo` asked and denied (`11a`) | **FAIL/DRIFT**: edit applied, shell also ran unprompted (F2). **3.3.0: PASS (smoke)** for the edit asks + accept applies contract; PASS-SYNTH for the shell asks (`codex_accept_edits_auto_accepts_workspace_edits_and_shell_still_asks`) |
| Plan blocks edits | **PASS**: no edit; plan file written; ExitPlanMode asked (`06a`-`06d`) | **PASS** with hook: no edit, no card, block came from agy's own plan mode not the hook (`10e`) | **PASS**: read-only sandbox rejected the patch (`10`) |
| Bypass mode | GAP-ok: no such app mode (`adapter.rs:125`); Claude `bypassPermissions` ignored (`claude.rs:1404`); NOT-RUN | GAP-ok: skip flag is internal only | GAP-ok. 3.3.0: all three **Changed by design**: bypass mode is on the plan's cut list |
| Always-allow (persistent) | **FAIL**-adjacent: Claude writes its own rule; the edited pattern is ignored (F1). **3.3.0: PASS-SYNTH**: app-owned typed rule with the user's pattern; nothing written to Claude's settings | **PASS-ish**: rule path exercised via "Allow for session" only; persistent save NOT-RUN. 3.3.0: PASS-SYNTH, also Always for contained edits and MCP tools (`9fff1d5c`) | **FAIL**: saved rule did not match next turn (F7). **3.3.0: PASS-SYNTH** (`codex_always_rule_from_the_prefill_matches_the_w0_request_next_time`) |
| Session-scoped auto-approval | **FAIL**: broadens to Accept edits (F1). **3.3.0: PASS-SYNTH** (`claude_allow_for_session_is_app_owned_and_exact`) | **PASS**: exact command auto-approved, `date` still asked (`08b`, `08d`) | **FAIL**: wedge and turn failure (F6). **3.3.0: PASS-SYNTH** (`codex_allow_for_session_replays_the_w0_frame`) |
| Pattern dialog | works but result ignored (F1). 3.3.0: result honoured; validates inline (`6c5d6f04`) | works (T1: `respond_with_rule_saves_custom_wildcard_rule_and_auto_approves_later`); loosened patterns NOT-RUN live. 3.3.0: a final `*` widens only an allowlist of read-only programs (`522a6ff9`) | pre-filled with the wrong text (F7). **3.3.0: PASS-SYNTH** (bare unwrapped command prefilled) |
| Permission export and import | NOT-RUN live (banner "Permissions Out of Sync: 480 external rules" seen, `11c`). 3.3.0: sources now come from a Settings list, not a hard-coded path (`cba980d1`; `no_agent_config_path_is_hard_coded`); export writes global rules only behind a warning (`982b3f92`) | NOT-RUN | CLAIMED: no Codex source or target (D5). 3.3.0: unchanged |
| Sync-discrepancy indicator | **PASS** (banner shown, reads the user's real permission files; `11c`) | NOT-RUN | n/a |
| agy hook installed and canary | n/a | **PASS** implicit: Ask offered, every tool routed to a card, canary never fired; no positive proof logged (F23). 3.3.0: three-state badge and journal verdict (`7e583b6e`, PASS-SYNTH) | n/a |
| agy hookless caveat | n/a | **NOT-RUN** (hook proven for the whole run) | n/a |
| Approvals across reconnect | PASS-SYNTH | PASS-SYNTH | **PASS**: stale card "Expired: the agent that asked has exited"; fresh `pwd` after reopen got a new card; repeated twice (`27`, `28a`, `28b`, `29`, `30`) |
| Codex non-command requests (permissions, MCP elicitation) | n/a | n/a | NOT-RUN live; PASS-SYNTH refused with a notice (`codex.rs:1354-1367`). 3.3.0: unchanged (D7) |
| Unanswered approval deadline | 3.3.0 (new row): PASS-SYNTH: a `can_use_tool` request left past 570 s is denied with a notice (`7739e3cd`, `an_unanswered_claude_request_is_denied_at_the_deadline`) | PASS-SYNTH (`an_unanswered_request_is_denied_and_expired_at_the_deadline`, existing) | n/a |
| Stop turn button | 3.3.0 (new row): PASS-SYNTH (`stop_turn_on_an_approval_ends_the_turn_interrupted`) | PASS-SYNTH (`agy_stop_turn_denies_the_hook_and_interrupts_the_turn`) | PASS-SYNTH (`w0_deny_sends_decline_and_stop_sends_cancel`); shown only when the server offers `cancel` |

### 4d. Shared experience

| Feature | Claude | agy | Codex |
|---|---|---|---|
| Queue (pinned, in order) | **PASS**: dashed bubbles, ALPHA then BETA (`08a`, `08b`) | **PASS** (`12a`, `12b`) | **PASS**: Q1 then Q2 (`18`, `19`). 3.3.0: native queue case added (`native_queue_stays_pinned_and_contained`) |
| Interrupt | **PASS**: process kept (one `session_started` across 14 turns); duplicate notices (F14). 3.3.0: notices deduplicated | **PASS**: process exits per caps; red "Turn failed" (F13); next prompt works (`13a`, `14a`). 3.3.0: no error card (`49801c70`) | **PASS**: process kept (`session_started` 3 = 1 + 2 reopens) |
| Model switch in session | **PASS**: live, no restart; two banners (`09c`). 3.3.0: one banner | **GAP-ok/DRIFT**: works via restart; not greyed (`17b`). 3.3.0: **Changed by design**: caps text corrected to say what happens (D3), and the thread's mode is kept (F22) | **PASS** luna to sol, pending then changed (`03a`, `03b`) |
| `/compact` | **PASS**: "Context compacted 45k -> 5.9k" (`09f`); agent then lost its cwd | **GAP-ok/DRIFT**: works as compact-by-handoff into a NEW thread (`15a`, `15c`). 3.3.0: notice now says a new thread opens; mode kept | **PASS**: gauge 29k to 6.1k (`22`, `24`) |
| Structured questions | **PASS**: card, answer reached agent (`09h`, `09l`). 3.3.0: the card moves to Answered (`3b2e3889`) | **NOT-RUN** (no trigger; caps false; absence only) | **NOT-RUN** (BLOCKED, not triggered); PASS-SYNTH `codex::tests::request_user_input_becomes_a_question_card_and_answers_with_question_ids`. 3.3.0: the Codex question **polyfill is Deferred** (separate PROPOSED plan 2026-10-09) |
| `@` file suggestions | **PASS** top-level list; `@src/ca` prefix NOT observed (`09j`, `09g`) | **GAP-ok**: hint omitted, no popup (`15b`). **3.3.0: PASS-SYNTH**: the app's own index (`git ls-files`, else a bounded walk; secret paths never listed) | **GAP-ok** (`23`). **3.3.0: PASS-SYNTH** (same index). Test for all three: `native_at_popover_lists_files_for_all_three_drivers` |
| Usage indicator | **PASS** `5h 12 %`, `wk 1 %` (`09n`) | **PASS** header bars; `/usage` raw dump (F21) (`16a`, `16b`). 3.3.0: `/usage` shows rows | **DRIFT**: caps false but `wk 94 %` bar shown; `/usage` hidden. 3.3.0: **Changed by design** (D2: the flag means the `/usage` panel only) |
| Context gauge | **DRIFT**: absent (F11). **3.3.0: PASS-SYNTH**: polled after each turn | **PASS** graceful "has not reported token usage yet" on a fresh thread. 3.3.0: polled after each turn | **PASS** `19k / 258k` |
| Account status | **PASS**: "Signed in as ****@gmail.com (Claude Pro)" (`11c`) | PASS: header and sidebar bars | PARTIAL: Settings row not opened (`26`) |
| Hand-off and cross-agent switch | **PASS**: Claude to agy, agy to Codex; notices "40 of 62" and "39 of 64"; canary absent (`11f`, `11g`, `11i`-`11l`); brief file on disk **NOT-RUN/UNVERIFIED**. 3.3.0: the switch notice also says "Session approvals (N) ended" (`14d0e669`, `session_rules_end_on_a_cross_agent_switch_and_always_rules_carry`) | PASS (receiving) | PASS (receiving); re-entry **FAIL** (F9). **3.3.0: PASS-SYNTH** re-entry continues in a new thread (`e47aeeee`) |
| Fork | **FAIL** (F8). 3.3.0: **Not reproduced**; PASS-SYNTH `a_fork_makes_a_second_thread_and_leaves_the_original_exactly_as_it_was` | NOT-RUN. 3.3.0: same test covers every agent | **FAIL** (F8). 3.3.0: **Not reproduced**, same test; `/fork` is a local built-in |
| Environment review | **PASS** launch and approval gating (Codex session in the folder; `11w`-`11z`); report completion NOT-RUN | NOT-RUN | PASS as the review host (raw JSON cards). 3.3.0: cards now show the command and folder (F16) |
| Store replay | PASS-SYNTH; chain history kept | **PASS** reopen replayed answers and thoughts (`03d`) | **PASS** reopen replayed 27 blocks |
| Redaction in the store | **PASS**: 0 rows for the full canary; `PARITY_CANARY_TOKEN=****0000`; Codex rollouts clean (`10`, `10b`) | **PASS** (user-prompt canary only; the read path is NOT-RUN) | **PASS** (seq 636, 659 `****0000`) |
| Redaction in the live view | not observed. 3.3.0: PASS-SYNTH for every event kind | **FAIL** user bubble unmasked (F5). **3.3.0: PASS-SYNTH** (prompt snapshot masked) | **FAIL** (F5). **3.3.0: PASS-SYNTH** (tool output, notice, error masked); ceiling: a secret split across two deltas shows until completion |
| Greyed controls | all caps true, nothing to grey. 3.3.0: every `caps::Feature` is a control or an explained absence (`dabc62bc`) | `/mcp` hidden PASS (`17a`); `@` omitted. 3.3.0: `@` now offered | `/mcp`, `/config`, `/usage` hidden (`22`). 3.3.0: typing an unbacked built-in such as `/mcp` answers with the note instead of being sent as a prompt |

## 5. DRIFT list (caps vs code vs docs vs live)

| ID | Item | Status after live | 3.3.0 status |
|---|---|---|---|
| D1 | `caps.rs:77` agy `streams_reasoning: false`, commit `c56486ea` | CONFIRMED by live: 23 reasoning items shown, one whole chunk each (`live-agy.md` 1a, 1b, "Settling the reasoning discrepancy"). Thoughts come from agy's on-disk transcript (`src/chat/session.rs:257,890`). No doc or release note mentions them; no consumer reads the flag | **Changed by design.** `dabc62bc` sets agy `streams_reasoning` true ("one whole block per step"); the adapter emits it Complete at once and never fakes streaming (`w0_agy_thinking_is_one_whole_block_never_streamed`). Transcript read moved off the main thread (`4f0f410a`, `f63c8e15`) |
| D1b | Claude `streams_reasoning: true` | CONTRADICTED by live: empty thinking text, no display mode requested (F10) | **Fixed** (F10) |
| D1c | Codex `streams_reasoning: true` | PARTLY: summary title only, one `summaryTextDelta` per item, and only on a model that reasons | **Changed by design.** Summaries are the contract; label "Summary" (`e02be258`); streaming beyond summaries is cut |
| D2 | Codex `usage: false` (`caps.rs:100`) | CONFIRMED: `/usage` hidden but `wk 94 %` bar shown from `account/rateLimits/updated` | **Changed by design.** `dabc62bc` documents the flag as the `/usage` panel only |
| D3 | `/compact` for agy with no `compact_command` and agy `model_switch_in_session: false` | CONFIRMED: both work (compact by handoff into a new thread; model switch by restart) and are not greyed. Caps text is misleading; the compact notice says "continues in a fresh session" while a new thread opens (`src/chat/view.rs:890-904`, `src/window/imp/threads.rs:3459`) | **Changed by design.** `dabc62bc`: the compact notice says a new thread opens; the behaviour is kept and described. I did not confirm the `model_switch_in_session` flag value was changed |
| D4 | Plan panel claimed for all agents | Only Codex has an emitter (`codex.rs:961`); not seen live on any agent. CLAIMED | **Changed by design.** Plan panels for agents with no emitter are on the cut list; the panel reveals only on a non-empty plan, so nothing was added (`a0f1b58a`) |
| D5 | v3.1.1 notes claim Codex `project.toml` import | Not in code (`src/always_allow.rs:239-262`); sources include a hard-coded `~/git/agent-config/sync/permissions.toml` | **Changed by design** for the path: `cba980d1` takes permission sources from Settings (U1; `no_agent_config_path_is_hard_coded`). The Codex import itself is still absent (not built) |
| D6 | Doc says the session-rule engine serves "AGY/Claude" | CONFIRMED wrong by live: Claude never uses it and ignores the dialog's rule (F1) | **Changed by design.** The claim is now true: one rule engine serves all three agents (`14d0e669`, `7ca166e9`, `c3bde6ac`) |
| D7 | Codex permissions and MCP elicitation requests | refused with a notice (`codex.rs:77-81`); NOT-RUN live | **Deferred.** No commit touches this; not in the plan's waves |
| D8 | agy renders question cards (plan text) vs caps false | NOT-RUN live; absence only | **Unverified.** No commit or test; missing: a trigger that makes agy ask a question |
| D9 | README: `@` mentions files | CONFIRMED: Claude only; agy and Codex hide the hint and show no popup | **Fixed.** `a0f1b58a`; `native_at_popover_lists_files_for_all_three_drivers` |
| D10 | README: "Codex history is not imported yet" | Stale since 3.1.0 (`import::tests::codex_transcript_imports_*`) | **Open (not fixed).** Verified: `README.md:229` still carries the sentence; no commit in `8502282f..HEAD` edits the README |
| D11 | Stale comments: `caps.rs:83-84` "Codex is not installed", `codex.rs:9-10` targets 0.160.1, plan row "codex NOT installed" | CONFIRMED stale: codex-cli 0.161.0 ran live. `caps.rs` Codex column was first recorded live in this run | **Fixed in part, Unverified remainder.** `dabc62bc` replaced the stale "Codex not installed" comments. `codex.rs:10` still says it targets codex-cli 0.160.1 |
| D12 | v3.1.0 notes: rules stored in `.agent-terminal/always_allow.toml`; `event.rs:291-293` "Codex never offers AllowAlways" | Both stale: actual file is `$XDG_STATE_HOME/agent-terminal/always-allow.json`; Codex does offer AllowAlways (`codex.rs:1405-1411`) | **Changed by design.** The store is now `approval-rules.json` with `schema_version`; the old `always-allow.json` is ignored (beta, no migration); an unparseable or newer file is never overwritten (`14d0e669`, `a_corrupt_or_newer_file_is_kept_byte_for_byte`). The v3.1.0 notes remain historical |
| D13 | Five caps fields gate nothing (`live_approvals`, `interrupt_keeps_process`, `streams_text`, `streams_reasoning`, `questions`) | CONFIRMED by grep (inventory section 2) | **Fixed.** `dabc62bc` adds `caps::Feature`, one per `Capabilities` field, each read by `/help`, built-in handling, the stop button or the mode picker; `b56dec33` guards `Feature::ALL` |
| D14 | Claude `context_usage: true` | No gauge shown live (F11), low confidence | **Fixed** (F11) |
| D15 | Hooked agy Plan mode | RESOLVED: edit was blocked by agy's own plan mode, not the hook (`live-agy.md` 6) | Unchanged (already resolved) |
| D16 | Accept edits "commands ask" (`adapter.rs:129`) | Contradicted on Codex (F2) and on Claude after F1 | **Fixed** (F1, F2) |

## 6. Fork and Codex re-entry failures (Claude chain)

The chain ran Claude, agy, Codex, then fork attempts and returns. Switches to agy and Codex worked, with notices stating how much history was carried, the canary absent from the injected context, and correct answers from the brief. Two failures followed:
- Fork: see F8. "Continue in a new thread" switched the same thread in place and `/fork` went to Codex as a prompt.
- Codex re-entry after Claude to agy to Codex to Claude to Codex: see F9, "no rollout found for thread id ...", every prompt fails, thread unusable.
Not verified: the brief file on disk (no `handoffs/` directory appeared in the preview state dir).

**3.3.0:** Fork: Not reproduced from the code (F8); the fork path was hardened and now has a test for every agent. Re-entry: fixed at the cause (F9); a missing rollout now continues in a new Codex thread with the handoff. Neither chain was replayed live. The brief file on disk is still unverified.

## 7. Test coverage gaps

Cells that are only T0 or only PASS-SYNTH, with no live proof:
- Sub-agents on all three agents, worktrees, plan panel (all agents), permission import and export actions, the sync widgets on agy and Codex.
- Codex non-command requests; Codex structured questions; agy structured questions.
- Fork (`fork_thread` is never called by a test); compact-by-handoff; session-level queue order after an interrupt.
- "Target branch" diff base and the Write/Edit label (earlier 3.3.0 work); agy on-disk diff fallback; the canary's denied-before-run exemption and 8-second grace.
- The Codex column against any real recorded stream: every Codex fixture is synthetic.

Live failures that no existing test would catch (tests pass at 811/0 and 7/7 native):
- F1: tests assert the broadening. F2 and F3: tests assert the policy table and option limiting, not the user-visible promise or the real server's offered list.
- F4: no read-scope test. F5: no test of the live view masking.
- F6, F7: tests use synthetic Codex input, not the dialog pre-fill, `zsh -lc` commands or a real `availableDecisions`.
- F8, F9: no fork test and no multi-leg re-entry test.
- F10, F11: fixtures contain thinking and context frames, but no test asserts what the UI renders.
- F20: no test parses Settings row titles as Pango markup.
- All 7 native tests and the 4 not-gated ignored tests are layout or measurement tests.

**3.3.0:**
- Closed by new tests: the Codex column now has real recorded frames (`tests/fixtures/real/codex/`: `ask-shell-decline.jsonl`, `ask-edit-decline.jsonl`, `untrusted-workspacewrite.jsonl`) replayed by `w0_deny_sends_decline_and_stop_sends_cancel`, `w0_session_and_always_answer_a_plain_accept`, `codex_accept_edits_auto_accepts_workspace_edits_and_shell_still_asks`, `rule_dialog_prefills_the_bare_w0_command`; F1 (`claude_allow_for_session_is_app_owned_and_exact`); F4 (`policy_table_read_containment`, `w0_agy_fixture_read_outside_the_project_asks_in_every_mode`); F5 (`a_secret_in_an_approval_or_prompt_reaches_the_view_masked`); F8 (fork for every agent); F9 (`scripted_resume_with_no_rollout_continues_in_a_new_codex_thread`); F10 and F11 (W0 replays, `native_the_context_gauge_is_polled_...`); F20 (`pages_have_renderable_titles`). The native suite also now clicks, checks toasts, queue, header and long text (14 native cases in the gate).
- Still open: sub-agents live on all three agents; worktrees live; permission export and import live (rule import now enforces `check(Scope::Always)`, `522a6ff9`); Codex non-command requests; agy and Codex structured questions (Codex polyfill deferred); compact-by-handoff live; the canary exemption and grace; the Codex `**` literals (F17); a Codex create-titled-"Edit" test; the agy model-switch default *model* (F22); a split-across-deltas secret in the live view; and the unchanged 4 ignored tests.

## 8. Not exercised

- Structured questions on agy (no trigger, caps false) and on Codex (not triggered; turn budget spent).
- The agy hookless caveat (edits in every mode without the hook): the hook was proven for the entire run.
- The agy secrets read path: agy refused to read `secrets.env` itself, so the file never reached the store. Redaction was proven only through a prompt-borne canary. Replay of the unmasked agy user bubble after reopen was not re-checked.
- The Claude thinking replay (nothing to replay; no close and reopen cycle completed).
- Codex Settings > Agents account row; Codex non-command requests; agy fork; agy and Codex permission export and import; worktree creation and removal; environment-review report completion; the handoff brief on disk; `@src/ca` prefix completion on Claude (not observed within 2.5 s, possibly timing).

**3.3.0:** Unchanged unless stated. In addition, for 3.3.0 the following were NOT re-exercised in the GUI: the 15-turn parity scenario, the Claude to agy to Codex to Claude to Codex chain, a multi-minute agy session reading outside the project, the live canary in the Codex and agy views, the agy hook badge, and the five-button approval card for Claude and agy.

## 9. Cost and cleanup

- Turns: Claude about 12 plus the chain, agy about 16 (plan said 12), Codex about 15 (3 over, mostly redaction and reconnect repeats). The Codex weekly meter read 94 % throughout and limited further Codex work.
- Harness notes (not product findings): Broadway pointer clicks are flaky (keyboard and move/down/up worked); the shared Playwright browser required selecting pages by URL; a stale `disconnected` dialog blocked one Codex call for 30 minutes; the preview TTL of 1800 s is short for a 20-turn agy run; app logs go to journald, so `preview_app_log` is nearly empty; the folder chooser cannot open in the preview (no portal backend), so `starting_directory` was seeded.
- Left behind outside the sandboxes (not removed):
  - `/home/jdesroches/.claude/plans/using-the-edit-tool-partitioned-torvalds.md` (Claude plan-mode file, created by the Claude run).
  - `~/.claude/projects/-tmp-parity-claude/` transcripts (contain the fake canary).
  - agy history entries and Codex rollouts (`~/.codex/sessions/2026/10/10`) for the test sessions.
- Cleaned: `/tmp/parity-codex` and preview pv_0005 (Codex run). Remove `target/parity/` and `docs/feature-parity-report.md` to roll the audit back.

**3.3.0:** the 3.3.0 status needed no new parity turns. Quota spent: W0 (Codex 4 turns, Claude 2, agy 1) and the live_smoke runs of 2026-10-11 (Claude 9 checks, Codex 16, agy 5; the plan budgeted about 4 turns per agent per run).

## 10. How this was verified (3.3.0)

**The 15-turn parity scenario was NOT re-run for 3.3.0.** No new live GUI audit was done; the plan schedules that re-run, with a regenerated report, after the combined release. Every 3.3.0 status above comes from four sources, and each status line names the one it rests on:

1. **Code and commits.** `git log 8502282f..HEAD` on master, with each fix named by short hash. The commit messages name the plan items (1.1 to 1.12, 2.1 to 2.7, 3.x, 4.x), the findings (F1, F5, F7, F8, F19, F20, F22) and the W0 frames they were built from.
2. **Regression tests.** Each test named above was found by name in `src/` or `crates/`. I read them or their commit diffs; I did not run `cargo test` for this report, so the current pass counts are not restated. Fixture-replay tests replay frames really recorded from the three CLIs in W0 (`tests/fixtures/real/`), not hand-written ones.
3. **The QA gate.** `tests/qa/gate.py` runs 14 native cases (7 at the beta audit). The plan Log records it green at `b2701e56`; it was not re-run at HEAD `324d5e0e` for this report.
4. **Live smoke contract runs.** `tests/qa/live_smoke.py` drives the real CLIs in a guarded temp sandbox and asserts contract facts only. Results of 2026-10-11:
   - Claude 9/9 passed: the thinking flag is accepted, and a `can_use_tool` deny is honoured.
   - Codex 16/16 passed: the wrapped command and `commandActions` arrive as the adapter expects; an unoffered `decline` is accepted; under `untrusted` + `workspaceWrite` an edit asks and an accept applies it.
   - agy 5/5 passed: `step_update` carries `tool_name` and `parameters`; the working-directory sandbox holds.
   These support F2, F3, F10 and the Codex title mapping (F17). They do not exercise the GUI, the approval hook, the rule engine, redaction in the view, or any multi-turn chain.

What a "Fixed" in this report therefore means: the code path changed, a test that fails on the old behaviour exists (named), and where a smoke check is named the real CLI also behaved as the code assumes. It does not mean the failure was re-driven in the GUI as in the beta audit.

### 3.3.0 tallies

Findings F1 to F23 (23):
| Status | Count | Items |
|---|---|---|
| Fixed | 19 | F1, F2, F3, F4, F5, F6, F7, F9, F10, F11, F12, F13, F14, F15, F16, F19, F21, F22, F23 |
| Of which also live-smoke corroborated | 3 | F2, F3, F10 |
| Fixed in part, remainder Unverified | 2 | F17 (Codex `**` literals; Codex create title test), F20 (worktree subtitle) |
| Not reproduced | 2 | F8, F18 |
| Changed by design | 0 | none |
| Deferred | 0 | none |

Drift list (18 rows: D1, D1b, D1c, D2 to D16):
| Status | Count | Items |
|---|---|---|
| Fixed | 5 | D1b, D9, D13, D14, D16 |
| Changed by design | 8 | D1, D1c, D2, D3, D4, D5, D6, D12 |
| Deferred | 1 | D7 |
| Open, not fixed | 1 | D10 |
| Unverified (fully or in part) | 2 | D8, D11 (remainder) |
| Already resolved before 3.3.0 | 1 | D15 |

UI audit (U1 to U11): 10 Fixed, 1 Fixed in part (U8). Matrix items beyond these lists: Deferred: Codex question polyfill (separate PROPOSED plan 2026-10-09), Codex non-command requests, Codex permission import; Changed by design: bypass mode and plan panels for agents with no emitter (cut list).

Unverified items to close before the regenerated parity report: gate and `cargo test` results at HEAD `324d5e0e`; the Codex `**` literals; the worktree subtitle markup (`imp.rs:3700`, from `2a7d207c`); the agy default model on model switch; whether the `model_switch_in_session` agy flag changed; `codex.rs:10` still naming 0.160.1; agy structured questions (D8); README.md:229; and the GUI-level re-drive of F1, F4, F5, F9, F23.
