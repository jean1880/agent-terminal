# P5 headless MCP bridge verification

Builder-MCP, 2026-10-15. Implementation: `src/cross_agent_mcp.rs` and
`src/cross_agent_mcp/{client,stdio,tests}.rs`. This is local protocol verification;
it does not by itself admit a provider route.

The headless entry runs rmcp 3.5.1 on a Tokio current-thread runtime with I/O and
timers enabled. Its only output on stdout is JSON-RPC. It starts the Unix-socket
hello eagerly, independently of the MCP lifecycle. The initial welcome deadline
is two seconds from entry; an authenticated transport loss fails pending calls
without replaying them, then permits one reconnect with a fresh two-second
deadline. A refusal or failed reconnect leaves only the capabilities tool listed.

The shared core owns all eight schemas, typed request validation, invocation
metadata whitelisting and final output budgeting. Incoming arguments remain
unchanged for supervisor attestation. No MCP request id supplies provider
identity. Neither credentials nor frames are included in diagnostic messages.

The custom stdio transport bounds each line before JSON parsing and checks both
the original encoded request id and its parsed representation. This matters for
Unicode escapes and numbers, which serde_json normalises. Every id over 256
encoded bytes produces `-32600` with `id: null`; the rejected value is not echoed.
rmcp supports numeric request ids only within `i64`; larger numeric ids within
the byte bound remain invalid requests, while oversized numeric encodings are
rejected by the earlier bound.

The SDK's 2026 lifecycle interprets cancellation for subscriptions. A bounded
bridge registry additionally signals the exact active handler token, including
the notification-before-handler race, so delegation waits release in either MCP
lifecycle. Cancellation sends IPC `cancel` and does not undo accepted mutations.

Validation completed using a GTK-free scratch harness under the ignored
`target/delegation/mcp-bridge-proof/`, loading the production module directly:

```text
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/home/jdesroches/git/agent-terminal/target cargo test --manifest-path target/delegation/mcp-bridge-proof/Cargo.toml
11 passed; 0 failed

CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/home/jdesroches/git/agent-terminal/target cargo clippy --manifest-path target/delegation/mcp-bridge-proof/Cargo.toml --all-targets -- -D warnings
Finished, no warnings
```

The tests remain in the production module and run in the app suite after main
module integration. They exercise both MCP lifecycles, structured success/error
results, degraded discovery, unknown tools/methods, schema errors, metadata
whitelisting, cancellation, concurrent out-of-order results, eager hello,
startup deadlines, reconnect without replay, original wire id boundaries,
oversized input and the final output guard. Rustfmt was applied to owned files.

Undo: remove the headless main dispatch and its module declaration, then remove
these source files and the two SDK dependencies. No live provider settings or
installed binaries were changed by this lane.
