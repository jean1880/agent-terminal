# Plan: cross-agent hand-off (Claude ⇄ AGY) — APPROVED 2026-09-27

Decisions (2026-09-27): build **all four WPs**, in order WP1 → WP3 → WP2 → WP4;
the brief is **terminal-built** (transcript + git), never produced by the
out-of-quota agent.

Goal: when one agent runs out of quota mid-task, continue the same work in the
other agent in one click, without re-explaining the task.

## What exists today (1290bd6f)

- Profiles are config-declared; any tab entry point can open as a chosen profile.
- Resume + the session browser work for **Claude only**: `known_resume_settings`
  knows `claude`; `list_sessions`/`find_session_dir` assume
  `<store>/<dir>/<id>.jsonl`.
- The AGY profile has no `resume_args`, so it cannot resume or be browsed.
- Shared instructions already line up: both agents load `agent-config/rules/core.md`.
  The *conversation* is the only thing that does not carry over.

## Facts established (evidence)

| # | Fact | Status | Evidence |
|---|------|--------|----------|
| F1 | AGY resumes with `--conversation <id>` and `--continue` | VERIFIED (strings) | flag text in `~/.local/bin/agy`; not yet exercised live |
| F2 | AGY takes an initial prompt interactively via `--prompt-interactive` | VERIFIED (strings) | same; exact short flag and arg form UNVERIFIED |
| F3 | AGY conversations are SQLite `conversations/<uuid>.db`, not JSONL | VERIFIED | `ls ~/.gemini/antigravity-cli/conversations` |
| F4 | `~/.gemini/antigravity-cli/history.jsonl` indexes them: `{display, timestamp, workspace, conversationId}` per prompt | VERIFIED | head of the file |
| F5 | Claude transcripts record quota exhaustion structurally: `"error":"rate_limit"`, `isApiErrorMessage:true`, text "hit your session limit" | VERIFIED | grep of `215bf7f7…jsonl` |
| F6 | Claude accepts `--session-id <uuid>`, so the terminal can know a new tab's transcript path | VERIFIED (strings) | `--session-id <uuid>` in the claude 2.1.283 binary |
| F7 | The AGY `.db` schema is readable enough to extract turns | UNVERIFIED | not opened; may be protobuf blobs |

## Proposal — four work packages, each shippable alone

### WP1 — AGY as a first-class resumable profile (small, do first)
- Add `agy` to `known_resume_settings`: `resume_args = ["--conversation", "{id}"]`.
- Introduce a session-store **kind** on `Profile` (`claude-jsonl` | `agy-history`),
  defaulted by `known_resume_settings`, so the browser and `find_session_dir`
  dispatch per kind instead of assuming JSONL.
- `agy-history` reader: stream `history.jsonl`, group by `conversationId`;
  title = first `display`, cwd = `workspace`, last-active = max `timestamp`.
  Same bounded-read discipline as `summarize_session`; corrupt lines skipped.
- Migration: backfill only profiles that set neither field (same rule as Claude).
- Result: browse/resume AGY sessions exactly like Claude ones.

### WP2 — "Continue in…" hand-off action (the core feature)
A tab-menu / shortcut action **Continue in ▸ <profile>** on any tab:
1. Take the current tab's cwd (fixed for its lifetime — already tracked in `TabState`).
2. Build a **hand-off brief** (WP3) and write it atomically to
   `$XDG_STATE_HOME/agent-terminal/handoffs/<ts>-<from>-to-<to>.md` (outside the
   repo, never in the worktree).
3. Open a new tab with the target profile in the same cwd, with the initial
   prompt: *"You are taking over from <from>. Read <path> and continue."*
   Delivered via a new per-profile `prompt_args` template, e.g.
   Claude `["{prompt}"]`, AGY `["--prompt-interactive", "{prompt}"]` — still a
   single `exec`, nothing written to the TTY first (respects the exec mandate).
4. Keep the source tab (a dead/limited session is not a reason to destroy UI).

### WP3 — Hand-off brief builder (pure logic in `utils.rs`, unit-tested)
The source agent is out of quota, so it cannot summarize itself; the terminal
builds the brief from what's on disk:
- **From the transcript** (Claude JSONL; AGY via `history.jsonl` prompts only
  unless F7 pans out): the original task (first user prompt), the last N user
  prompts, the last assistant text reply, and the files touched by Edit/Write
  tool calls. Bounded read of head + tail, like the browser.
- **From the workspace**: `git status --short` and `git diff --stat` of the cwd
  (run via `spawn_blocking`, with timeout — never on the main thread).
