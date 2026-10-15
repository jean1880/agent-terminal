# P9 managed registration hardening

Builder-MCP, 2026-10-15. Native review and follow-up implementation; this is not
a heterogeneous reviewer pass. The initial native review found three blockers:
unvalidated parseable journals, lost managed-rule ownership after Remove → Undo,
and a failed selected-profile environment load falling back to the application
home. The coordinator reviews the resulting implementation independently.

The recovery journal now validates its version, exact two target paths, bounded
JSON images and unique exact managed-rule subset before any mutation. Version 2
records ownership before and after a change. Undo preserves prior ownership in a
non-undoable ledger, so a second Undo cannot erase it and a subsequent Remove
still removes only rules added by the application. The journal marks an apply
incomplete before either target changes; an interrupted apply must be undone
before another registration change. Partial undo can be retried.

The selected home and every existing path component must belong to the current
user and refuse group/world writes, symlinks and multiply linked files. Preview
shows the profile, home and both exact paths. Apply compares both the config
preimages and the ownership journal under the application lock, then compares
each target again immediately before publication. An uncooperative same-user
writer can still race a final rename; these checks do not claim a filesystem
compare-and-swap against such a writer.

Settings loads the selected profile's environment afresh on a worker using
`load_env_file_checked`; failure stops the operation. Configure, Remove, Undo
and Test use that same selected home. Test additionally executes the actual
current binary's headless bridge in that environment. It strips bindings after
profile overrides, checks initialize, tools/list and capabilities, and expects
the unbound result. It neither consumes provider quota nor claims an
authenticated live child route. Replies and waits are bounded; all outcomes
stop the diagnostic process group, and server output is never included in
errors or logs.

GTK-free production-helper verification: **11 tests passed**; strict all-target
clippy passed. Cases cover private/idempotent apply, unrelated settings,
conflicts, later edits, symlinks, Remove → Undo → Remove ownership, parseable
invalid journals, interrupted apply/undo, stale ownership previews, unsafe home
permissions, selected-environment MCP handshake with binding stripping,
untrusted reply suppression and timeout cleanup of a descendant process.

A second non-quota integration check connected the production diagnostic to the
production rmcp shim through two ignored scratch executables: **passed** for
initialize, discovery and the expected unbound capabilities result. The copied
coordinator files were byte-identical to the tested helper and Settings module.
The coordinator performed a fresh native source review with no remaining
blocker; the original reviewer rechecked all three findings and the home check.
This is a native review pass, conditional on the integrated application gate.

The fixture home explicitly uses mode 0700. Production code was compiled by an
ignored scratch harness importing the production source; the coordinator must
also run the integrated application gate after connecting its checked loader.
No user registration or installed binary was changed by these checks. Undo for
the source changes is a revert; user-facing registration changes retain their
private recovery journal and explicit Undo action.
