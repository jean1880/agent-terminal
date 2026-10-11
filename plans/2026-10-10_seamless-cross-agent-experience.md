# Seamless cross-agent experience: closing the parity and UI gaps

Status: APPROVED (plan-reviewer round 2; user CHALLENGE answered); W0 running · 2026-10-10 · HEAD `8502282` (3.3.0)
Hive: topic #29 `agent-terminal-seamless-parity`. Round 1 is #301–#318, Round 2 is #319–#321, and the confirmed matcher bypass is #310.
Evidence:
- `docs/feature-parity-report.md`;
- `target/parity/{inventory,live-claude,live-agy,live-codex,ui-audit}.md`;
- the agy red-team verdict (CHANGES-REQUIRED, 4 blockers).

## Goal
A user configures one or more of Claude, agy and Codex and switches between them freely. Every thread looks and behaves the same, whichever agent drives it:
- **Thoughts:** one reasoning block.
- **Actions:** one card grammar.
- **Approvals:** one approval card whose buttons mean the same thing everywhere.
- **Controls:** one set.

Where an agent truly cannot do something, the control is visibly absent and says why; it never fails silently or means something different. Separately, every surface must be pressable, selectable and copyable, and must stay inside its container.

## The UX contract (what "the same" means)
| Surface | Uniform behaviour | Claude | agy | Codex |
|---|---|---|---|---|
| Reasoning block | One widget with four states: *streaming*, *complete*, *replayed*, *not shared by this model*. It is collapsible, persisted and replayed. | native, via `--thinking-display summarized` (W0 spike) | whole-block from the transcript (no fake streaming) | reasoning summaries; label is "summary" |
| Tool card title | `Read <path>`, `Search <pattern>`, `Run <command>`, `Edit <path>`, `Create <path>`, `Fetch <url>`, `<server>/<tool>`. Raw argv and JSON go behind **Details**. | native | native | unwrap `zsh -lc`; use `commandActions` |
| Approval card | Buttons: **Allow once** · **Allow for session** (this exact command or pattern, this agent, this workspace) · **Always allow** (persisted rule) · **Deny** (the agent sees the denial and continues) · **Stop turn**. All five buttons are shown for every agent (W0 proved Codex honours `decline`); `cancel` is never labelled "Deny". | app-owned rules | app-owned (hook) | app-owned; `decline` for Deny, `cancel` for Stop |
| Modes | **Ask:** every edit and shell command asks. **Accept edits:** in-workspace edits auto-allowed, shell asks. **Plan:** nothing writes. The chip must be true, or it carries a warning style and tooltip. | native | native, hook-backed | `untrusted` plus app auto-allow for edits (W0 probe), else relabelled |
| Secrets | Redacted in the live view, approvals, saved rules, store and handoffs. | | | |
| Switching | Context and handoff carry over. Session approvals end, and the switch notice says so. Always-rules persist and show their origin. Fork creates a new thread. Returning to an agent always works. | | | |
| Gaps | Greyed out with a tooltip ("Codex has no MCP panel"), or polyfilled by the app: `@` files from the app's own index, questions per the existing polyfill plan. | | | |

## Decisions (from deliberation)
- **D1. agent-terminal owns every approval decision for every agent (architect option A), built in targeted increments (option B order).**
  - The app never forwards a CLI's own scope or mode change. For Claude, Allow for session and Always no longer send `updatedPermissions` at all; neither `setMode` nor `addRules` is sent to `.claude/settings.local.json`.
  - The app records the rule in its own store (`always_allow`) and answers later matching `can_use_tool` requests itself. Claude, agy and Codex rules then live in one place, matched by one matcher and shown in one list, which U2 requires.
  - Rejected: writing one rule set into each CLI's config (option C, which is how the two-owner mess arose); a new approval crate; forwarding `addRules` to Claude's own store, which would leave a second matcher the app can't see (plan-reviewer, round 1).
