# Council review: first start and install flow (2026-10-20)

Seats: experience-designer, architect, red-team, senior-engineer (all read-only, all Claude
native personas). The orchestrator checked the key claims in source; they are marked **verified**.

## Verdict

Setup is short and safe in what it writes: it never touches files outside the app's own settings,
first-run detection fails closed, and writes are atomic and `0600`. It does **not** fully
configure a working install, though, and one setting it shows does nothing.

## Findings, ranked

| # | Sev | Finding | Evidence |
|---|---|---|---|
| 1 | **Blocker** | "Automatic Pattern Approvals" does nothing. It defaults to on, setup describes it as "On by default for smooth operation", but no runtime path reads it. Saved rules are always applied, so turning it off changes nothing. | **verified**: only readers are `setup.rs:142`, `agents_prefs.rs:585`; default `config.rs:1040`; `approval_server.rs:1052-1058` `rules_allow` ignores it |
| 2 | High | Setup can end with agy as the default and **no approval hook**. Without the hook, agy applies file edits on its own, Plan mode included. That warning, and the hook status, are only in Settings → Agents. | `setup.rs:105,121`; `agents_prefs.rs:1914-2014` |
| 3 | High | Setup never says what it changes. It writes the settings file on close, even with no changes. The startup update check (GitHub) is on by default, can fire while setup is open, and has no switch in setup. | `setup.rs:51-52`; `config.rs:1024`; `imp.rs:2854-2876` |
| 4 | High | Sign-in can stall. "Sign In…" becomes "Check Again", and only a tooltip explains why. Done is allowed with every enabled agent signed out or missing, and nothing says so. | `agents_prefs.rs:1486,1505-1510`; `setup.rs:187-193` |
| 5 | Med | Install hints are plain text with no link (agy's has no URL), and the dialog has no rescan. | `agent-core/src/adapter.rs:69-97`; `agents_prefs.rs:1459` |
| 6 | Med | The text "Agents found on this computer are switched on" is wrong: an agent that isn't installed is also switched on. | `setup.rs:105`; `agents_prefs.rs:1446` |
| 7 | Med | Setup builds its two approval switches inline instead of reusing shared rows. The wording already differs from Settings. | `setup.rs:136-177` vs `agents_prefs.rs:584` |
| 8 | Med | `confirm_rule_modification` is read from disk on the main thread with each click. On a corrupt file, every click saves another copy aside. | **verified** `chat/view/cards.rs:2238` |
| 9 | Low | Closing setup counts as finishing it. A failed save only logs, so setup returns on every launch with no message. | `setup.rs:52`; `config.rs:1534-1540` |
| 10 | Low | README drift: the approvals group isn't described, Install says to sign in before launch (setup now does sign-in), and Troubleshooting doesn't cover "setup never appeared / how to re-run it". | `README.md:106-116,144-152,816+` |

Dismissed: "the save may be lost on quit". Window close flushes the debounced save
(`closing.rs:377`), so the save is lost only on a crash.

## What's fully configured, what isn't

| State | Setup |
|---|---|
| Agents on/off, install state, sign-in, default agent | Configured or detected |
| agy approval hook / socket | **Not shown** (Settings only, copy-paste by design) |
| Default model, effort, args, env file | Left to Settings (fine) |
| Update check | **Not shown** |
| Theme, default folder | Not offered (optional) |
| cross-agent MCP, store, worktrees, diff tool | Defaults; nothing to do |
| Desktop file / icons | Installed by every package |

## Screenshots

Yes, for the README only; keep the dialog text-led. Today only `setup-welcome.png` exists.
Recommended (seats agreed on the first two):

1. `setup-agents.png`: the Your Agents page with agents found and a default chosen.
2. `setup-nothing-installed.png`: the same page with no CLIs found and the install hints. This
   is the Troubleshooting counterpart.
3. `setup-signin.png`: an agent found but signed out, showing Sign In.
4. `agy-hook-missing.png`: agy's hook status row once it is added to setup (finding 2).

Produce them reproducibly with `preview_app` on `target/debug/agent-terminal`: fresh XDG homes make
a first run, and Playwright clicks Get Started. For "nothing installed", point the profile commands
at bogus paths in a seeded config, since `preview_app` can't clear `PATH`. Write the recipe down in
`tests/qa/README.md` or as a Makefile target, not as a one-off script.

## Recommended slice (smallest first)

1. **Fix or remove `pattern_auto_approval`** (S-M): enforce it in `gate_approval`/`rules_allow`,
   with a test, or remove both switches. Drop "for smooth operation".
2. **agy hook status + warning in setup's agy group** (S): reuse `hook_config::badge` and the
   Copy button. Keep it read-only; an auto-install into `~/.gemini` would need its own security
   review.
3. **"Check for Updates at Startup" switch in setup** and defer the first check until setup
   closes (S).
4. **Ready summary on the agents page** ("Ready: Claude" / "No agent ready"), a visible sign-in
   subtitle, link buttons for install hints, and a Check Again button (S-M).
5. **Move the two approval switches into shared row builders** (S); read
   `confirm_rule_modification` from memory (S).
6. **README**: describe the approvals group and update check, reword Install, and add a
   Troubleshooting entry. Add the screenshots (S-M).
7. **Regression check**: a native test that opens setup with an empty `XDG_CONFIG_HOME`, closes
   it, and asserts that `config.json` exists and `is_first_run() == false` (S).

Don't build: an in-app CLI installer (it breaks the standalone philosophy), a multi-page wizard,
or a "setup complete" screen.
