# Cross-agent feature parity report (Claude, agy, Codex)

Target: agent-terminal 3.3.0 plus unreleased work, HEAD `8502282`. Date: 2026-10-10.
CLIs: claude 2.1.293, agy 1.3.3, codex-cli 0.161.0. Report only: no source was edited, committed, installed or built in release.

Evidence sources (all under `target/parity/` unless noted):
- `inventory.md` (T0 claims and T1 tests, Phase 1).
- `live-claude.md`, `live-agy.md`, `live-codex.md` (T2, driven in the GUI through `preview_app` and Playwright). Screenshots are in `shots/<agent>/NN-*.png`, named in the cells below by their number.
- `logs/cargo-test.log`, `logs/test-list.txt`, `logs/gate-native.log` (first, failed run), and `target/qa-artifacts/` (`gate.json`, `native-1.log` to `native-7.log`, `workspace.log`, `clippy.log`, `format.log`, `screenshots/`) for the passing gate re-run.
- Code references are file:line from reading the source. Anything marked "suspected" was not proven by an experiment.

Rule applied throughout: a live result overrides a claim. Where a runner reported BLOCKED or "not exercised", the cell is NOT-RUN and is not upgraded.

Verdict key: PASS (live), PASS-SYNTH (tests only), CLAIMED (docs or code only), GAP-ok (absent and correctly greyed or hidden), GAP-not-greyed (absent, offered anyway), FAIL, DRIFT, BLOCKED, NOT-RUN.

## 1. Executive summary

