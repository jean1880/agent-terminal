# Delegation route evidence, 2026-10-15

The installed versions are Claude Code 2.1.300, Codex 0.161.0 and agy 1.3.5. The
reproducible harness is `tests/qa/delegation_proof.py`, run only through
`make delegation-proof`. Its hermetic harness tests join ordinary QA checks.

Claude and Codex tests launch the **real installed CLI**, real fixture MCP
subprocesses and their ordinary session persistence. A deterministic HTTP model
fixture listens on an ephemeral loopback socket, supplies protocol responses, and
captures only tool names. It uses no cloud inference, user credentials or user
configuration. These are CLI enforcement and persistence proofs; they do not
claim a cloud-model or complete GUI route test. Supervisor integration tests
separately cover the app's authentication, bootstrap, typed task dispatch,
publication and durable replay.

Fixtures are versioned `*.proof.json` gate projections in
`tests/fixtures/real/delegation/route-proof/`, with explicit provider, evidence kind
and frame arrays. Export omits transcripts, incidental model prose, credentials,
account data and filesystem paths; identifiers are fixture pseudonyms and relative
timestamps retain the attestation timing. Only the harness's own fixed synthetic
tool arguments are retained. Private temporary provider recordings are never
published. The exact capability canary is scanned in the disposable provider home
before removal; auth files and symlinks are excluded. No credentials are copied
or read by the harness.

| Gate | Result | Direct evidence |
| --- | --- | --- |
| G-C1, restricted Bash | Passed | The local model emits a valid Bash `tool_use`; the installed CLI raises `can_use_tool`; a host deny prevents `touch proof-denied`. |
| G-C3, Claude token persistence | Passed | A capability exists only in the MCP overlay's server environment; no exact canary appears in any generated provider-home file after the completed turn and process exit. |
| G-X1, Codex ambient MCP isolation | Passed with a compatibility correction | Effective config contains an enabled `ambient` fixture server. Per-thread disabling prevents its process from starting, and every observed MCP startup status names only `agent_terminal`. Sandbox and approval policy read back exactly. |
| G-X2, Codex native delegation | **Failed; Codex children cannot be admitted** | The actual outgoing Responses request advertises `collaboration.spawn_agent`; a valid invocation produces `subAgentActivity` with a new `agentThreadId`, despite both process and per-thread feature disables. An enabled positive control also starts a worker. |
| G-X3, Codex token persistence | Passed | No exact capability canary appears in the disposable provider home, including session rollouts and SQLite files. |
| Codex root overlay on resume | Passed | A fresh app-server resumes the saved native thread with the overlay resent, starts a fresh MCP process and completes another turn through the local model fixture. |
| G-A1, agy hook payload/enforcement | Passed | A real agy cloud turn calls the actual app approval-hook client. Its own MCP call is allowed, a foreign MCP call is denied, and a native sub-agent invocation is denied. Only the own MCP call reaches its server. |
| G-A3, agy token persistence | Passed | No exact capability canary appears in generated private-home files after the completed cloud turn and process exit. The existing native auth source is bound read-only, never copied. |
| G-A5, agy invocation attestation | Passed | The provider's step and the MCP call agree exactly on conversation, progress-token step index, server, tool and arguments; the step arrives within two seconds of the call. Fixtures retain arrival timestamps. |

G-X2 remains failed with `agents.max_depth=0` and `agents.max_threads=1`.
`agents.max_threads=0` is rejected: the installed CLI requires at least one
concurrent thread. Feature flags read back false, so their echo is explicitly
insufficient evidence. The generated app-server schema has no per-turn built-in
tool exclusion field. Codex **parent** routes can be admitted independently;
Codex child routes must report their native-delegation blocker clearly.

Admission records each gate independently in `delegation/route.rs`. On
2026-10-15, native Codex runtime and security reviews and an independent Claude
source review passed the final implementation. The coordinator then admitted
Claude and agy children **with a bridge** from Claude, Codex and agy parents in
Plan, Ask and AcceptEdits. Codex children remain Unsupported, with
`codex_native_delegation` as their sole measured blocker.