- **Resume pointer**: the source session ID, so you can go back when the quota resets.
- Size-capped (e.g. 8 KB) so the brief never blows the new agent's context.
- Fallback when no transcript is found: the visible scrollback tail from VTE
  (`terminal.text_range`), scrubbed of nothing more than ANSI.

### WP4 — Quota detection → prompt (optional polish)
- Per-profile, config-declared `limit_patterns` (regexes) matched in the
  existing `contents_changed` handler against the last few lines only
  (debounced). Defaults: Claude `hit your (session|weekly) limit`, AGY TBD
  (capture the real message next time it happens).
- On a match: mark the tab, show an `adw::Banner` "<Claude> is out of quota —
  Continue in AGY?" with the button wired to WP2. Never auto-switch.
- Three states, per the indicator rule: limited / fine / unknown.

## Review amendments (plan-reviewer, 2026-09-27) — these override the WPs above

1. **BLOCKER → fixed: the brief is redacted and locked down.** Every brief passes
   an inline `redact()` in `utils.rs` (no `nuvek-core`: standalone mandate)
   before it touches disk: known token prefixes (`ghp_`, `gho_`, `github_pat_`,
   `sk-`, `sk-ant-`, `AIza`, `xox[bpa]-`, `glpat-`), `Bearer <tok>`,
   `NAME=value` / `"name": "value"` where NAME contains KEY/TOKEN/SECRET/PASSWORD,
   and whole PEM blocks → `****[last-4]` or `[REDACTED PEM]`. Ceiling: pattern
   based, so an unprefixed secret in prose gets through; documented in the
   brief's header. Directory `0700`, files `0600`, created that way (no chmod
   race). Retention: prune briefs older than 7 days on each write.
2. **WP4 is transcript-first.** New profile field `session_id_args`
   (Claude: `["--session-id", "{id}"]`): new tabs get a UUID up front, so the
   terminal knows the transcript path and polls its tail (off-thread, on a
   timer, only while the tab is open) for `"error":"rate_limit"`. VTE-text
   `limit_patterns` is the fallback for profiles without a structured signal
   (AGY until F7). The tab's `session_id` is then always known, so restart
   resumes rather than starts over.
3. **WP1 has its own reader.** `history.jsonl` is one shared, growing log, not a
   file per session: read a bounded tail (4 MiB), group rows by
   `conversationId`, order by each row's `timestamp`, title = earliest
   `display` seen, cwd = `workspace`. Profile gains `session_format`
   (`claude-jsonl` default | `agy-history`) to dispatch.
4. **Startup precedence is explicit:** `resume` > `prompt` > plain, as an enum
   `Launch::{Fresh, Resume(id), Prompt(text)}` passed to `get_startup_command`,
   with a test per branch. The prompt is a fixed template naming the brief path
   (never user text), and still goes through `shell_quote`.

## Progress (2026-09-27, branch `feat/agent-handoff`, uncommitted)

- WP1–WP4 implemented. The gate is green: fmt, clippy `-D warnings`, 103 tests.
- The brief was rendered from a real Claude transcript. That run caught Read
  tool calls being listed as edits; now fixed and covered by a test.
- Deviations from the plan: markers are case-insensitive substrings, not
  regexes, so no regex crate is needed. Screen text comes from
  `write_contents_sync`, because `text_range_format` would raise the VTE floor
  to 0.72. Restart does not resume a *pinned* ID: a session that died at launch
  has no transcript to resume.
- Still UNVERIFIED live: the `--prompt-interactive <text>` argument form, AGY's
  real quota message, and a GUI click-through of the banner and Continue In.
- rust-reviewer: PASS (no blockers or should-fixes). Its one nit is that a
  brief directory predating this feature keeps its old mode if the `chmod`
  fails. The `0600` file mode still holds.
- Pending: a live GUI test via `dbus-run-session -- ./target/debug/agent-terminal`,
  then commit.

## Out of scope
- Syncing memory between agents (already shared via agent-config + native memory).
- Translating tool calls or permissions between CLIs.
- Auto-switching without a click.

## Risks
- CLI flag drift (AGY is young): everything is config-declared, so a fix is a
  config edit, not a rebuild.
- Brief quality from AGY transcripts is limited until F7 is resolved.
- Prompt with a path under `$XDG_STATE_HOME` may trigger a read-permission
  prompt in the receiving agent — acceptable, or add the dir to both allowlists
  via agent-config.

## Tests left behind
- `agy-history` grouping/title/cwd (fixture file), corrupt-line tolerance.
- `prompt_args` substitution + shell quoting (injection cases, like session IDs).
- Brief builder: caps, missing transcript fallback, tool-call file extraction.
- `limit_patterns` match only in the tail window.
- Migration backfill for a pre-existing `Agy` profile; explicit opt-out respected.
