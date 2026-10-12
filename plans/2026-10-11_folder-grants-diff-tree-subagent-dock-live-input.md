# 2026-10-11 — folder grants, diff tree, sub-agent dock, live input

Requested by the user in one session. Order is the order of work.

## 1. "Allow for session" on an edit grants a folder — IN REVIEW

Was: an edit rule held exactly one file (`Target::Path`, equality match), so every other file in
the same crate asked again.

Done (uncommitted at time of writing):
- `Target::Dir`, session-only; `always_allow::session_folder` picks the file's folder; the card
  says so before the click (`approval_server::card_note`).
- Policy: inside the workspace; outside it only ≥2 levels inside home (never `~/git`, `~/bin`,
  `/tmp`, `/etc`, `/run`); never `/`, home or above; nothing without a known home; sensitive files
  inside still ask (`folder_auto_matchable`); the answer keeps the folder only if it is the one the
  card showed (`decide_as_shown`); typed `dir/` rules get the same breadth check; the dialog never
  prefills a folder; "Always allow" refuses folders and `matches_any` ignores them.
- Red-team round 1: CHANGES-REQUIRED (outside-workspace breadth, card/click re-resolve, typed and
  prefilled home grants, no-home fail-open) — all addressed. Round 2: pending.
- Follow-up, not in this change: SENSITIVE_NAMES gaps shared with Accept edits
  (`.github/actions`, `.forgejo/workflows`, `.gitea/workflows`, `.githooks`, `lefthook.yml`,
  `rust-toolchain.toml`, `.npmrc`).

## Decisions (user, 2026-10-11)

- Diff panel: one header per folder (full path), files under it collapsed with +/- counts, a file
  expands to its own diff, built on first expand.
- Sub-agents: keep this turn's finished sub-agents docked (until the next prompt or dismissed)
  instead of the shelf sliding away. Collapsed default and dialog-on-click stay as they are.
- Live input: Enter sends straight to the agent mid-turn where the agent supports it (Claude
  verified); agents that cannot keep today's queued card.
- Every item lands on its own branch + PR (master is protected).

## 2. Diff panel: files grouped under their folder, each collapsible

Now: `window/diff_panel.rs` shows a flat `ListBox` of files above ONE `TextView` holding the whole
diff; clicking a file scrolls the text.

Want: a tree — folder headers, a file row per changed file under its folder, each file expandable
to its own diff (collapsed/expanded independently), so the diffs are physically separate.

Notes for the design:
- Split `diff.text` per file on the `diff --git` headers (`crate::diff::classify` already finds
  `LineKind::File` lines); keep the split pure and unit-tested in `crate::diff`/`agent-kit::diff`.
- One small `TextView` per expanded file (built lazily on first expand) instead of one huge
  buffer; keep tag colouring and the "too large"/"truncated" notes.
- Folder grouping: common folder of each path (`src/window/`), files sorted within; a file at the
  root goes under the repository name.
- Keep the per-file "Open in ‹tool›" button and the Undo/base header untouched.
- Check `agent-terminal-wt-subagents` / `-wt-view` branches touch `diff_panel.rs` before
  starting, to avoid a conflict.

## 3. Sub-agents docked at the bottom of the chat

Want: running/recent sub-agents pinned at the bottom of the thread (above the composer), so they
can be opened and closed without scrolling back through history.

Notes: branches `fix/background-subagents` (wt-subagents) and `fix/view-subagents-status`
(wt-view) already touch sub-agent display (`subagent_panel` in `chat/view.rs`). Read their state
and merge or build on them rather than duplicate.

## 4. Live input instead of the app's own queue

Finding (verified in code): the app queues every message sent during a running turn
(`chat/view.rs` `submit` → `queue_prompt`; `chat/session.rs` sends the next one on
`TurnCompleted`). Nothing reaches the agent until the turn ends, for all three agents.

Want: messages reach the agent while it works, the way Claude Code takes them in.

To verify live before designing (never from memory):
- Claude Code `--input-format stream-json`: VERIFIED 2026-10-11 (haiku, probe script). A second
  user message written 6 s into a turn (during `sleep 20`) was folded into the SAME turn: one
  `result` ("DONE1 BANANA", `num_turns: 2`), no second turn, and no event echoing the injected
  message (the app must render it itself).
- Codex app-server: is there a steer / mid-turn input method in the version we target?
- agy: any mid-turn input at all? If not, it keeps the queue.
Then: per-agent capability in `agent_core::caps`, the composer sends live where supported, the
queued card stays only where it is not.