The reviewed application gates cover managed entry/rules/source enforcement,
full L2 workspace checks, L3/L4 caller binding, fresh hook proof and actual
generation launch facts. Regression evidence includes 25 supervisor runtime
tests, 190 session tests and three exact receipt-correlation tests. These native
scripted tests and source reviews establish application enforcement separately
from the installed-provider fixtures above; they do not claim cloud end-to-end
execution of every admitted parent/child/mode combination. Per-launch checks can
still withhold a route when the selected configuration or workspace is unsafe.
The gate table remains authoritative.

Only agy's **with-bridge** variant is eligible for admission in this release.
The without-bridge variant remains Unverified with its evidence-bundle blocker;
public spawn and capabilities do not select it or offer an automatic fallback.

The app's final AGY launch guard checks the actual argv, physical working
directory, effective HOME, configuration redirect names and hook proof before
spawning. It withholds stdin while that check runs; Stop or replacement prevents
the cancelled process from starting or receiving held input. Successful process
facts are retained without general environment values, logging or serialisation,
and subsequent checks use that generation rather than a freshly resolved profile.
Native scripted tests cover final-guard refusal, frozen launch facts, cancellation
during preparation, ignored interrupts, stale terminal receipts and durable
publication. These are application enforcement tests, not additional cloud runs.

Two compatibility corrections were found by the real trigger path:

1. Codex validates transport even on a disabled server entry. An absent
   `codex_apps` entry with only `enabled=false` fails `thread/start` with
   `invalid transport`. The child overlay supplies an inert, credential-free
   transport for absent built-in names before disabling them. If a future
   provider ignores disabling, the foreign-name startup guard still stops it.
2. Claude 2.1.300 ignores the obsolete `TodoWrite` tool even when named in
   `--tools`. Requiring it in strict readback makes every child fail. The
   coordinator approved removing it from both explicit tool lists; Plan now
   requests `Read,Glob,Grep`, and writer modes add
   `Edit,Write,NotebookEdit,Bash`. Exact readback remains required.

To reproduce without cloud inference:

```sh
make delegation-proof PROOF_ARGS=--dry-run
make delegation-proof PROOF_ARGS=--provider=claude
make delegation-proof PROOF_ARGS=--provider=codex-parent
make delegation-proof PROOF_ARGS=--provider=codex
```

The last command currently exits non-zero because G-X2 is a measured failed
gate. It still records independent passing gates; this failure is not hidden by
the parent-only proof.

agy requires its cloud backend. A completely isolated home fails authentication
before any inference (zero turns and tokens). The successful proof used an
explicit `--cloud` probe and one existing native auth file
through a read-only `bwrap` bind; the host filesystem is read-only and only the
disposable fixture directory is writable. The harness never reads the auth
source and cannot refresh or overwrite it. Missing auth or a refresh refusal
leaves the affected gates unavailable. The hook fixture calls the real
`agent-terminal --approval-hook` client and denies foreign MCP and native agent
tools through its local Unix approval fixture. No persistent provider config is
edited.

The passing agy command is reproducible with a built application and a valid
native auth source. The source path is metadata, not a credential value:

```sh
make delegation-proof PROOF_ARGS='--provider=agy --cloud --app=/absolute/path/to/agent-terminal --agy-auth-file=/absolute/path/to/antigravity-oauth-token --dry-run'
make delegation-proof PROOF_ARGS='--provider=agy --cloud --app=/absolute/path/to/agent-terminal --agy-auth-file=/absolute/path/to/antigravity-oauth-token'
```

All fixture processes have independent process groups, finite response
deadlines, escalation and group cleanup. The temporary homes are removed after
scanning. Rollback is to revert the source/fixture commit; there are no changes
to installed applications, user settings or live infrastructure.
