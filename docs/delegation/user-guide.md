# Delegate work between agents

Agent Terminal includes a local delegation server in its existing executable. Open
Settings → Agents and enable **Available for delegation** for each profile you want
to use as a parent or child. Reopen an existing thread to attach its bridge. Turning
availability off revokes its delegation access and stops affected children.

Claude and Codex use a temporary bridge configuration for each process. agy needs a
managed registration: turning **Available for delegation** on for agy reviews it (the
same as **Configure / Repair** in the delegation settings). Check the selected profile,
home and files, then apply; the switch stays on only once the registration is ready.
Group- or world-writable folders and files on its path are listed with their mode
change and repaired as part of the apply. The helper preserves other servers and
permission rules. Turning the switch off offers **Remove**. The registration row shows
its state whenever the settings open, naming any path it refuses.

If an opted-in agy thread starts while its registration is unusable, it starts without
delegation (as if opted out) and its thread says why, rather than refusing to start.
**Test** distinguishes registration and headless MCP startup from an authenticated
thread binding. **Remove** removes only managed entries and rules. **Undo** restores
the previous managed change only when the current files still match its recorded
images; it refuses to overwrite later edits.

agy routes require the managed bridge and permission rules in the profile's actual
home, plus successful workspace and process checks. This release supports agy with
its bridge only. It does not fall back to an agy child without a bridge when those
checks fail; capabilities reports the withheld route and its blockers.

A parent receives eight tools: `cross_agent_capabilities`, `cross_agent_spawn`,
`cross_agent_send`, `cross_agent_read`, `cross_agent_wait`, `cross_agent_list`,
`cross_agent_cancel` and `cross_agent_close`. Start with capabilities to see available
profiles, route support, authority and delivery limitations. Reuse an idempotency key
only for an identical operation; a receipt means acceptance, and subsequent reads
report delivery and completion separately.

Capabilities also helps a parent choose. Each model can carry tags (what it is good
for), a tier and an effort per task tier, read from a model tag file you write
(`model_tags_path` in the config; by default `model-tags.json` beside it). Without
that file, models are still listed, untagged. Each profile reports its plan usage: the
fullest window's share (`load`) and a headroom of ok, low, exhausted or unknown, so a
parent can spread work across agents with room rather than default to its own. Pass
`refresh: true` to read usage again first; each agent is read at most once a minute,
and a signed-out agy never. The same usage, without any account details, is kept in
`usage.json` in agent-terminal's cache folder for tools outside the app.

Children appear beside native workers in the worker dock. Select one to inspect its
activity, transcript, messages, approvals and file changes. Its provider, requested
model and reported model are distinguished. Effort is shown with its evidence;
missing provider metadata stays unknown. An approval belongs to the displayed
worker and process generation. A stale card cannot answer a newer request.

Plan children cannot receive more authority than their parent. Writing tasks require
an isolated git worktree, starting at a selected commit; uncommitted parent changes
are absent. Session and Always grants remain confined to the child's workspace and
parent authority. The app does not merge child edits automatically. Keep the worktree
to inspect or integrate them; removal requires a clean worktree and never forces it.

**Stop** cancels the current root work and its supervised descendants. Closing the
root page stops its children. Moving a page to another window stops its descendants
and restarts the root with fresh credentials. The app waits for observed process
exits before reusing process capacity. A completed child turn can receive another
message; **Close** ends that worker. Busy delivery differs by provider and is reported
as strict, best effort or unsupported rather than silently falling back.

After an app restart, unfinished work is uncertain. History remains available, but
the app does not replay edits or signal recorded process IDs. Explicitly resume work
when offered. Persisted and injected text passes through redaction and size limits.

Every agent can use every other as a sub-agent, Codex included. Codex 0.161.0 as a
**sub-agent** comes with a caveat: it still starts its own built-in sub-agents despite
its disable settings, and those run outside Agent Terminal's depth, mode and worktree
limits (the worker dock shows them). The capabilities response reports it as
`route_caveats: ["codex_native_workers_unsupervised"]`, and the settings say so where
you turn delegation on. Recorded provider proofs, labelled by provider and evidence type,
live under `tests/fixtures/real/delegation/`.

To roll back the feature, disable delegation on profiles, close their threads and
remove the managed agy entry. The standalone homelab `cross-agent` command is separate
and is unchanged. The store uses schema 5; retain its pre-migration backup and do not
open a migrated store with an older binary in place.
