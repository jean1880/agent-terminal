# agy delegation discovery (P0, D1–D7)

Scout-Agy, 2026-10-14, `agy` 1.3.5, branch `feat/cross-agent-runtime`. The evidence is in
`tests/fixtures/real/delegation/agy/` (see its README). The scripts are in the git-ignored
`target/delegation/agy/bin/`. Every live run used a throwaway `HOME` and workspace, `env -i`
and `--mode plan`, and none used `--dangerously-skip-permissions`. The real `~/.gemini` was
not touched: strace shows no opens under it, and the files' mtimes are unchanged.

## Bottom line

- **agy as a child: feasible, with conditions.** Binding through inherited launch env works.
  Calls carry correlation ids. Delivery is next-turn only. Plan mode does **not** stop MCP
  calls, so the bridge must enforce the child's ceiling itself.
- **agy as a parent: feasible only with persistent registration.** There is no documented
  per-launch MCP override. The managed entry must live in the global
  `~/.gemini/config/mcp_config.json`. A hookless agy auto-denies every MCP call unless
  `~/.gemini/antigravity-cli/settings.json` has `permissions.allow: ["mcp(<server>/<tool>)"]`.
  The alternative is the proven-hook launch (skip flag + app hook), where the app's hook sees
  every `call_mcp_tool`.
- **Existing hook proof gaps (not new code):** agy also runs
  `<workspace>/.agents/plugins/<p>/hooks.json`, and a `--gemini_dir=` in profile args moves
  every hook file. `hook_config::check_target` checks neither. Fix this before agy delegation
  ships (see D6).

## D2: provider registration. Verdict: PERSISTENT ONLY (no supported per-launch override)

These are the MCP sources agy 1.3.5 reads. `$G` is `$HOME/.gemini` (or the `--gemini_dir`
value), `$W` is the workspace: the cwd and its ancestors up to the git toplevel.

| Source | Read | Servers started | Evidence |
| --- | --- | --- | --- |
| `$G/config/mcp_config.json` | yes | yes. The only file `agy mcp list` shows. `agy mcp add` writes here and has no scope flag | `d2-mcplist.txt`, `d2-config-probes.txt` |
| `$G/config/plugins/<p>/mcp_config.json` | yes | yes. The name is namespaced `<p>_<server>` | `homeplugin` → `hplug_hpfake` |
| `$W/.agents/mcp_config.json` | yes | yes | `wsagents` |
| `$W/.agents/plugins/<p>/mcp_config.json` | yes | yes, as `<p>_<server>` | `wsplugin` → `wplug_wpfake` |
| `$G/antigravity-cli/plugins/<p>/`, `$G/plugins/<p>/mcp_config.json` | yes (opened) | **no** by default. Enablement rule UNVERIFIED | `agycliplugin`, `geminiplugin` never started |
| `$G/antigravity-cli/mcp_config.json` | probed once (ENOENT, first launch) | no, not opened when present later | `agycli` never started |
| `$W/.mcp.json`, `$W/.gemini/config/mcp_config.json` | **no** | no | not in strace |
| `/etc/antigravity/admin_settings.json` | probed | the binary has `allowed_mcp_servers` strings, so an admin allow-list can apply. UNVERIFIED | `d2-config-probes.txt` |
| Env: `CASCADE_ENABLE_MCP_TOOLS`, `CASCADE_MCP_SERVER_INIT`, `CASCADE_GLOBAL_CONFIG_OVERRIDE` | strings in the binary only | UNVERIFIED. Not tested. `hook_config` already refuses on `CASCADE_GLOBAL_CONFIG_OVERRIDE` | `strings-env.txt` |
| Flag `--gemini_dir=<dir>` (and `--app_data_dir=`) | **works**, absolute or relative to HOME | replaces **all** of `$G`: MCP, hooks, settings, conversations | `d2-gdirabs.txt`, `d2-gemini-dir-probes.txt`. Seen only in agy's own `--bg-updater` argv. Undocumented |

