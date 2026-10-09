# Codex approvals disappearing after reconnect

Status: public release blocker; source fix validated locally, not committed or deployed.

## Verified evidence

The running application reported version 3.0.1. Read-only queries of its stored
events confirmed four command approvals with reused JSON-RPC request IDs:

| Request | New approval, local time on 2026-10-08 | Expired after interruption | Wait |
|---|---|---|---|
| 0 | 13:05:54 | 13:15:26 | 572 seconds |
| 1 | 13:15:46 | 13:20:29 | 283 seconds |
| 2 | 13:23:26 | 13:29:06 | 340 seconds |
| 3 | 13:30:39 | 13:40:39 | 600 seconds |

Each ID already belonged to a resolved approval earlier in the same conversation.
The new requests have different command item IDs and no resolution before the
user interrupts. No raw command arguments or conversation content are needed to
establish the collision.

## Cause

Codex JSON-RPC IDs are scoped to its app-server connection and restart from zero.
The adapter used the raw ID as the canonical request identity. The transcript
retains request identities across history replay and ignores an approval whose
identity already exists. After reconnect, the adapter therefore waits for a new
approval that the transcript treats as a duplicate of an old resolved card.

This is a request identity defect rather than evidence of a slow Git command.
The sandbox's immediate SSH configuration-permission failure explains why the
commands request escalation, but is separate from the disappearing approval.
A shell timeout cannot expire an approval that precedes starting the shell.

## Separate SSH sandbox failure

Codex CLI version: 0.161.0. Inside its current sandbox, `ssh -G github.com`
fails while parsing configuration, without attempting a network connection.
Following the SSH configuration symlink reports its target as mode 0644,
UID/GID 65534. System executables likewise appear owned by UID/GID 65534.
`/proc/self/uid_map` and `gid_map` report `1000 0 1` in this execution
environment. These observations identify user-namespace ownership translation
as the likely reason SSH rejects a system configuration that works in the
user's regular terminal. The symlink's 0777 mode is normal and is not evidence
that the target needs chmod. Host-side stat and SSH parse comparison were
requested and are still pending.

Agent Terminal launches `codex app-server` with profile arguments and passes
the thread's cwd. It sends explicit approval and sandbox policies on every
turn; Accept Edits currently uses on-request/workspaceWrite with no network
access enabled. Therefore changing a default in Codex config must not be
assumed to override the policy supplied by the application. Network access
alone would not fix SSH's ownership validation.

Working from the repository helps scope permissions and load trusted project
configuration, but does not change the observed system-file ownership
translation or repair approval identity collisions. A fresh application thread
is a temporary workaround for historical request-ID collisions, not a fix for
reopening existing threads. Once approvals are visible, an approved command
outside the sandbox is the existing path for operations the sandbox cannot
perform. Preserve this explicit permission boundary rather than silently
disabling sandboxing or altering system SSH ownership.

## Fix and release gate

Give canonical Codex approval and question requests an identity scoped to their
provider thread, turn, item and method, retaining the original JSON-RPC value for
wire replies. Resolve provider notifications through that same canonical
identity. Preserve historical cards and genuine duplicate suppression.

Regression coverage must replay an old resolved numeric request, reconnect with
the same numeric ID, show a fresh pending approval, answer it and verify the
original numeric ID in the wire response. Repeat after another reconnect.
Cover questions, expiry and numeric versus string wire IDs as well.

Validation completed:

- The transcript reconnect regression fails against the pushed release source in
  an isolated worktree and passes with the patch. The temporary worktree was
  removed after the comparison.
- All 228 agent-core tests and 34 transcript model tests pass.
- Workspace formatting, strict Clippy across all targets and diff checks pass.
- Independent review confirms the observed collision and the patch's response
  routing. Truly identical native thread/turn/item and RPC IDs across reconnects
  would still denote the same canonical request; there is no evidence of this
  edge case in the observed failures.

Before clearing this blocker, verify the packaged fix in the real application:
reopen a conversation with approval history, request a harmless command requiring
approval, check that its card and needs-approval status appear, allow it and
confirm execution completes. Repeat after reconnecting again. No automatic
approval or permission bypass is part of this fix.

The user reports pushing master and v3.0.1. That tag triggers package publication
automatically. Publication status could not be checked from this sandbox; do not
assume the release is held or withdrawn. Do not move the existing tag.