- **D2. The rule matcher is replaced first, before any change that adds auto-approve paths (#310).**
  - A pure, std-only `simple_command_tokens()` fails closed and gives *no auto-match* (so the command asks) for any of:
    - `;`, `&&`, `||`, `|`, `&`, a backtick, `$(` or `<(`;
    - `$VAR` or `${`;
    - `\` or a line continuation;
    - braces, `~`, redirection or a newline;
    - an unquoted glob;
    - a leading `NAME=value` environment prefix.
  - It extends, and replaces, the existing `SHELL_SYNTAX` list at `src/approval_server.rs:1080`, so there is one list, not two.
  - A shell wrapper (`zsh -lc '…'`, `bash -c '…'`) is refused if its quoted body contains any of the above. Until W2.3's unwrap ships, a wrapped command never auto-matches.
  - Rules hold typed fields: workspace, canonical cwd, origin driver, canonical action kind (`Shell | Edit | Read | Mcp`), argv tokens or a canonical path.
  - Edit and read paths are canonicalised and checked for containment before matching, reusing agy's existing containment helper rather than adding a second one. `..`, a symlinked escape or a sensitive target never auto-match.
  - Commands that contain a secret (as `redact` detects) are never offered **Always** or **Allow for session**: a redacted rule could never match again, and an unredacted one would store the secret.
- **D2a. The rule store is replaced outright. This is a beta, so there is no migration or downgrade path (user, 2026-10-10).**
  - The typed rules use a new format with `schema_version`. The old `always-allow.json` is ignored, and the user re-approves what they still want.
  - **Data-loss guard (kept, as it applies to every future version):** today `load()` treats an unparseable file as "no rules" (`always_allow.rs:196-201`), and the next save overwrites it (`session.rs:1581-1586`). Instead:
    - an unparseable file, or one with a newer `schema_version`, is kept byte-for-byte and never overwritten;
    - auto-approval from saved rules is off for that run, and a notice names the file;
    - writes are atomic (temp file, `sync_all`, rename).
  - Test: corrupt the file, then "Always allow" a command. The corrupt file's bytes are unchanged and the notice is shown.
  - Rules that 3.3.x wrote into projects' `.claude/settings.local.json` are not managed by the app. A one-line release note tells beta users to clear them by hand. They are left untouched by the app.
- **D3. Session rules are tagged with their agent and end on a cross-agent switch**, announced in the existing switch notice ("Session approvals (3) ended"). A same-agent model switch keeps them. *All seats agree after Round 2.*
- **D4 (revised after W0). One card, with the same five buttons for every agent.** W0 proved Codex accepts `decline` even when `availableDecisions` doesn't list it: the item ends `declined` and the turn continues.
  - **Deny** always sends `decline` (Codex) or `deny` (Claude, agy), so the agent sees the denial and continues.
  - **Stop turn** is a separate button that ends the turn (`cancel` or interrupt).
  - **Allow for session** and **Always** are app-owned for every agent (D1); Codex never offers `acceptForSession` anyway.
  - The app never sends Codex's `acceptWithExecpolicyAmendment`, which would write Codex's own persistent rule and create a second rule owner.
  - Denied is never green (F12).
- **D5. Codex Accept edits moves to `untrusted` plus `workspaceWrite`, with the app auto-allowing in-workspace edits**, so shell asks as it does elsewhere. **W0: GO.** Both requests ask, and an `accept` applies the edit. The relabel fallback is dropped.
- **D6. caps.rs stops being an unchecked claim.**
  - Fields nothing reads are either wired to a control or deleted.
  - Per-session facts become a runtime capabilities event.
  - Each adapter gets a contract test against **recorded real frames** (W0), not hand-written fixtures.
- **D7. Proof against real agents.** W0 records real frames into fixtures once. An opt-in local `tests/qa/live_smoke.py` asserts contract facts only, at about 4 tiny turns per agent. The 15-turn parity run is not scripted (Codex quota).
- **Cut (agreed):**
  - fake streaming of agy reasoning;
  - plan panels for agents with no plan emitter;
  - Codex MCP and settings panels;
  - parity for agy's `/usage` raw dump (it gets formatting only);
  - bypass mode;
  - streaming Codex reasoning beyond summaries.

## Waves

### W0: Ground truth (4–6 real turns, a sandbox repo under `/tmp`, no app code changes)
The harness is a raw protocol recorder (`tests/qa/record_frames.py`, stdlib Python), **not** the app. The adapter refuses decisions the server did not offer (`codex.rs:1739`), so only a raw harness can test sending an unoffered `decline`. The recorder stays as the fixture-refresh tool.
1. **Codex:** record the real `requestApproval` frames for a shell command and a patch. Capture `availableDecisions`, the wrapped command string and `commandActions`. Send `decline` once to see whether the server accepts it when it is not offered.
2. **Codex:** start a turn on `untrusted` plus `workspaceWrite` and check whether edits ask, and whether the app can auto-answer them (D5).
3. **Claude:** one turn with `--thinking-display summarized` (verify the flag on 2.1.293, which it is hidden in), and capture the thinking frames.
4. **agy:** capture one approval-hook query for an outside-project read (feeds W1.4).

Output: fixtures under `tests/fixtures/real/<agent>/` (redacted), plus a short findings note appended to this plan. Each W1 and W2 Codex item cites a W0 fixture.

### W1a (v3.4.0, shipped alone first): The rule engine
This is a minor version because approval behaviour changes. Rules are re-approved, with no migration (D2a).
| # | Fix | Where | Regression test (would have caught it) |
|---|---|---|---|
| 1.1 | Replace the matcher (D2) and the store (D2a) | `src/always_allow.rs:26-36, 94-147, 196-201`; call sites `src/approval_server.rs:952-1051, 1080` and `src/chat/session.rs:1559-1600` | Table test using the exact #310 payloads: a multi-line tail, `cargo test; curl`, `git status && rm`, `$(…)`, backticks, `$VAR`, `FOO=1 cmd`, `zsh -lc 'a; b'`, `/repo/../../.bashrc`, a symlink escape, a cwd mismatch. All must ask. Plus the D2a corrupt-file test. |
| 1.2 | Claude Allow for session and Always are app-owned (D1). No `updatedPermissions` is sent; the app records the **user's chosen** pattern and auto-answers matching `can_use_tool` requests. | `crates/agent-core/src/claude.rs:1302-1327`; `session.rs:1559` (custom_rule is used for every driver, not only Codex) | Rewrite `claude.rs:2945` to assert there is no `updatedPermissions`. Session test reproducing the live F1 failure: after "Allow for session" on `touch b.tmp` with the user's exact pattern, `touch b.tmp` auto-approves, while `mkdir dd` and `touch b.tmp; rm x` **ask**, and the mode stays Ask. |
| 1.3 | Codex "Allow for session" and "Always" become app-owned, like Claude's in 1.2 (D1, D4). Each sends a plain `accept` and records a typed rule; `acceptForSession` and `acceptWithExecpolicyAmendment` are never sent. This fixes the F6 wedge and F7. | `codex.rs:2010-2028`, `session.rs:1559-1600` | Replays the W0 frame `codex/ask-shell-decline.jsonl`: Session sends `accept`, and the same request then auto-approves; `ls -la; id` asks. |
| 1.7 | Session rules tagged by driver and cleared on a cross-agent switch (D3); always-rules carry their origin (U2) | the `always_allow.rs` Rule, the `session.rs` switch path | Switch test: a Claude session rule does not auto-approve an agy request after the switch. The notice text is asserted. An always-rule matches across drivers by action kind. |

### W1b (v3.4.1): Containment, secrets, deadlines and dead clicks
| # | Fix | Where | Regression test (would have caught it) |
|---|---|---|---|
| 1.4 | agy read containment. In **every** mode, Plan included, reads outside the workspace, sensitive targets and symlink escapes **ask**; in-workspace reads stay automatic. Today ReadOnly is allowed in all modes (`approval_server.rs:271`). | `src/approval_server.rs:72-85, 269-272` (`policy_with_home`) | `policy_table` rows × 3 modes: an in-project read is allowed; `~/.ssh/id_rsa`, a sibling directory, `/etc/hostname` and a symlink escape each ask. Driven by the W0 agy read-args fixture, if captured. |
| 1.5 | Live redaction: run `redact` before the event reaches the view sink, on approval title, input and detail, and on the user prompt snapshot. Commands that contain secrets get no Always or Session button (D2). | `src/chat/session.rs` emit path, `approval_server.rs:1124-1135`, `cards.rs` approval buttons | Canary test: an envelope containing `sk-…` reaches the sink as `****`, and the approval card for it has no Always or Session button. |
| 1.6 | Claude `can_use_tool` deadline, failing closed (deny plus a notice), matching agy's 570 s. The timer lives in `session.rs`; agent-core stays pure and only exposes the pending request ids. | `session.rs` (timer), `claude.rs` (pending ids) | Scripted test: an unanswered request past the deadline produces a deny frame. |
| 1.8 | **Dead clicks:** `cards::label()` stops defaulting to selectable. Content labels opt in; labels inside buttons or click targets do not. | `src/chat/view/cards.rs:67-75`, plus every caller in ui-audit #1, including the always-allow dialog's suggestion buttons and the sub-agent shelf rows | Replace `chat_labels_are_selectable` (`cards.rs:2443`) with a widget-tree walk: no selectable label has a Button or GestureClick ancestor, and content labels stay selectable. Native test (run by the existing `tests/qa/gate.py:153` runner) that clicks a tool-card title, "Thought process", the usage meter, the sub-agents counter, a question option, a rule-dialog suggestion and a shelf row, and asserts each effect. |
| 1.9 | Pango markup: a bare `&` blanks row titles (F20) | `agents_prefs.rs:335`, `setup.rs:155` | Build the prefs and setup pages and assert every `ActionRow` title is non-empty. |
| 1.10 | U1: permission sources come from config, and the hard-coded `~/git/agent-config` path is removed | `always_allow.rs`, `config.rs` (atomic write, inline validation) | A config round-trip test; a source-tree test that no `agent-config` literal remains outside tests |
| 1.11 | **agy mode safety (F22):** compact and model switch silently adopt the profile's default mode. They keep the thread's current mode instead, and any mode change is announced. | `session.rs` agy restart path | Session test: Ask mode, then compact or model switch, and the mode is still Ask |
| 1.12 | **agy hook proof (F23):** a visible badge or Settings row showing the hook is verified (from `check_installed` and the socket being bound), plus a journal line | `hook_config.rs`, header or Settings | Unit test: verdict to badge state, with three states (verified, missing, unreadable) per the repo's indicator rule |

The release gate for W1a and W1b:
- a fresh `red-team` agent that did not argue in Rounds 1–2 attacks 1.1 and 1.2 (W1a), then 1.4, 1.5 and 1.11 (W1b), before each release;
- security waves get both peers (Codex and agy) independently; if Codex is rate-limited, agy plus a native deep-tier red-team;
- rust-reviewer passes;
- `tests/qa/gate.py` passes.

### W2 (v3.4.2): Codex approval and session parity (needs W0 and W1a)
| # | Fix | Where | Test |
|---|---|---|---|
| 2.1 | Deny always sends `decline` (W0: accepted even when not offered), and **Stop turn** is a separate button that sends `cancel` (D4). Denied is styled red. The adapter's refusal of unoffered decisions (`codex.rs:1739`) is relaxed for `decline` only. Edit approvals (`fileChange/requestApproval`) carry no `availableDecisions`; the card shows the same buttons and takes the diff from the preceding `item/started` frame. | `codex.rs:1739, 2010-2028`, `cards.rs` | Replays `codex/ask-shell-decline.jsonl` and `ask-edit-decline.jsonl`: Deny sends `decline`, the item ends `declined` and the turn continues; Stop sends `cancel` |
| 2.3 | Always-allow prefill from the unwrapped command, using one shared `unwrap_shell_wrapper()` for both create and match. The saved rule matches next time. | `codex.rs`, `always_allow.rs`, rule dialog (`cards.rs:1660`) | Fixture-driven, from the W0 Codex frame: build the dialog, assert its **prefilled text** is the bare command (not the footer sentence, F7), save it, and the same W0 request then auto-approves |
| 2.4 | Accept edits on `untrusted` + `workspaceWrite`, with the app auto-answering `accept` for in-workspace edits. **W0: GO**, so there is no relabel fallback. | `codex.rs:1996`, `session.rs` | Replays `codex/untrusted-workspacewrite.jsonl`: the edit request is auto-accepted and the shell request shows a card |
| 2.5 | **Re-entry ("no rollout found")**: the thread id stays provisional until the first completed turn. A resume that finds no rollout starts fresh with the store's handoff and a notice. | `session.rs:603`, `codex.rs:392` | Scripted: a persisted id with no rollout means the next prompt succeeds in a new Codex thread |
| 2.6 | Approval card shows command and cwd, with JSON capped behind "Show raw" (ui-audit #5). The same applies to Claude's ExitPlanMode card, which renders the plan as markdown instead of raw JSON (F15). | `cards.rs` ApprovalCard | Codex-shaped and ExitPlanMode inputs: visible lines ≤ the cap, and the plan renders as markdown |
| 2.7 | The question card leaves "Sending…" once the answer is accepted (F19) | `cards.rs` question card, `session.rs` | Scripted: answer, then a resolved event moves the card to "Answered" |

### W3 (v3.5.0): One thought block, one card grammar, honest capabilities
| # | Work | Where | Test |
|---|---|---|---|
| 3.1 | Canonical `Reasoning { state: Streaming, Complete, Replayed, Unavailable }` event in agent-core. Adapters map into it. | `crates/agent-core/src/event.rs`, `claude.rs`, `agy.rs`, `codex.rs` | Adapter contract test per agent from W0 fixtures |
| 3.2 | Claude passes `--thinking-display summarized` when W0 proves it works. The flag is hidden, so it is **gated**: passed only for Claude versions known to accept it (the version comes from the existing probe). If the CLI exits at start with an unknown-option error, the session restarts once without the flag and the block shows *Unavailable*. It never fails to start. | `claude.rs` spawn args, `session.rs` restart, `claude_probe.rs` | Fixture replay renders non-empty thinking; a scripted unknown-option exit is followed by one retry without the flag, then the *Unavailable* label |
| 3.3 | agy transcript reader moves off the main thread. W0 showed the transcript's `args` values are JSON-encoded strings with extra quotes, so the reader decodes them before card titles use them. (`gio::spawn_blocking`), fixing an AGENTS.md mandate violation | `session.rs:257, 890` | Existing session tests plus a no-main-thread-I/O assertion where feasible |
| 3.4 | Canonical `ToolKind` and title grammar in agent-core. Codex `zsh -lc` unwrapping and `commandActions` live in `codex.rs`; raw content goes under Details. | `event.rs`, the adapters, `cards.rs` | Golden titles per agent from W0 fixtures |
| 3.5 | caps.rs truth (D6): wire or delete the 5 unread fields; correct agy `streams_reasoning`, model switch and compact, and Codex `usage`; fix the stale "Codex not installed" comments; add a runtime capabilities event for per-session facts. | `caps.rs`, `codex.rs:9`, consumers | Contract test: each caps flag has a rendered control or a greyed control with a tooltip |
| 3.6 | Fork: reproduce first (the code paths read correctly: `thread_menu.rs:250`, `threads.rs:3459`). Suspects are `/fork` not reaching the slash handler for Codex, or a mis-click in the live run. Fix whatever reproduces. | thread menu, composer slash handling | Native test: fork creates a second thread and the original is unchanged |
| 3.7 | Gaps polyfilled or greyed: `@` files from the app's own index for agy and Codex; questions per `plans/2026-10-09_…question-polyfill.md`; Claude context gauge; plan panel hidden where there is no emitter | composer, header | Native: the `@` popover lists files for all three drivers |

### W4 (v3.5.1): UI robustness, the rest of the audit
| # | Fix | Where | Test |
|---|---|---|---|
| 4.1 | Header at 360 px: hide the activity text at the compact breakpoint (keep the spinner) | `header.rs:64` | `native_ux_matrix`: the header's last child stays inside the window width at 360 |
| 4.2 | Toasts never cover the composer or shelf: the overlay wraps only the transcript scroller | `threads.rs:1030-1049` | Native: toast bounds do not intersect the composer or shelf |
| 4.3 | Wrap or ellipsize labels that can force width: the switch divider, question options and chip, plan expander | `cards.rs:727, 1796, 1817` | `native_ux_matrix` history with a 900-char model, a 300-char option and a 300-char plan step |
| 4.4 | Copy affordance on tool sections, diffs, approval details and errors; "Show full" for output capped at 40 lines | `cards.rs:967-969`, the diff header | Every `.card-section` and `.code-block` has a Copy button |
| 4.5 | Hit targets ≥ 24 px (HIG 34 for primary actions), accessible names on icon buttons, focus rings | ui-audit #8 | An a11y walk over the widget tree: every button has a name and a minimum size |
| 4.6 | Cosmetic: Settings dropdown truncation (ui-audit #9), duplicate "Model switched" and "Interrupted" notices, diff panel stale after undo, agy interrupt showing "Turn failed", agy `/usage` formatting | various | Per-item unit or native tests |
| 4.7 | `native_ux_matrix` gains **click** coverage: every button class in the demo is clicked and its effect asserted. Today the matrix never clicks. | `src/chat/view/ux_tests.rs` | — |
| 4.8 | The welcome dialog's "Get Started" button sits on the dialog edge (ui-audit) | welcome or setup dialog | Native: the button lies fully inside the dialog bounds at 360 and 900 px |
| 4.9 | The queue was never exercised by the UI audit. Add a queued-prompts demo scene (`AGENT_TERMINAL_DEMO_SCENE`) and audit the queue for pressability, selection and containment. | `demo.rs`, `ux_tests.rs` | Native: queued items stay pinned, are selectable, and their remove buttons click |

## Proof and gates (every wave)
- `cargo fmt`, `clippy -D warnings`, `cargo test --workspace` and `python3 tests/qa/gate.py` (xvfb is now installed).
- rust-reviewer runs after every Rust change; re-reviews go to the same agent as deltas.
- A cross-harness Maker-Checker review rotates per the delegation skill. Security waves (W1, W2) get both peers.
- Live check through `preview_app` (`--chat-demo` for UI). For W1, W2 and W3, `tests/qa/live_smoke.py --agent <a>` runs at about 4 turns each, with the user's quota approval per run.
- Re-run the parity scenario only at 3.5.0 (after W3), then regenerate `docs/feature-parity-report.md` as the before/after proof.
- Release per wave: `make bump-version`, `docs/releases/v<x>.md`, `make release`, then CI and debian-maintainer. Never `make install`.

## Execution model
- Builders are standard-tier agents, one per file lane, using job-board waves on topic #29. The coordinator keeps `always_allow.rs`, the shared types in `event.rs`, commits and releases.
- W1a (1.1, then 1.2 and 1.7) is built first and ships alone as 3.4.0. W1b then runs in parallel by lane: approval_server (1.4, 1.12); session (1.5, 1.6, 1.11); the view (1.8, 1.9); config (1.10).
- Cadence is realistic, not optimistic. Each release waits for its red-team and peer reviews and the quota-gated smoke run. Releases merge if a review slips; they are never skipped.
- Every approval fix carries its regression test in the same commit.

## Risks
| Risk | Mitigation |
|---|---|
| The fail-closed matcher asks more often (for pipes in common commands) | This is intentional. Exact compound commands can still be approved for the session verbatim (whole-string equality, no tail-splitting). Revisit with data. |
| Codex `untrusted` asks on edits and the app cannot auto-answer them | The W0 probe decides; fallback is the relabel (D5) |
| The hidden Claude flag disappears in a CLI update | The caps runtime event plus the *Unavailable* label; the contract test fails loudly on fixture refresh |
| Clearing session rules on a switch annoys the user | Announced in the notice; Always-rules carry; the decision is reversible later |
| Quota: W0 plus smoke runs | About 6 + 12 turns per wave, each run approved by the user |

## Rollback
Each wave is a release. `git revert` the wave's commits and ship the previous version through the apt pipeline.
- This is a beta, so no downgrade path is kept for the rule store (D2a). Rolling back means re-approving rules under the old version.
- The matcher change only removes auto-approvals.
- Fixtures and tests are additive.

## User decisions (CHALLENGE round, 2026-10-10)
- **U1. Permission export:** keep it, but as config. An optional "permission sources" list in Settings, empty by default. The hard-coded `~/git/agent-config/...` path leaves the binary, and this machine's config points the list at agent-config. → **W1.10**: `always_allow.rs` import and export read paths from config. A test asserts no `agent-config` string remains in the binary's source outside tests.
- **U2. Always-rules are shared across agents**, matched by canonical action kind (shell argv, edit path, MCP server/tool), not the provider's tool name. Each rule records the agent it was created on, shown in Settings. Session rules stay per agent (D3).
  - This pulls a minimal canonical kind (`Shell | Edit | Read | Mcp`) into **W1.1**'s typed rule, ahead of W3.4's full `ToolKind`.
  - No migration (beta): existing rules are not carried over (D2a).
- **U3. W0 runs now** (about 6 real turns). W1 work that does not depend on Codex starts in parallel.

## Log
- 2026-10-11: Resumed. All waves are on master: W1a/W1b, W2 (plus Stop turn on every card, `0c28918f`), W3 core and app, W4, and F17 (reasoning as markdown). The W1 security gate took 5 rounds:
  - Round 1: the four checkers rejected the first cut.
  - Round 2: the native red-team said SHIP, rust-review approved, Codex and agy did not.
  - Round 3: the matcher moved to a read-only prefix allowlist with option clusters, a credential parser and a store lock.
  - Round 4: the allowlist was trimmed (fd/tree/file/date/git branch/git remote removed); masking became quote-aware.
  - Round 5: jq `--module-path`, rg `--hostname-bin`, and git signature options. Decision: keep `git log`/`show` widening; config-driven git execution is gated by `.git/config` and home dotfiles being sensitive edit targets.

  live_smoke passed on 2026-10-11: Claude 9/9, Codex 16/16, agy 5/5. Parity report regenerated (`0709b648`) from code, tests and smoke runs; the 15-turn scenario was NOT re-run. Release notes are drafted in `docs/releases/v3.4.0.md`.
- 2026-10-10 (checkpoint, session paused on usage limit). ON MASTER, reviewed and approved: live_smoke (agy run live, 5/5 PASS); 1.4 + 1.12; W1a first cut; UI lane 1.8, 1.9, W4 4.1–4.9 (4.6: notices dedup, dropdown truncation; diff-panel-after-undo not reproduced); session lane 1.5 (all events masked), 1.6, 1.10, 1.11 + fork mode; W2 2.1–2.7 + U2 Always for Edit/Mcp (kept per U2); W3-app 3.3, 3.5, 3.6, 3.7 (`@` files + gauge), agy interrupt/usage. Gate green at b2701e56 (14 native cases).
  OPEN, on branches, not merged:
  - `w1a-rules` holds the W1 security-gate fixes. Codex, agy, the native red-team and rust-review all returned CHANGES-REQUIRED; Hive #342 lists the decisions. Fixes are in 982b3f92 plus a merge. Still running: integration with W2 (R1 through `always_offerable`, R5 on Codex accept-edits, the shared `codex_change_targets`, and Rule::new returning Result). Next: merge it, then re-review the delta with the same four checkers.
  - `lane-w3-core`: 3.1 Reasoning, 3.2 the thinking flag, 3.4 ToolKind titles, plus the Codex mapping. Builder running. Next: merge, then rust-review.
  - `lane-w3-app`: hardening follow-ups (state clearing, a conv_id check, the `@` cache). Builder running.
  - After all of that: live_smoke for claude and codex (approved), the parity re-run plus a regenerated parity report, one combined release (user: single large update; waves merge to master and the beta suite builds it), release notes listing the re-approve-rules and `.claude/settings.local.json` cleanup notes, and on this machine `"permission_sources": ["~/git/agent-config/sync/permissions.toml"]`.
  - Deferred: Codex question polyfill (separate PROPOSED plan 2026-10-09).
- 2026-10-10: Execution. User: live_smoke runs approved for every wave; no per-wave releases: waves merge to `master` (the beta suite builds it) and ship as ONE combined release at the end. Round 1 lanes in worktrees: W1a (`w1a-rules`), 1.4/1.12 (`lane-agy-policy`), 1.8/1.9 + W4 (`lane-view-parity`), live_smoke (`lane-live-smoke`).
- 2026-10-10: W0 done (Codex 4 turns, one over the ~3 approved; Claude 2; agy 1, whose cwd slipped to $HOME on a read-only prompt). Results:
  - D5 GO; thinking flag GO.
  - Codex accepts an unoffered `decline`, so D4 is simplified to the same five buttons everywhere.
  - Codex never offers `acceptForSession`, so session scope is app-owned (new W1.3) and the old 1.3 "hide" and 2.2 are removed.
  - Fixtures scrubbed of machine identity by the recorder's new rules (hook and MCP-status frames dropped; environment lists, email, organization, home, host and user masked; `rescrub` mode added). gitleaks is clean.
- 2026-10-10: User: this is a beta build, so no upgrade path is needed. Migration, the v2 file, the legacy-file handling, Claude external-rule review and the downgrade caveats are removed. The data-loss guard (an unreadable file is never overwritten) is kept.
- 2026-10-10: plan-reviewer round 2: **APPROVED**. Two SHOULDs folded into D2a: v2 parse failure is never treated as empty; Claude-side 3.3.x rules are surfaced as external. One NICE added to Rollback: revocations don't carry back.
- 2026-10-10: plan-reviewer round 1 returned CHANGES-REQUIRED: 1 BLOCKER, 5 SHOULD and 5 NICE, all addressed.
  - BLOCKER: rule-store migration and rollback. Fixed by D2a (a new v2 file; legacy file never rewritten).
  - D2 fail-closed list widened, reusing `SHELL_SYNTAX`.
  - D1 made true for Claude: no `addRules` forwarding.
  - Tests reproduce the live failures (F1 `mkdir`, F7 prefill, read containment in every mode).
  - Hidden-flag gate with retry; W0 uses a raw harness.
  - F15, F19, F22, F23, the welcome dialog and the queue added.
  - Versions moved to 3.4.0+ for the format change; W1 split into W1a and W1b; `cards.rs:2443` corrected; the 1.6 timer moved into `session.rs`.
- 2026-10-10: Draft. Panel: experience-designer (Lyra), architect (Vitruvius), senior-engineer (Pip). Peer: agy red-team (CHANGES-REQUIRED). UI audit: 1 BLOCKER and 5 SHOULD. The orchestrator verified #310 (`always_allow.rs:102-110`) and ui-audit #1 (`cards.rs:70`) in code.

## W0 findings (2026-10-10)

Recorded with `tests/qa/record_frames.py` in a sandbox repo; fixtures under `tests/fixtures/real/{codex,claude,agy}/`. Codex 0.161.0, Claude 2.1.293.

- **Codex shell approval (`ask-shell-decline.jsonl`).** `availableDecisions` = `["accept", {"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["ls","-la"]}}, "cancel"]`. There is no `acceptForSession` and no `decline`. The command string is wrapped: `/usr/bin/zsh -lc 'ls -la'`. `commandActions` = `[{"type":"listFiles","command":"ls -la","path":null}]`. `cwd` is the sandbox. `proposedExecpolicyAmendment` is also present. The amendment key is snake_case (`execpolicy_amendment`).
- **Decline when not offered: ACCEPTED.** The item completes with status `declined`, the turn continues, and the agent reports the rejection (`turn/completed`, status `completed`).
- **Codex patch approval (`ask-edit-decline.jsonl`).** `item/fileChange/requestApproval` has NO `availableDecisions` (params: `threadId`, `turnId`, `itemId`, `startedAtMs`, `reason: null`, `grantRoot: null`). `acceptForSession` is therefore not offered. The diff is only in the preceding `item/started` fileChange item (`changes[].path/kind/diff`). W1.3 must treat an absent field as "accept/decline/cancel only".
- **D5 verdict: GO** (`untrusted-workspacewrite.jsonl`). On `untrusted` + `workspaceWrite`, the edit asks (`fileChange/requestApproval`) and the shell `ls` asks (`commandExecution/requestApproval`). Answering `accept` to the edit applied it (`item/completed` status `completed`, file changed on disk). The app can auto-answer `accept` for in-workspace edits, and shell keeps asking.
- **Claude thinking flag: GO.** 2.1.293 accepts `--thinking-display summarized` with no start error. Thinking blocks stream non-empty `thinking_delta` text and end with a full `thinking` block plus `signature` (`thinking-and-touch.jsonl`).
- **Claude `can_use_tool` (`touch x.tmp`).** The request has `tool_name: Bash`, `input {command, description}`, `blocked_path`, and `tool_use_id`. `permission_suggestions` = `[{type: addRules, rules: [{toolName: Bash, ruleContent: "touch x.tmp"}], behavior: allow, destination: localSettings}, {type: addDirectories, directories: [cwd], destination: session}, {type: setMode, mode: acceptEdits, destination: session}]`. A deny `control_response` was accepted and the turn continued.
- **agy (`agy/`).** `agy -p … --output-format stream-json` emits `step_update` events with `tool_name: view_file` and `tool_info.parameters {AbsolutePath: "/etc/hostname"}`. The on-disk transcript (`~/.gemini/antigravity-cli/brain/<conv>/.system_generated/logs/transcript.jsonl`) has `tool_calls[{name: "view_file", args: {AbsolutePath, toolAction, toolSummary}}]`, and the arg values are JSON-encoded strings (extra quotes). The hook socket was not driven. The agy turn ran with the real home as cwd, not the sandbox (read-only prompt; a process slip).
- **Canary:** never read by any agent; it appears nowhere in the fixtures. The recorder drops `hook/*` frames and masks IPv4 addresses.
- **Turns:** Codex 4 (one per fixture, with two in the workspaceWrite run), Claude 2 (one process), agy 1.