**Name precedence:** the global file wins over `$W/.agents` for the same name: the workspace
`dup` server never started. A repository therefore cannot shadow the exact managed global name.
It can add look-alike names, which the model sees alongside it.

**Consequence:**
- Implement registration as the plan's §8 items 5 and 6: an atomic, previewed, undoable edit
  of one entry in `$HOME/.gemini/config/mcp_config.json`, with an absolute
  `agent-terminal --cross-agent-mcp` command. Verify it by parsing that file. Check that the
  exact name maps to our absolute command, and treat a duplicate of our name in
  `$W/.agents/**` as a visible conflict. `agy mcp list` is a secondary check of the global
  file only.
- Do **not** use `--gemini_dir` as the override. It is undocumented, it drops the user's rules,
  skills, MCP servers, settings and conversation history (so resume breaks), and it amounts to
  the forbidden private-home variant. Setting `HOME` also changes HOME for every shell command
  the agent runs. Keep both UNSUPPORTED.
- A global entry starts in **every** agy session, including the user's own terminal agy and
  side commands (`agy models` started the global servers). Outside an app launch,
  `--cross-agent-mcp` must answer `runtime_unavailable` quickly and cheaply (plan §8 item 9).
- Deployment note, not app logic: on this workstation, agent-sync owns
  `~/.gemini/config/mcp_config.json` (a `*.agent-sync.bak` is present). An app-written entry
  will show as drift, or be reverted, unless it is added to `agent-config/sync/mcp.toml`.

## D1: parent binding. Verdict: SUPPORTED through the inherited launch env (behind D2)

- Handshake (`d1-d3-mcp-exchange…`): `server/discover` (protocol `2026-07-28`, answered -32601,
  tolerated), `notifications/roots/list_changed`, `initialize` (`2025-11-25`, clientInfo
  `antigravity-client`), `notifications/initialized`, `tools/list`, `tools/call`. The bridge
  must reject `server/discover` with -32601 rather than fail.
- Environment: every server receives agy's full launch env plus the entry's `env` (names in
  `d1-d2-server-starts.jsonl`: `DELEG_CANARY_PARENT`, `AGENT_TERMINAL_DELEGATION_ENDPOINT`,
  `FAKE_DECLARED_ENV`). Each server's cwd is the workspace and its parent is the agy process.
  An endpoint or capability in the launch env does reach the bridge.
- **Risk:** the same capability reaches **every** server agy starts, including
  repository-controlled `$W/.agents/mcp_config.json` and workspace plugin servers. No trust
  prompt appeared in headless mode. This is within the plan's "a same-UID bearer is not a
  sandbox", but it means a cloned repo can harvest the delegation capability of any agy thread
  opened in it. Give each launch a narrowly scoped, short-lived capability, and consider
  refusing agy delegation when `$W/.agents/**/mcp_config.json` exists (like the hook rule).
- Restart: `--conversation` resume restarts every server with a fresh `initialize`
  (`d4-d7-resume…`). Bind per process generation and never assume a server carries over.

## D3: invocation correlation. Verdict: SUPPORTED (exact)

`tools/call.params._meta` carries `antigravity.google/conversation_id` (equal to the stream's
`conversation_id`), `antigravity.google/artifacts_dir`, and
`progressToken = "<uuid>:<step_index>"`. The suffix equals the `step_update.step_index` of the
`call_mcp_tool` step (2 and 3 in the fixture). The uuid prefix is not in the stream. The stream
step names the call through `tool_info.parameters.{ServerName,ToolName,Arguments}` and, when
done, `output`. Link by (conversation_id, step_index). The JSON-RPC `id` is per-server and
carries no meaning for linking.

## D4: model and effort. Verdict: LAUNCH_CONFIGURED only

`init.model` appears only when `--model` is passed, and it echoes the argument (base id
`gemini-3.8-flash` was echoed and the turn succeeded). `--effort low` was accepted and never
echoed. `agy models` lists ids with the effort built in (`d4-models.tsv`). Nothing in the stream
reports the effective model. Model is `launch_configured` (or `unknown` without `--model`), and
effort is `launch_configured` or `unknown`. Whether the base id and `--effort` were honoured is
UNVERIFIED (no provider evidence), so keep the adapter's composed-id behaviour.