1. **Are thoughts clearly captured per agent? Partly, and not evenly.** agy shows 23 collapsible "Thought process" blocks, persisted and replayed, although caps says it does not stream reasoning; it arrives as one whole chunk per step, read from agy's on-disk transcript. Codex shows a collapsed block only on a model that produces reasoning, and it holds a single summary title with literal `**` markers, not progressive text. Claude shows nothing: the CLI sends the thinking block with empty text and the app never asks for a display mode, so 19 empty reasoning items render nothing. Verdict: agy yes (undocumented), Codex thin, Claude no.
2. **Do actions work? Yes, with cosmetic gaps.** Read, edit, write, shell, diff panel, per-turn checkpoints and undo passed live on all three agents, and undo left `git status` clean each time. Card titling is inconsistent on Codex (raw `/usr/bin/zsh -lc` argv, creates titled "Edit") and Codex has no search kind; Claude's ExitPlanMode and Codex's approval cards dump raw JSON.
3. **Does the approval system work? Not reliably, and several failures are security-relevant.** Claude "Allow for session" silently switches the thread to Accept edits, and a non-matching `mkdir` then ran unprompted. Codex Accept edits runs shell commands (including `cat secrets.env`) without asking. Codex deny sends `cancel` and kills the turn. Codex "Save for session" wedges the card and fails the turn, and its saved always-allow rule never matched. agy reads outside the project directory are not gated. Approve, plan-mode blocking, and agy's hook-backed Ask all worked; the Codex reconnect scenario from the blocker doc also held.
4. **Is the experience shared? Mostly for queueing, interrupt, model switch, compact, redaction in the store and cross-agent continuation. Not for fork, Codex re-entry, `@` files, questions, plan panel or the live redaction.** The queue, interrupt, model switch and `/compact` worked on all three (agy's via restart and handoff). Fork did not fork, returning to Codex after Claude to agy to Codex to Claude broke the thread with "no rollout found", and the live view shows the canary unmasked while persisted copies are scrubbed.

Findings: 5 High, 6 Medium, 12 Low/cosmetic (section 3).

## 2. Automated proof (T1)

| Run | Result | Evidence |
|---|---|---|
| `cargo test --workspace` | exit 0. **811 passed, 0 failed, 11 ignored** (822 listed). agent-core 258, agent-kit 142, agent-terminal bin 405 (+11 ignored), `tests/icons.rs` 6, doc-tests 0 | `logs/cargo-test.log`, `logs/test-list.txt` |
| `gate.py --native-only`, first run | FAILED before any test: `native-1: exit=127`, `Could not start test command ... 'xvfb-run'` (not installed at the time) | `logs/gate-native.log` |
| `gate.py --native-only`, after xvfb was installed | **PASSED 7/7 native tests** at `8502282` (`working_tree_dirty: true`, `delivery: false`, `native_only: true`) | `target/qa-artifacts/gate.json`, `native-1.log` to `native-7.log` |

What the 7 native tests cover (`tests/qa/native-tests.json`): long tool titles stay in the viewport, newest row stays in view while following, jump-to-latest, interruption shelf reveal and collapse, small window fit, completed plans retire to history, and `native_ux_matrix` (6 widths and scales, built on a Codex status backend). All are layout tests. None exercises an agent-specific feature, so none of the findings below could have been caught by them.
The 4 ignored tests not run by the gate: `coloured_buttons_keep_their_text_colour`, `a_thread_streamed_on_another_tab_shows_what_a_visible_one_does`, `stream_cost_of_a_long_thinking_block` (measurement only), `clear_rows_ends_and_keeps_an_open_menu`.
Codex tests (the 7 `scripted_tests.rs` cases that spawn `tests/support/scripted_codex.py`, plus the adapter tests) all passed, but they run against synthetic fixtures (`codex-synthetic-turn.ndjson`, `codex-schema-workers.ndjson`) and a fake `app-server`. Several live Codex failures below (deny, session scope, always-allow, re-entry) sit exactly where these fixtures could not reflect the real server.

## 3. Ranked findings

Severity: High = approval or secret-handling correctness, or the product does the opposite of what the UI says. Medium = feature fails or is misleading. Low = cosmetic or documentation. Security and approval items come first.

### High

**F1. Claude "Allow for session" broadens to Accept edits; a non-matching command ran unprompted.** Agents: Claude.
- Repro: Ask mode, trigger a `touch` approval, More, "Allow for session", choose `touch *` in "Confirm Session Approval Rule", "Save for Session & Allow". Next turn: `touch c.tmp` then `mkdir dd` both ran with no card; the mode chip had flipped from "Ask before edits" to "Accept edits".
- Evidence: `live-claude.md` step 5; store event 365 (permission suggestions were `addRules(Bash "touch b.tmp", localSettings)`, `addDirectories(/tmp/parity-claude, session)`, `setMode acceptEdits (session)`; resolution `allow_for_session`); screenshots `05a` to `05e`. The user's edited pattern was ignored.
- Suspected: `crates/agent-core/src/claude.rs:1312-1326` (turns `AllowForSession` into the CLI's own `updatedPermissions`, including its `setMode acceptEdits` suggestion) and `src/chat/session.rs:1553-1558` (only agy and Codex consume `custom_rule`); the dialog itself is driver-blind (`src/chat/view/cards.rs:1668-1696`).
- Tests: `claude::tests::always_allow_writes_only_rules_to_local_settings_and_session_stays_in_memory` and `encode_approve_allow_deny_cancel_session` assert the `setMode acceptEdits` output, so the tests lock in the broadening. No test covers "a command that does not match the chosen pattern still asks".

**F2. Codex Accept edits runs shell commands without asking.** Agents: Codex.
- Repro: Accept edits mode; ask for `ls`, `sleep 25` and `cat secrets.env`. All ran with no card (the scenario expects shell to still ask). In Ask mode even read-only commands asked.
- Evidence: `live-codex.md` step 7; screenshots `12`, `18`.
- Suspected: `crates/agent-core/src/codex.rs:1996` maps AcceptEdits to `on-request` + `workspaceWrite`, so the sandbox runs commands unasked; the adapter and README still promise "commands ask" (`crates/agent-core/src/adapter.rs:129`).
- Tests: `codex::tests::prompt_encodes_turn_start_with_the_mode_policy` and `deferred_modes_preserve_accept_edits_cancel_and_apply_only_to_the_next_turn` assert this mapping. Nothing checks the user-visible promise.

**F3. Codex deny is `cancel`, which ends the whole turn.** Agents: Codex.
- Repro: Ask mode, any command approval, press the only negative button, "Cancel". Card shows "Declined", then "Interrupted"; the agent never sees a denial and cannot continue.
- Evidence: `live-codex.md` step 4a; store row 127 `approval_resolved cancel`, row 133 `turn_completed interrupted`; screenshot `05b`.
- Suspected: `crates/agent-core/src/codex.rs:1385-1411` builds the option list from the server's `availableDecisions`; when `decline` is not in that list only Cancel is shown. `decision_wire` maps Deny to `decline` (`codex.rs:2015`) but it is never offered.
- Tests: `codex::tests::available_decisions_limit_the_options` and `a_decision_the_server_did_not_offer_is_refused_and_the_request_stays_open` exercise the limiting with synthetic frames. No test models what the real server offers.

**F4. agy reads outside the project directory are ungated in Ask mode.** Agents: agy.
- Repro: Ask mode, deny `ls -la`; agy then guessed paths for about two minutes and read `/home/jdesroches/.gemini/config/hooks.json` and the sibling sandbox `/tmp/parity-claude/.git/*` with no approval card.
- Evidence: `live-agy.md` row 4c and "Other findings" 1; screenshots `02c` to `02e`. The runner notes this is by design for reads.
- Suspected: `src/approval_server.rs:72-110` (`classify` and `mutates`): read tools are not queried at all, so no path containment applies to reads.
- Tests: `approval_server::tests::policy_table` and the symlink and workspace-containment tests cover edits only. Nothing covers read scope.

**F5. The live view shows the canary unmasked; only persisted and replayed copies are masked.** Agents: agy (user bubble), Codex (tool output and echo). Claude: not observed.
- Repro: put `sk-parity-canary-0000000000000000` in a prompt (agy) or have Codex print `secrets.env` verbatim. Live view shows the full value. After reopening, the replayed view and the DB show `****0000`.
- Evidence: `live-agy.md` step 10 (store: 0 rows for the value, user bubble live unmasked; replay after reopen not re-checked); `live-codex.md` step 10 (live render `PARITY_CANARY_TOKEN=sk-parity-canary-0000000000000000`, replay `****0000`; screenshots `21`, `27`).
- Suspected: scrubbing happens only in the store, `crates/agent-kit/src/store.rs:171` (`scrub_envelope`), while `src/chat/session.rs:1470-1485` delivers the raw prompt snapshot to the view first and live envelopes are not scrubbed before display.
- Tests: `store::tests::a_planted_token_is_masked_in_raw_and_text` and `redact::tests::*` prove the store path. No test asserts that the live view masks.

### Medium

**F6. Codex "Save for Session" is offered, then wedges the card and fails the turn.** Agents: Codex.
- Repro: Ask mode, command approval, More, Always allow dialog, Save for Session. Result: red `Turn failed: invalid command: Codex did not offer acceptForSession for approval ...`; card stuck on "Allow for session..." with greyed buttons until Stop (Stop recovers cleanly).
- Evidence: `live-codex.md` step 5; screenshots `07a` to `07d`.
- Suspected: `crates/agent-core/src/codex.rs:1739` rejects a decision the server did not offer, while the UI offers it regardless (`src/chat/view/cards.rs:1700`, `src/chat/session.rs:1559-1599`).
- Tests: `codex::tests::a_decision_the_server_did_not_offer_is_refused_and_the_request_stays_open` covers the refusal but not the wedged UI, and no test covers the UI offering it.

**F7. Codex "Always allow" does not persist as an effective rule, and the dialog is pre-filled with the wrong text.** Agents: Codex.
- Repro: Always allow, Save Always & Allow on `git status --short`. Dialog pattern field read a footer sentence ("`...atus --short` in /tmp/parity-codex") and "Permit all tools on ``". The rule saved ("Always allowed here"), but the identical command asked again in a new turn. A non-matching `git log --oneline` asked as expected.
- Evidence: `live-codex.md` step 5; screenshots `08a` to `08c`. The runner's mismatch theory (the `zsh -lc` wrapper) is UNVERIFIED.
- Suspected: `src/chat/view/cards.rs:1660` pre-fills the dialog from `a.remembers`, which `crates/agent-core/src/codex.rs:1378` formats as the sentence `` `cmd` in cwd ``; that string becomes the rule pattern. Matching then happens in `src/chat/session.rs:1048-1094`.
- Tests: `chat::session::tests::codex_session_approval_rule_auto_approves_matching_commands` passes with synthetic input. It does not use the dialog's pre-fill or a `zsh -lc` command.

**F8. Fork does not fork.** Agents: Codex, Claude (the chain used both).
- Repro: sidebar context menu "Continue in a new thread", Claude then Default model, and Codex then Default model: both switched the same thread in place (store keeps one thread, notice "Continuing in Claude/Codex with 39 of 68 earlier messages"). Typing `/fork` was sent to Codex as an ordinary prompt.
- Evidence: `live-claude.md` step 11 "Fork"; screenshots `11a`, `11o` to `11u`. The runner notes the popover was driven by keyboard and hover, so a mis-target of "Switch this thread to" is possible; the submenu header read "Continue in a new thread".
- Suspected: the code reads as if `/fork` should work (`src/chat/view.rs:925`, `src/window/imp/threads.rs:3459-3473`); why the live run differs is unknown. UNVERIFIED cause.
- Tests: none call `fork_thread`.

**F9. Re-entering Codex after Claude to agy to Codex to Claude fails with "no rollout found".** Agents: Codex (via the Claude chain).
- Repro: switch Claude, agy, Codex, back to Claude, then Codex again. Every later prompt fails with `Could not switch to gpt-6.1-sol: no rollout found for thread id 01a1282c-...` and `Turn failed: no rollout found for thread id ...`; the chip shows gpt-6.1-sol. The thread is unusable. No workaround attempted.
- Evidence: `live-claude.md` step 11; screenshot `11u`.
- Suspected: a stale Codex thread id is resumed (`crates/agent-core/src/codex.rs` handshake with `thread/resume`; stale id kept by `src/chat/session.rs` provider-thread handling). UNVERIFIED.
- Tests: `chat::session::scripted_tests::scripted_codex_claude_codex_switch_routes_replies_and_recovers_a_failed_switch` covers one round trip with a fake server. No test goes through a second Claude to Codex re-entry or a missing rollout.

**F10. Claude reasoning never appears.** Agents: Claude.
- Repro: ask a "think step by step" prompt. No thinking block. Store has 19 `item_started kind=reasoning` plus bare `item_completed` with zero reasoning `content_delta`. Raw frames carry `"thinking_display":"updates"` and `{"type":"thinking","thinking":"","signature":""}`.
- Evidence: `live-claude.md` step 1; screenshots `01a` to `01d`.
- Suspected: the app never requests a thinking display mode (`crates/agent-core/src/claude.rs:1034-1060` argv has none; the runner's grep for `thinking-display` or `show-thinking` found nothing). The adapter handles `thinking_delta` (`claude.rs:504`) but the CLI sends none. Empty reasoning items are emitted and render nothing.
- Tests: no test names thinking. Fixtures `claude-interrupt`, `claude-turn-approval`, `claude-controls-slash` contain `thinking` frames, but none checks that text reaches the UI.

**F11. Claude context gauge is absent although caps says `context_usage: true`.** Agents: Claude.
- Evidence: `live-claude.md` step 9 (usage shown; no gauge like Codex's `29k / 258k`; context only appears as the compact divider "45k -> 5.9k"); screenshot `09n`. The runner marks it unverified (may need a control round trip). Verdict DRIFT, low confidence.
- Tests: `claude::tests::context_usage_reply_updates_the_gauge` passes with synthetic input.

### Low and cosmetic

| ID | Agent | Issue | Evidence |
|---|---|---|---|
| F12 | Claude, agy | A denial shows the card label "Denied" in green | `live-claude.md` step 4 (`04b`), `live-agy.md` 4a (`06c`) |
| F13 | agy | Interrupt renders a red "Turn failed: interrupted" card | `live-agy.md` 8b (`13a`) |
| F14 | Claude | Interrupt shows two notices ("[Request interrupted by user]" and "Interrupted") | `live-claude.md` step 8 (`08c`) |
| F15 | Claude | ExitPlanMode approval card is raw JSON, not a rendered plan | `live-claude.md` step 6 (`06c`) |
| F16 | Codex | Approval cards dump the full raw JSON request (`availableDecisions`, ids, `startedAtMs`) | `live-codex.md` step 4a (`05b`); `live-claude.md` step 11 |
| F17 | Codex | Command cards are titled with raw `/usr/bin/zsh -lc ...` argv; creates are titled "Edit"; reasoning shows literal `**` | `live-codex.md` steps 1, 2, 3 (`04c`, `06a`, `12` to `17`) |
| F18 | Claude | Diff panel stays on the previous turn's two files after an undo, until a later change | `live-claude.md` step 3 (`03i`, `04b`) |
| F19 | Claude | Model switch shows two banners; the question card keeps "Sending..." after completion | `live-claude.md` step 9 (`09c`, `09l`) |
| F20 | Claude | GTK markup errors on startup and in Settings > Agents: unescaped `&` in "In-App Approvals & Pattern Matching" and "Confirm & Customise Patterns on Save" (empty row title), and `<folder>/<repo>/<branch>` in the worktree subtitle | `live-claude.md` harness notes (`11c`) |
| F21 | agy | `/usage` panel is a raw key and path dump | `live-agy.md` 9f (`16b`) |
| F22 | agy | Compact-by-handoff and model switch silently adopt the profile default model and mode (after an accidental "Make Default" the new thread jumped to gemini-3.8-flash-high and Plan); the "Make Default" toast also captures clicks on the composer | `live-agy.md` Other findings 2, 6 (`15a`, `15c`) |
| F23 | agy | No positive hook proof anywhere: the journal only logs `approval socket listening`, no "hook verified" line and no UI badge | `live-agy.md` Hook status, row 11 |

## 4. Matrix

Cell format: VERDICT, then evidence. "T2" is live, "T1" test name, "T0" claim. Screenshot numbers refer to `shots/<agent>/NN-*.png`.

### 4a. Thoughts

| Feature | Claude | agy | Codex |
|---|---|---|---|
| Reasoning captured and shown | **DRIFT**: nothing rendered; empty thinking text (F10); `live-claude.md` s1 | **PASS**: 23 "Thought process" blocks; `live-agy.md` 1a (`02c`, `02h`, `02i`) | **PASS** on `gpt-6-sol` only; `gpt-6-luna` gave 0 reasoning tokens; `live-codex.md` s1 (`11`, `27`) |
| Streamed progressively | **DRIFT**: caps true, nothing arrives | **GAP-ok**: one whole chunk per step (23 items, 23 deltas vs 298 assistant deltas); caps false is accurate but see D1 | **DRIFT**: caps true, but one summary title per item (single `summaryTextDelta`) |
| Collapsible | NOT-RUN (no block) | **PASS** (`02h`) | **PASS** |
| Persisted and replayed after close and reopen | NOT-RUN: nothing to replay; reopen cycle not completed | **PASS** (`03d`) | **PASS** (27 blocks replayed after reopen) |
| Not carried in hand-off | PASS-SYNTH: `store::tests::transcript_leaves_out_reasoning`; chain notice "Reasoning and tool state are not carried" (`11f`) | same | same |
| Native import of thinking | PASS-SYNTH: `import::tests::assistant_text_and_thinking_use_the_live_item_kinds` | PASS-SYNTH: same family | PASS-SYNTH: `import::tests::codex_transcript_imports_*` |

### 4b. Actions

| Feature | Claude | agy | Codex |
|---|---|---|---|
| Read and search cards | **PASS**: `Read`, `Run`, `Search` titled; no Grep or Glob in this CLI build; `02` | **PASS** read; **GAP-ok** no search kind (agy uses `run_command`); `02g`, `04a` | **DRIFT**: raw `zsh -lc` titles, no semantic Read or Search (F17); `04c`, `06a`, `06c` |
| Edit and new-file card with diff | **PASS** `03a` to `03d` (approval card with unified diff) | **PASS** `05a` to `05e` (`Edit` and `Write`, `+2 -0`, `/dev/null` to notes.txt) | **PASS** minor DRIFT: create titled "Edit"; `12` to `15` |
| Diff vs turn baseline ("Changes this turn") | PASS `03f` | PASS `05e` | PASS `14`, `15` |
| Diff panel bases | **PASS** (Uncommitted, Target branch, Last turn, This tab) `03e`; stale after undo (F18) | PASS `05f`-`05h` | PASS `16` |
| Checkpoint refs | PASS: `refs/agent-terminal/<id>/0001`, `0002` | PASS: journal `Checkpoint ... 0001..0003` | PASS: hidden refs `0001`, `0002`, plus `pre-restore-*` |
| Undo (restore, itself undoable) | **PASS** `03g`-`03i` (`git status` clean) | **PASS** `05i`, `05j` | **PASS** `17` |
| Plan panel (steps) | CLAIMED/DRIFT: no emitter in `claude.rs`; not seen live | CLAIMED/DRIFT: no emitter | PASS-SYNTH: `codex.rs:961`, `codex::tests::plan_updates_errors_and_reroutes`; not seen live |
| Sub-agents | PASS-SYNTH: `claude::tests::a_background_sub_agent_runs_past_its_launch_result_and_the_turn_end` | CLAIMED: card only (`agy.rs:773`) | PASS-SYNTH: `codex::tests::collaboration_frames_publish_workers_and_explicit_waits` |
| Worktrees | PASS-SYNTH: `worktree::tests::*`; NOT-RUN live | PASS-SYNTH; NOT-RUN | PASS-SYNTH; NOT-RUN |

### 4c. Approvals

| Feature | Claude | agy | Codex |
|---|---|---|---|
| Ask: approve and deny, agent sees result | **PASS** (deny text reached the agent; `04a`-`04d`) | **PASS** (agent quoted the hook denial; `06c`, `07a`) | **FAIL** deny = cancel ends the turn (F3); approve **PASS** `06b`, `06c` |
| Accept edits: edit applies, shell asks | **PASS**: edit unprompted, `python3 -c` asked (`07`) | **PASS**: edit unprompted, `echo` asked and denied (`11a`) | **FAIL/DRIFT**: edit applied, shell also ran unprompted (F2) |
| Plan blocks edits | **PASS**: no edit; plan file written; ExitPlanMode asked (`06a`-`06d`) | **PASS** with hook: no edit, no card, block came from agy's own plan mode not the hook (`10e`) | **PASS**: read-only sandbox rejected the patch (`10`) |
| Bypass mode | GAP-ok: no such app mode (`adapter.rs:125`); Claude `bypassPermissions` ignored (`claude.rs:1404`); NOT-RUN | GAP-ok: skip flag is internal only | GAP-ok |
| Always-allow (persistent) | **FAIL**-adjacent: Claude writes its own rule; the edited pattern is ignored (F1) | **PASS-ish**: rule path exercised via "Allow for session" only; persistent save NOT-RUN | **FAIL**: saved rule did not match next turn (F7) |
| Session-scoped auto-approval | **FAIL**: broadens to Accept edits (F1) | **PASS**: exact command auto-approved, `date` still asked (`08b`, `08d`) | **FAIL**: wedge and turn failure (F6) |
| Pattern dialog | works but result ignored (F1) | works (T1: `respond_with_rule_saves_custom_wildcard_rule_and_auto_approves_later`); loosened patterns NOT-RUN live | pre-filled with the wrong text (F7) |
| Permission export and import | NOT-RUN live (banner "Permissions Out of Sync: 480 external rules" seen, `11c`) | NOT-RUN | CLAIMED: no Codex source or target (D5) |
| Sync-discrepancy indicator | **PASS** (banner shown, reads the user's real permission files; `11c`) | NOT-RUN | n/a |
| agy hook installed and canary | n/a | **PASS** implicit: Ask offered, every tool routed to a card, canary never fired; no positive proof logged (F23) | n/a |
| agy hookless caveat | n/a | **NOT-RUN** (hook proven for the whole run) | n/a |
| Approvals across reconnect | PASS-SYNTH | PASS-SYNTH | **PASS**: stale card "Expired: the agent that asked has exited"; fresh `pwd` after reopen got a new card; repeated twice (`27`, `28a`, `28b`, `29`, `30`) |
| Codex non-command requests (permissions, MCP elicitation) | n/a | n/a | NOT-RUN live; PASS-SYNTH refused with a notice (`codex.rs:1354-1367`) |

### 4d. Shared experience

| Feature | Claude | agy | Codex |
|---|---|---|---|
| Queue (pinned, in order) | **PASS**: dashed bubbles, ALPHA then BETA (`08a`, `08b`) | **PASS** (`12a`, `12b`) | **PASS**: Q1 then Q2 (`18`, `19`) |
| Interrupt | **PASS**: process kept (one `session_started` across 14 turns); duplicate notices (F14) | **PASS**: process exits per caps; red "Turn failed" (F13); next prompt works (`13a`, `14a`) | **PASS**: process kept (`session_started` 3 = 1 + 2 reopens) |
| Model switch in session | **PASS**: live, no restart; two banners (`09c`) | **GAP-ok/DRIFT**: works via restart; not greyed (`17b`) | **PASS** luna to sol, pending then changed (`03a`, `03b`) |
| `/compact` | **PASS**: "Context compacted 45k -> 5.9k" (`09f`); agent then lost its cwd | **GAP-ok/DRIFT**: works as compact-by-handoff into a NEW thread (`15a`, `15c`) | **PASS**: gauge 29k to 6.1k (`22`, `24`) |
| Structured questions | **PASS**: card, answer reached agent (`09h`, `09l`) | **NOT-RUN** (no trigger; caps false; absence only) | **NOT-RUN** (BLOCKED, not triggered); PASS-SYNTH `codex::tests::request_user_input_becomes_a_question_card_and_answers_with_question_ids` |
| `@` file suggestions | **PASS** top-level list; `@src/ca` prefix NOT observed (`09j`, `09g`) | **GAP-ok**: hint omitted, no popup (`15b`) | **GAP-ok** (`23`) |
| Usage indicator | **PASS** `5h 12 %`, `wk 1 %` (`09n`) | **PASS** header bars; `/usage` raw dump (F21) (`16a`, `16b`) | **DRIFT**: caps false but `wk 94 %` bar shown; `/usage` hidden |
| Context gauge | **DRIFT**: absent (F11) | **PASS** graceful "has not reported token usage yet" on a fresh thread | **PASS** `19k / 258k` |
| Account status | **PASS**: "Signed in as ****@gmail.com (Claude Pro)" (`11c`) | PASS: header and sidebar bars | PARTIAL: Settings row not opened (`26`) |
| Hand-off and cross-agent switch | **PASS**: Claude to agy, agy to Codex; notices "40 of 62" and "39 of 64"; canary absent (`11f`, `11g`, `11i`-`11l`); brief file on disk **NOT-RUN/UNVERIFIED** | PASS (receiving) | PASS (receiving); re-entry **FAIL** (F9) |
| Fork | **FAIL** (F8) | NOT-RUN | **FAIL** (F8) |
| Environment review | **PASS** launch and approval gating (Codex session in the folder; `11w`-`11z`); report completion NOT-RUN | NOT-RUN | PASS as the review host (raw JSON cards) |
| Store replay | PASS-SYNTH; chain history kept | **PASS** reopen replayed answers and thoughts (`03d`) | **PASS** reopen replayed 27 blocks |
| Redaction in the store | **PASS**: 0 rows for the full canary; `PARITY_CANARY_TOKEN=****0000`; Codex rollouts clean (`10`, `10b`) | **PASS** (user-prompt canary only; the read path is NOT-RUN) | **PASS** (seq 636, 659 `****0000`) |
| Redaction in the live view | not observed | **FAIL** user bubble unmasked (F5) | **FAIL** (F5) |
| Greyed controls | all caps true, nothing to grey | `/mcp` hidden PASS (`17a`); `@` omitted | `/mcp`, `/config`, `/usage` hidden (`22`) |

## 5. DRIFT list (caps vs code vs docs vs live)

| ID | Item | Status after live |
|---|---|---|
| D1 | `caps.rs:77` agy `streams_reasoning: false`, commit `c56486ea` | CONFIRMED by live: 23 reasoning items shown, one whole chunk each (`live-agy.md` 1a, 1b, "Settling the reasoning discrepancy"). Thoughts come from agy's on-disk transcript (`src/chat/session.rs:257,890`). No doc or release note mentions them; no consumer reads the flag |
| D1b | Claude `streams_reasoning: true` | CONTRADICTED by live: empty thinking text, no display mode requested (F10) |
| D1c | Codex `streams_reasoning: true` | PARTLY: summary title only, one `summaryTextDelta` per item, and only on a model that reasons |
| D2 | Codex `usage: false` (`caps.rs:100`) | CONFIRMED: `/usage` hidden but `wk 94 %` bar shown from `account/rateLimits/updated` |
| D3 | agy `/compact` with no `compact_command` and agy `model_switch_in_session: false` | CONFIRMED: both work (compact by handoff into a new thread; model switch by restart) and are not greyed. Caps text is misleading; the compact notice says "continues in a fresh session" while a new thread opens (`src/chat/view.rs:890-904`, `src/window/imp/threads.rs:3459`) |
| D4 | Plan panel claimed for all agents | Only Codex has an emitter (`codex.rs:961`); not seen live on any agent. CLAIMED |
| D5 | v3.1.1 notes claim Codex `project.toml` import | Not in code (`src/always_allow.rs:239-262`); sources include a hard-coded `~/git/agent-config/sync/permissions.toml` |
| D6 | Doc says the session-rule engine serves "AGY/Claude" | CONFIRMED wrong by live: Claude never uses it and ignores the dialog's rule (F1) |
| D7 | Codex permissions and MCP elicitation requests | refused with a notice (`codex.rs:77-81`); NOT-RUN live |
| D8 | agy renders question cards (plan text) vs caps false | NOT-RUN live; absence only |
| D9 | README: `@` mentions files | CONFIRMED: Claude only; agy and Codex hide the hint and show no popup |
| D10 | README: "Codex history is not imported yet" | Stale since 3.1.0 (`import::tests::codex_transcript_imports_*`) |
| D11 | Stale comments: `caps.rs:83-84` "Codex is not installed", `codex.rs:9-10` targets 0.160.1, plan row "codex NOT installed" | CONFIRMED stale: codex-cli 0.161.0 ran live. `caps.rs` Codex column was first recorded live in this run |
| D12 | v3.1.0 notes: rules stored in `.agent-terminal/always_allow.toml`; `event.rs:291-293` "Codex never offers AllowAlways" | Both stale: actual file is `$XDG_STATE_HOME/agent-terminal/always-allow.json`; Codex does offer AllowAlways (`codex.rs:1405-1411`) |
| D13 | Five caps fields gate nothing (`live_approvals`, `interrupt_keeps_process`, `streams_text`, `streams_reasoning`, `questions`) | CONFIRMED by grep (inventory section 2) |
| D14 | Claude `context_usage: true` | No gauge shown live (F11), low confidence |
| D15 | Hooked agy Plan mode | RESOLVED: edit was blocked by agy's own plan mode, not the hook (`live-agy.md` 6) |
| D16 | Accept edits "commands ask" (`adapter.rs:129`) | Contradicted on Codex (F2) and on Claude after F1 |

## 6. Fork and Codex re-entry failures (Claude chain)

The chain ran Claude, agy, Codex, then fork attempts and returns. Switches to agy and Codex worked, with notices stating how much history was carried, the canary absent from the injected context, and correct answers from the brief. Two failures followed:
- Fork: see F8. "Continue in a new thread" switched the same thread in place and `/fork` went to Codex as a prompt.
- Codex re-entry after Claude to agy to Codex to Claude to Codex: see F9, "no rollout found for thread id ...", every prompt fails, thread unusable.
Not verified: the brief file on disk (no `handoffs/` directory appeared in the preview state dir).

## 7. Test coverage gaps

Cells that are only T0 or only PASS-SYNTH, with no live proof:
- Sub-agents on all three agents, worktrees, plan panel (all agents), permission import and export actions, the sync widgets on agy and Codex.
- Codex non-command requests; Codex structured questions; agy structured questions.
- Fork (`fork_thread` is never called by a test); compact-by-handoff; session-level queue order after an interrupt.
- "Target branch" diff base and the Write/Edit label (3.3.0); agy on-disk diff fallback; the canary's denied-before-run exemption and 8-second grace.
- The Codex column against any real recorded stream: every Codex fixture is synthetic.

Live failures that no existing test would catch (tests pass at 811/0 and 7/7 native):
- F1: tests assert the broadening. F2 and F3: tests assert the policy table and option limiting, not the user-visible promise or the real server's offered list.
- F4: no read-scope test. F5: no test of the live view masking.
- F6, F7: tests use synthetic Codex input, not the dialog pre-fill, `zsh -lc` commands or a real `availableDecisions`.
- F8, F9: no fork test and no multi-leg re-entry test.
- F10, F11: fixtures contain thinking and context frames, but no test asserts what the UI renders.
- F20: no test parses Settings row titles as Pango markup.
- All 7 native tests and the 4 not-gated ignored tests are layout or measurement tests.

## 8. Not exercised

- Structured questions on agy (no trigger, caps false) and on Codex (not triggered; turn budget spent).
- The agy hookless caveat (edits in every mode without the hook): the hook was proven for the entire run.
- The agy secrets read path: agy refused to read `secrets.env` itself, so the file never reached the store. Redaction was proven only through a prompt-borne canary. Replay of the unmasked agy user bubble after reopen was not re-checked.
- The Claude thinking replay (nothing to replay; no close and reopen cycle completed).
- Codex Settings > Agents account row; Codex non-command requests; agy fork; agy and Codex permission export and import; worktree creation and removal; environment-review report completion; the handoff brief on disk; `@src/ca` prefix completion on Claude (not observed within 2.5 s, possibly timing).

## 9. Cost and cleanup

- Turns: Claude about 12 plus the chain, agy about 16 (plan said 12), Codex about 15 (3 over, mostly redaction and reconnect repeats). The Codex weekly meter read 94 % throughout and limited further Codex work.
- Harness notes (not product findings): Broadway pointer clicks are flaky (keyboard and move/down/up worked); the shared Playwright browser required selecting pages by URL; a stale `disconnected` dialog blocked one Codex call for 30 minutes; the preview TTL of 1800 s is short for a 20-turn agy run; app logs go to journald, so `preview_app_log` is nearly empty; the folder chooser cannot open in the preview (no portal backend), so `starting_directory` was seeded.
- Left behind outside the sandboxes (not removed):
  - `/home/jdesroches/.claude/plans/using-the-edit-tool-partitioned-torvalds.md` (Claude plan-mode file, created by the Claude run).
  - `~/.claude/projects/-tmp-parity-claude/` transcripts (contain the fake canary).
  - agy history entries and Codex rollouts (`~/.codex/sessions/2026/10/10`) for the test sessions.
- Cleaned: `/tmp/parity-codex` and preview pv_0005 (Codex run). Remove `target/parity/` and `docs/feature-parity-report.md` to roll the audit back.
