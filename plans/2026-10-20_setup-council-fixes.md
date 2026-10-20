# Plan: first-start setup council fixes (2026-10-20)

Source: `docs/setup-council-review.md` (four-seat council, findings 1-10).
Branch: `feat/setup-council-fixes` from `origin/master`. One PR; required checks `gate/check`
and `cargo-deny` must pass before merge.

## Decisions (orchestrator)

- **D1: fix `pattern_auto_approval` rather than remove it.** "Pattern approvals" means the
  saved **Always allow** rules (the rules file). On (default, unchanged): saved rules approve
  matching requests without a card, as today. Off: saved rules never auto-approve, and cards do
  not offer "Always allow". Session grants ("Allow for session"), which the user picks in this
  session, keep working either way. This applies to all three agents: Claude and Codex through
  `ChatSession::gate_approval`, agy through `approval_server` `rules_allow`/`saved_allow`.
  The default stays `true`. The current behaviour already equals "on", so no existing user's
  experience changes; the copy stops selling it ("for smooth operation" goes).
- **D2: setup stays read-only towards other tools' config.** The agy hook is shown (status +
  warning + Copy), never installed by the app.
- **D3: no wizard pages, no in-app installer.** One welcome page and one agents page.

## Shared contract (both lanes code against this; lane A defines it)

In `src/always_allow.rs`, a process-wide policy the window sets and every gate reads:

```rust
/// Whether saved "Always allow" rules approve matching requests (Settings → Agents,
/// `pattern_auto_approval`). Set by the window from the config at startup and on change.
pub fn set_saved_rules_enabled(on: bool);
pub fn saved_rules_enabled() -> bool; // default true until set

/// Whether "Always allow" first shows the confirm-and-customise dialog
/// (`confirm_rule_modification`), read instead of loading the config file.
pub fn set_confirm_rule_modification(on: bool);
pub fn confirm_rule_modification() -> bool; // default matches TerminalConfig::default()
```

Backed by `AtomicBool`s (`Ordering::Relaxed` is enough: the main thread is the writer, and the
readers are main-thread or tolerate one stale read).

Lane B calls both setters: in the window wherever the config is first loaded, and in the shared
row builders when the switch changes.

## Lanes

### Lane A: approval enforcement (maker: Codex `gpt-6.1-sol`, effort high; security)

Owns: `src/always_allow.rs`, `src/approval_server.rs`, `src/chat/session.rs` (and
`src/chat/session/**`), `src/chat/view/cards.rs`.
Must not touch: `src/window/**`, `src/config.rs`, `README.md`, `crates/**`.

1. Add the contract above, with unit tests.
2. `ChatSession::gate_approval`: when `!saved_rules_enabled()`, a saved rule never makes `auto`
   true, and `Decision::AllowAlways` is removed from the options. Session rules are unaffected.
3. `approval_server`: `rules_allow` consults `saved_allow` only when `saved_rules_enabled()`.
   "Always allow" is not offered or kept for agy cards when off (follow where the agy card's
   options and `remember` are built). Session rules are unaffected.
4. `chat/view/cards.rs:2238`: replace `TerminalConfig::load().confirm_rule_modification` with
   `crate::always_allow::confirm_rule_modification()`.
5. Tests: off means a matching saved rule is not applied and Always is not offered (both Claude/
   Codex gate and agy server); on keeps today's behaviour; session rules apply either way.
6. Verify: `cargo clippy --workspace --all-targets -- -D warnings`, then
   `cargo test -p agent-terminal always_allow approval_server session`.

### Lane B: setup UI and window (maker: agy `claude-sonnet-5-5`, effort medium)

Owns: `src/window/imp/setup.rs`, `src/window/imp/agents_prefs.rs`, `src/window/imp.rs`,
`src/config.rs`, `crates/agent-core/src/adapter.rs`.
Must not touch: lane A's files, `README.md`.