## D5: delivery. Verdict: NEXT-TURN QUEUE ONLY, with no live steering (confirmed)

- A second NDJSON message written ahead of time ran as a **separate** turn after turn 1's
  `result` (`d3-d5…`: result `num_turns` 1, then a new `user_input` step, then result
  `num_turns` 2). It was never injected into the running turn.
- **Gotcha:** when a turn ends on a denied action and another message is already queued, agy
  emits **no** result for that turn. One result (`num_turns` 2) covered both prompts, and the
  first prompt's answer was lost (`d5-d6…`). Keep the host-side queue and write only after
  `result` (as `Capabilities::live_input = false` already does). Never pre-write. Count turns
  from `user_input` steps and `num_turns`, not from the number of result frames.
- SIGINT drops anything queued in stdin (`d7-interrupt`). Durable mailbox delivery must re-send
  after a restart.
- `this_run` = unsupported, `auto` = queue for the next turn, as in plan §7.

## D6: safety. Verdict: PARTIAL. The hook proof holds per worktree but has two gaps. Plan does not gate MCP

- **Worktree:** in a linked worktree at `<wt>/sub`, agy probed only `<wt>/sub/.agents` and
  `<wt>/.agents`, never the main tree's `.agents` (it reads the main `.git` only for git
  metadata) (`d6-worktree-probes.txt`). `hook_config::target_sources` already walks cwd → git
  toplevel, and `agent_kit::git::discover` returns the worktree root. The proof therefore holds
  **if it is computed for the child's actual worktree cwd** (HookTarget is keyed by home + cwd).
  The child's workspace hook and MCP files come from the **base commit**, so check them after
  the worktree is created, never on the parent's tree.
- **Gap 1, HIGH (existing code):** agy runs `$W/.agents/plugins/<p>/hooks.json`. A harmless
  PreToolUse marker hook fired on `call_mcp_tool` (`d6-workspace-plugin-hook…`). `target_sources`
  checks only `.agents/hooks.json` and `hooks/hooks.json`. Following its own fail-closed rule, a
  present workspace plugin hook must refuse the proof.
- **Gap 2, HIGH (existing code):** `--gemini_dir=` (and probably `--app_data_dir=`) in profile
  `extra_args` moves every hooks file (`d2-gemini-dir-probes.txt`), while `check_target`
  proves `$HOME/.gemini`. The adapter strips only the skip flag. Strip or refuse these flags, or
  refuse the skip flag whenever one is present.
- **Plan mode:** headless agy in `--mode plan` **executed** an allowed MCP call
  (`d3-d5…`). Plan is not an MCP gate, so the bridge must apply the principal's mode ceiling
  (refuse writer spawns from a Plan parent, for example). Without the skip flag every `mcp`
  call is auto-denied unless the user's settings allow it. Hooks (PreToolUse) ran without the
  skip flag too.
- With a proven hook, the app's own hook sees the parent's `call_mcp_tool` to the bridge. It
  must recognise its own bridge tools and apply delegation authority there, not raise a second
  generic prompt.

## D7: lifecycle. Verdict: SUPPORTED (process-scoped)

The MCP servers are direct children of agy. After a clean stdin EOF, agy exits after the last
result and the servers see EOF. On SIGINT the result is `status ERROR, error "interrupted"`,
the process exits, no server remains, and queued input is lost. Resume restarts every server.
There is no in-process reconnect: a bridge disconnect means the agy process (or the turn) is
gone. Nested stop or root Stop = SIGINT the child process and record the observed exit.

## Open (UNVERIFIED)

- The enablement rule for `$G/antigravity-cli/plugins` and `$G/plugins` MCP configs.
- The effect of the `CASCADE_*MCP*` env vars and of `/etc/antigravity/admin_settings.json`
  allow-lists.
- The hook payload shape for `call_mcp_tool`. It needs a skip-flag run with the real app hook,
  which this scout was not allowed to do.
- Whether `--effort` or the base model id is honoured.
