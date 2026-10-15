# P6 scoped worker view verification

Builder-MCP, 2026-10-15. Source: `src/chat/view/delegation.rs`, the shared worker
explorer/shelf/panel, and the additive `ChatBackend` worker methods.

The root transcript receives no child envelope. Each `(worker, generation, run)`
owns a separate `Transcript`, with private native-to-opaque item and request
lookups. Registry summaries join native workers in the existing explorer and
shelf; a supervised handle appears once even when a linked snapshot also exists.
The inspector uses the same card builder as native workers and retains each
run and process generation in its history selector.

Provider marks use bundled icons and the existing accent classes. Model and
effort labels distinguish provider-reported, provider-accepted, configured,
unsupported and unknown values. Historical runs retain their identity snapshot.
An accepted/configured value is never labelled effective.

Send, interrupt, close, approvals, questions and forms carry an exact `Scope`.
The backend supplies the current scope, including queued workers before their
first process event. The view never guesses generation zero. Controls keep the
scope they last displayed; a delayed click cannot retarget itself to a replacement
process. Historical requests expire, and scoped request lookup prevents a child
card from calling a root approval method. The backend must still recheck scope
and authority at dispatch.

Diff requests carry the original child item id and obtain a source through
`worker_diff_source(scope)`. There is no fallback to the root's workspace or turn
baseline. Root window integration owns the concrete child diff source.

UI updates are coalesced on the main context. Events that precede their registry
are buffered up to 64 events/4 MiB; overflow is surfaced. The subscriber retains
the view weakly. A closing view does not stay alive through the supervisor.

Completed verification:

- `cargo check`: passes. In the isolated builder checkout, its only warning was
  the subscriber entry point awaiting window integration.
- `cargo test --bin agent-terminal chat::view::delegation::tests`: **4 passed**.
  Covers root turn/usage isolation, identical provider ids across scopes,
  generation history and opaque requests, stale approval expiry, bounded
  out-of-order events, wrong-root rejection and authoritative queued scopes.
- `window::imp::tests::test_window_initialization`: **passed**, on private Xvfb,
  D-Bus and XDG directories matching the Makefile's test environment. Its shared
  `ui_checks` now exercises supervised file-change/approval cards, exact child
  approval and diff routing, sending, generation history selection and stale
  click refusal.
- A provenance-label regression verifies that requested or accepted values
  cannot masquerade as an effective model.
- Rustfmt and `git diff --check` pass on owned files.

The private-display check uses an in-memory backend, not live provider accounts.
It proves rendering and routing, not admission of a provider pair. Coordinator
integration must run the full workspace gate after connecting the subscriber,
canonical scope lookup, action dispatcher and child diff baselines.

No installed binary or user configuration was changed by this lane. Undo is a
revert of the P6 source integration and its matching window integration.