1. **Shared approval rows** (findings 7, 1 copy): move "Automatic Pattern Approvals" and
   "Confirm and Customise Patterns on Save" into `pub(super)` row builders in
   `agents_prefs.rs`, used by both Settings → Agents and setup. On change: update config,
   `schedule_config_save()`, and call the lane A setter. New copy (Canadian English):
   - Title "Automatic Pattern Approvals", subtitle: "Saved Always-allow rules approve matching
     requests without asking. Off: every request asks, except what you allow for this session."
   - Group description in setup: "How saved rules are used. Each rule is one you chose with
     Always allow; broad patterns such as 'cargo *' approve more than you may expect."
2. **Startup sync**: wherever the window takes its config at startup, call
   `crate::always_allow::set_saved_rules_enabled(..)` and `set_confirm_rule_modification(..)`.
3. **agy hook in setup** (finding 2): in setup's Antigravity group add a read-only hook status
   row reusing the Settings → Agents hook row's logic (`hook_config` badge, its three states,
   the warning that hookless agy applies file edits by itself in every mode, and the Copy
   button). Factor the existing builder so both pages use it; no new hook logic.
4. **Update check consent** (finding 3): add "Check for Updates at Startup" to setup (reuse the
   General page's row logic as a shared builder). On a first run, start the startup update check
   only after the setup dialog closes (`imp.rs` `check_updates_on_startup`, ~l.2817); the
   welcome text names what setup changes: "Setup writes Agent Terminal's own settings file and
   nothing in any agent's configuration."
5. **Ready summary + sign-in clarity** (finding 4): a row at the top of the agents page:
   "Ready: Claude, Codex" / "No agent is ready yet: threads need an agent that is installed and
   signed in." It updates as detection and sign-in answers arrive. Make the Sign In button's
   explanation a visible subtitle, not only a tooltip. Done stays enabled.
6. **Install hints** (finding 5): give each `Driver` an install URL in
   `crates/agent-core/src/adapter.rs` (the README's "Get it" links, `README.md:112-116`) and
   show a link button on a not-installed row; add a "Check Again" button to the agents page
   that reruns detection (`Probes::Force`, `agents_prefs.rs:1459`).
7. **Accurate copy** (finding 6): replace "Agents found on this computer are switched on" with
   wording that is true (an agent not installed is shown as not installed).
8. **Save failure** (finding 9): when the save that ends setup fails, show a toast saying the
   settings could not be saved and setup will return next start.
9. **Regression check**: a native `#[ignore]` test beside the existing one: empty
   `XDG_CONFIG_HOME`, `show_setup()`, close the dialog, settle, flush the save, and assert that
   the config file exists and a reload has `is_first_run() == false`.
10. Verify: `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo test -p
    agent-terminal setup agents_prefs config`, then `cargo test -p agent-core adapter`.

### Lane C: docs and screenshots (orchestrator, after A and B merge into the branch)

- README: first start (approvals group, update check, hook row, ready summary), reword Install
  (sign-in is part of setup), add Troubleshooting "Setup never appeared / run it again", and
  describe the Automatic Pattern Approvals semantics (D1) in Approvals and rules.
- Screenshots via `preview_app` + Playwright: `setup-agents.png`, `setup-nothing-installed.png`
  (seeded config with bogus agent commands), `setup-signin.png`, `agy-hook-missing.png`. Write the
  recipe down in `tests/qa/README.md`.

## Review (maker-checker rotation)

- Lane A (Codex maker, security): agy checker (`gemini-4-argon`, plan mode, read-only) and a
  native Claude `red-team`, independently.
- Lane B (agy maker): Codex checker (`gpt-6.1-sol`, plan mode).
- The orchestrator runs `make check` on the integrated branch, then opens the PR and merges once
  CI is green.

## Rollback

All changes are in one PR; revert the merge commit. D1 keeps the default and today's behaviour
for "on", so a revert changes nothing for users who never turned the switch off.
