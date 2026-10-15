# Contract v1 review resolution (revisions 2 to 6)

Inputs:
- the Codex review (architect and red team, findings 1–22, CHANGES-REQUIRED);
- the agy review (red team, AT-DEL-01..07, CHANGES-REQUIRED).

Both were run on 2026-10-15 against `docs/delegation/contract-v1.md` revision 1. Every finding is resolved in
revision 2 of `contract-v1.md`, `p1-types.md` and the plan's P0 amendments.

Revision 2 totals: **23 FIX, 6 NARROW, 0 REJECT.** Revision 3 totals: **14 FIX, 0 NARROW, 0 REJECT** (below).

## User decisions (2026-10-15, binding)

These decisions were taken by the user. Revision 3 is written to them.

1. **Accepted: the depth and mode limits cover supervised delegation only** (Codex 3).
   - In delegated children, built-in sub-agents and every other MCP server are disabled.
   - Root sessions keep their own sub-agents, which agent-terminal shows but does not limit.
   - Revision 3 also makes sure a root's native sub-agents cannot break the root's delegation (agy R2-3).
2. **Changed: Claude `this_run` is best-effort, not unsupported** (Codex 15).
   - Capabilities advertise `delivery.this_run` as an enum: Codex `strict`, Claude `best_effort`, agy `unsupported`.
     `Controls.this_run` uses the same enum.
   - The receipt states only what is known at acceptance.
   - The message record later reports either `provider_confirmed`, or the new terminal delivery state `misdelivered`
     with the actual `recipient_run_id`, when the replay echo shows the line landed in the next turn.
   - Delivery is a state, not an error. `DeliveryState` grows from 8 to 9 values, and `ErrorCode::ALL` stays at 25.
3. **Allowed: a private `CODEX_HOME` for Codex children only**, as the G-X1 fallback (Codex 1). It is never used for
   roots.

## Revision 2 resolution table (first review round)

| Finding | Resolution | Where | Rationale |
| --- | --- | --- | --- |
| Codex 1: Codex sandbox does not cover ambient MCP servers | FIX | §5 child rows (MCP isolation, read-back), §11, p1 `codex_child_config`, `codex_child_servers_ok`, `RouteBlocker::CodexAmbientMcp` | G-X1 is a route blocker and the first P5 proof gate. Whatever the mechanism, a `mcpServer/startupStatus/updated` naming any server other than `agent_terminal` (including the built-in `codex_apps`, seen in every fixture) stops the child before its first turn |
| Codex 2: Plan send restriction bypassable by `reply` | FIX | §2 direction rules, §3 send, §6 ceiling and E7, p1 `delivery_allowed` | The ceiling applies to every parent→child delivery of any kind, checked at acceptance and at dispatch |
| Codex 3: native spawning and other MCP launchers bypass depth/mode | NARROW | §6 "Scope of the depth and mode guarantee", §5 child native delegation row, §11 (G-X2, G-A1) | The promise is now for supervised delegation. Children have native delegation and foreign MCP servers disabled, with a read-back or a route blocker. Roots are explicitly not limited, and the UI says so |
| Codex 4: agy L2 check/use gap | NARROW | §7 L2 post-start recheck, attestation, residual risk | agy has no launch mechanism that pins what it loads, so the window cannot be closed. It is stated plainly, then detected (recheck, second hello, census) and limited (mandatory attestation for agy) |
| Codex 5: peer test authenticates a process shape only | NARROW | §7 "What L3 and L4 do not do", census, second-hello signal | L3/L4 are defence in depth. A second hello from another pid revokes the generation. Attestation limits an impostor to calls the model made, with those exact arguments |
| Codex 6: single-use auth leaves IPC recovery undefined | FIX | §4 reconnect, §6 rebind, `WorkerSummary.delegation` | One reconnect is allowed: a sequential re-hello for Claude and Codex, always refused for agy. Otherwise `rebind_required`, then a fresh generation per provider at an idle point, with UI text |
| Codex 7: Claude overlay file holds the token | FIX | §5 overlay file rules, exposure matrix, G-C3 blocker | The file is created exclusively with `O_NOFOLLOW` and mode `0600` in a verified `0700` tmpfs directory, unlinked on exit and swept at startup. Provider persistence is a route-blocking proof gate that searches the whole home, the workspace, `$TMPDIR` and strace-written files |
| Codex 8: exposure needs per-provider treatment | FIX | §5 token exposure matrix, P1 negative tests, G-C3/G-X3/G-A3 | One matrix covers argv, files, process env, shell, other servers and provider logs. The `env_vars` wording is corrected |
| Codex 9: agy hook relocation must be an explicit blocker | FIX | §7 L2 items 1–2, §11 agy groups (`AgyHookProofFix`) | PR #44 (plugin hooks, dropped `--gemini_dir`/`--app_data_dir`, fresh proof per start) is a named blocker. L2 also refuses on the final argv and env |
| Codex 10: `provider_accepted` overstates identity | FIX | §2 Provenance definitions, §5 provenance rows, p1 identity tests | `provider_accepted` means "not refused". `effective` stays null for Codex and agy |
| Codex 11: no credential re-establishment protocol | FIX | §6 Revocation table and Rebind | Each event has a scope, the credentials it kills and its effect on runs. A restricted root keeps its token, and E3 narrows it live. Rebind is defined per provider |
| Codex 12: replay can hide an uncertain dispatch | FIX | §2 replay table, §8 item 5, p1 `StoredOperation`, `replay` | An `unknown` dispatch replays as `delivery_unknown` with the original receipt in `details`. The receipt and the dispatch state are stored separately |
| Codex 13: canonicalisation underspecified | FIX | §2 Canonical request and check order, p1 `normalised`, `Resolution` | Syntax is normalised and defaults made explicit. The resolution is persisted at acceptance and never recomputed. The scrubbed text compared is the text dispatched. Epoch takes precedence over conflict |
| Codex 14: inconsistent mutation receipts | FIX | §2 `MutationReceipt`, §3 all mutating outputs and `ack` | One flattened envelope: operation id, accepted epoch, `replayed`, `dispatch` and `warnings` |
| Codex 15: Claude `this_run` unproven | NARROW | §3 send, §5 `this_run` row, p1 `DelegationCaps.this_run`, `EchoQueue` | Claude `this_run` is `unsupported_delivery`. `auto` reports the run that consumed the line: FIFO plus text match now, the echo's `uuid` once G-C2 proves an input uuid is echoed (fixtures show provider-generated uuids) |
| Codex 16: unfenced cancel can stop newer work | FIX | §3 cancel "Bound at acceptance" | Runs are bound when the cancel is accepted and stored in `resolution`. A cancel never retargets |
| Codex 17: IPC transport not bounded | FIX | §1 stdio bound, §4 transport bounds, §9, p1 `FrameDecoder`, `ConnState` | Adds a pre-auth cap, body and write deadlines, a bounded outbound queue, single-frame decode with admission at decode, a 1 MiB stdio line bound, and duplicate-id and second-hello violations |
| Codex 18: cold bridge can stall startup | FIX | §1 startup, §4 client deadlines, p1 `WELCOME_DEADLINE_MS` | Connect, hello and welcome must complete within 2 s of start, else degraded mode; `initialize` is answered at once |
| Codex 19: `InheritedAuthority` lacks its own worker id | FIX | p1 `InheritedAuthority.worker`, `child` doc, lineage test | `child` sets parent = the current worker, root inherited, depth + 1 |
| Codex 20: Claude correlation disagrees | FIX | §4 Attestation (linking), p1 `ObservedCalls` (`matches_session` removed) | A call links only to a call observed in that worker's own generation (key, tool, arguments). Metadata can deny, never grant |
| Codex 21: pagination and ACK rules missing | FIX | §3 page rules, §8 item 6, p1 `page.rs`, `ack_eligible` | One `seq` order, cursors bound to their filter, combined byte accounting with the snapshot first, a one-entry progress rule. ACK requires prior presentation. `recipient_run_id` is dropped from ACK input |
| Codex 22: warnings, untagged state, stopping→stopped | FIX | §2 `Warning`, `StateRef`; §3 cancel completion; p1 lifecycle invariants | Warnings are in the envelope and the state domain is tagged. An interrupted turn with a live process goes to `waiting`; `stopped` only on an observed exit; cancel completes when the bound runs are terminal |
| AT-DEL-01: non-git ancestors missed | FIX | §7 L2 item 5, p1 `l2_directories` | Non-git: walk to `/`; discovery failure withholds. That agy searches above cwd in a non-git tree is unproven (agy.md D2), but the walk fails closed either way |
| AT-DEL-02: argv/env redirection not inspected | FIX | §7 L2 items 1–3, p1 `argv_redirect`, `env_redirect` | Checks the final argv (PR #44 classification), `CONFIG_REDIRECT_VARS` and any `CASCADE_*`, and `HOME` against the verified home |
| AT-DEL-03: false claim about shell commands; stdin injection | NARROW | §7 L4 correction, attestation, residual risk | The claim is withdrawn. Attestation is mandatory for agy (G-A5 blocker), so injected bridge calls the model did not make are refused. Same-UID code driving agents is stated as out of scope for every provider |
| AT-DEL-04: plugin scan not recursive | FIX | §7 L2 item 5, §9 L2 scan limits | A recursive scan of `.agents` for `mcp_config.json` or `hooks.json` at any depth: symlinks canonicalised, cycles detected, bounded, fail closed |
| AT-DEL-05: L3 vulnerable to an `exec` race | NARROW | §7 "What L3 and L4 do not do" | Same resolution as Codex 5: L2 is primary, and L3/L4 only detect |
| AT-DEL-06: `/proc/<pid>/stat` parsing | FIX | §7 L3, p1 `stat_ppid` and test | Parse after the last `)` |
| AT-DEL-07: allow rules not verified | FIX | §7 L2 item 4, p1 `ALLOW_RULES`, `allow_rules_missing` | A hookless agy launch withholds the token unless all 8 exact rules are present |

## Notes

- `ErrorCode::ALL` stays at 25 (24 codes plus `Unknown`), and no code was added. Unattested calls use `authority_denied`
  with `details.reason`. A rebind uses `runtime_unavailable`. A cursor filter mismatch uses `invalid_argument`.
- The P2 sanitiser is concrete. §10 and `Scope::stream_key` now name `redact::stream::StreamKey`, with no generic
  parameter.
- **New proof gates.** Route blockers:
  - G-C3, G-X3, G-A3: token persistence.
  - G-X1: Codex MCP isolation.
  - G-X2: Codex native delegation.
  - G-A1, widened: the hook denies other servers and the sub-agent tools.
  - G-A5: agy attestation.

  Advisory: G-C2 (Claude echoes an input uuid), G-C4 (overlay read once), G-C5 and G-X5 (attestation for Claude and
  Codex).
- **Residual risks that cannot be closed** (contract §7):
  - The L2 race window, because agy cannot pin the configuration it loads.
  - Same-UID code driving any agent.
  - User-installed global and home-plugin servers receiving the agy token.

## Revision 3 resolution table (second review round)

Inputs:
- the Codex re-review of revision 2 (R2-1..9, CHANGES-REQUIRED);
- the agy re-review of revision 2 (R2-1..5; R2-1, R2-2 and R2-3 were blockers; CHANGES-REQUIRED);
- the user decisions above.

| Finding | Resolution | Where | Rationale |
| --- | --- | --- | --- |
| Codex R2-1: attestation does not bind exact arguments (key, epoch, scrub-identical text) | FIX | §2 "Attestation arguments", §4 attestation and tests, p1 `attestation_arguments`, `AttestationArgs` | One helper, used for both sides, separate from the idempotency canonical form. It keeps every input field (key and epoch included), is not scrubbed, and keeps only a salted in-memory digest. Substitution tests cover the key, the epoch and scrub-identical text |
| Codex R2-2: agy-child bridge protections missing from the route groups; bridge-less children unrepresented | FIX | §11 route admission and agy child variants, §3 spawn, §6 status table, p1 `ChildBridge`, `agy_child_variant` | An agy child with a bridge needs every credential control (managed entry, L2, lineage, L3, census, L4, G-A5, G-A3). Without a bridge it is `bridge_disabled`, with `can_spawn: false`, and reports by its final result only |
| Codex R2-3: an immutable receipt cannot hold Claude's later-observed run | FIX | §2 frozen-fields rule, §3 send (`target_run_id`), §8 item 6, p1 `SendReceipt`, receipt test | The receipt is frozen at acceptance, and only `replayed` and `dispatch` refresh. The observed run and the final delivery state live on the message record |
| Codex R2-4: cancel of an idle target with active descendants | FIX | §3 cancel bound set, p1 `cancel_bound_set` and test | An empty bound set alone means `idle`. An idle target with active descendants is dispatched, with `run_id` null |
| Codex R2-5: `restricted` missing from wire/P1; recovery unclear | FIX | §2 `DelegationStatus`, §6 status table and rebind scope, §8 item 7, p1 `DelegationStatusCode` (6) | Store, wire and P1 share one 6-code set, with a reason and paths. Only `rebind_required` rebinds automatically. Restricted, bridge-disabled and closed workers never do |
| Codex R2-6: mode-dependent blocker not expressible by `route(parent, child)` | FIX | §11 route admission, §3 capabilities `modes[]`, §6 E4, p1 `admit_route(RouteQuery)`, `BlockerScope` | Admission is per (parent, child, task_mode, child bridge). G-C1 applies to writer modes only, so the Claude plan-child fallback is expressible |
| Codex R2-7: message/event boundary ambiguous in pages | FIX | §3 "Order and units", p1 `assemble` with `UnitKind` and test | Each `seq` is one indivisible unit with one representation: a message seq appears only in `messages`. `max_units` counts it once, and the cursor never skips a lower matching seq |
| Codex R2-8: output bounds unsubstantiated | FIX | §2 WorkerSummary bounds (≤48 KiB), §3 capabilities, list and page frame-fit rules, §9, p1 `fit_summary`, `assemble_list` | Every variable field is bounded. List pages stop at 512 KiB and capabilities at 512 KiB, with `models_truncated`. The snapshot is always included and at least one unit is returned. The worst case is shown to fit in 1 MiB |
| Codex R2-9: reconnect has no deadline | FIX | §4 reconnect, §9, p1 `RECONNECT_DEADLINE_MS` | A fresh 2 s deadline runs from the detected loss. On expiry: degraded, and `rebind_required` |
| agy R2-1 (BLOCKER): the census counts the app's own `--approval-hook` (same exe, direct child of agy) | FIX | §7 L3 peer and census, p1 `is_bridge_cmdline`, `census_ok` | The premise is confirmed (`hook_config.rs` `exec … --approval-hook`). Peer and census now also require the exact bridge argv `--cross-agent-mcp`; hook processes are ignored |
| agy R2-2 (BLOCKER): `read`, `list` and `wait` are unattested, so injected reads exfiltrate | FIX | §4 authority bullet, §7 L3/L4 text and residual risk, p1 `attestation_required` | For agy, all 8 tools need attestation, reads included. Legitimate calls are always `call_mcp_tool` steps, so nothing is lost. The claim is restated to match |
| agy R2-3 (BLOCKER): a root's native sub-agent starts a second bridge and trips the compromise path, killing the root | FIX | §1 lineage self-check, §5 `AGENT_TERMINAL_DELEGATION_APP_PID`, §6 root scope, §7 L4 hello rules, p1 `lineage_ok`, `hello_verdict` | Fail-closed, and legitimate roots survive. A bridge whose grandparent is not the app never says hello (mandatory for agy). A hello that fails L3 is refused with a notice and consumes nothing. Only a second **direct-child bridge** is a compromise. Whether agy sub-agents run as separate processes is unverified; both cases are covered |
| agy R2-4: canonicalisation for stream-step versus bridge args undefined | FIX | §2 "Attestation arguments", p1 `attestation_arguments` | Same as Codex R2-1: one helper, accepting an object or a JSON string, un-redacted, syntax-normalised, sorted keys. The shape of agy's `Arguments` stays part of G-A5 |
| agy R2-5: sidecars not in the L2 source names | FIX | §7 L2 item 5, p1 `WORKSPACE_SOURCE_NAMES` (7) | Adds `sidecars.json` and `sidecars`, plus `plugin.json`, `plugins.json` and `plugins.txt`, which agy also probed (`d2-config-probes.txt`) and whose contents are unverified. Fail closed |
| User decision 2 (Claude `this_run` best-effort) | FIX (supersedes the revision 2 NARROW of Codex 15) | §2 `ThisRunSupport`, `DeliveryState` (9), §3 send table, §5 `this_run` row, p1 `ThisRunSupport`, `DeliveryPlan::WriteLive`, `EchoMatch::Misdelivered` | Honest best-effort: nothing is promised at acceptance, and misdelivery is recorded as a delivery fact with the actual run. It is never reported as an error |
| User decision 3 (private `CODEX_HOME`, children only) | FIX | §5 child MCP isolation row, §11 | Allowed only as the G-X1 fallback. Its contents and lifetime are specified, and Settings discloses what the child loses |

Counts after revision 3:
- `ErrorCode::ALL` is 25 (unchanged).
- `DeliveryState::ALL` is 9 (adds `misdelivered`).
- `DelegationStatusCode::ALL` is 6.
- `WORKSPACE_SOURCE_NAMES` is 7.
- `STRIP_ON_SPAWN` is 5 (adds `ENV_APP_PID`).

## Revision 4 resolution table (third review round)

Round 3 verdicts:
- **agy: APPROVED**, with no blockers or shoulds, and two NICE items.
- **Codex:** one SHOULD left (R2-8, capability output bounds), plus two NICE items.

| Finding | Resolution | Where | Rationale |
| --- | --- | --- | --- |
| Codex R3 (R2-8 reopened): the capabilities bound is false. A maximal ASCII catalogue is 882,782 B, and 1,811,068 B as an MCP response | FIX | §3 "Output budget (every tool)", §3 capabilities truncation, §2 `ToolError.details` and `Warning` caps, §9, p1 `transport_cost`, `OUTPUT_BUDGET`, `truncate_capabilities`, worst-case regression test | The wrong claim is withdrawn. Every output is measured at the real transport boundary: `len(json)` + `len(escaped text block)` ≤ 768 KiB, with 256 KiB left for the JSON-RPC envelope; the IPC frame carries one copy. Capabilities truncation is deterministic: catalogue order is kept, models are dropped before profiles, the first profile and profile identity fields are never dropped, and `truncated` with `omitted: {profiles, models}` is reported. P1 builds the maximal catalogue, in ASCII and with six-fold escaping, and asserts the fit and the report |
| Codex R3: the same rule for other count-bounded responses | FIX | §3 page rules ("Output budget", "Fit"), §3 list bounds and `truncated`, receipts (≤8 warnings, details ≤4 KiB), p1 `assemble`, `assemble_list`, worst-case page, list and receipt tests | Pages and lists stop at the first unit that would exceed the budget, never skipping. The progress minimum costs at most 444 KiB, under the budget. Receipts and errors are bounded field by field. A still-oversized output becomes `invalid_argument` (`output_budget`) and is never sent |
| Codex NICE: p1-types:28 still says `route()` | FIX | p1 module table | Now `RouteQuery`, `admit_route()` |
| Codex NICE: the §6 native sub-agent lineage claim overstates Claude and Codex | FIX | §6 root scope | Qualified. The self-check is mandatory only for agy; Claude and Codex are log-only until P5. Their nested bridge may say hello under the root's principal and passes E2/E3 like the root itself. Attestation gates it once G-C5/G-X5 pass |
| agy NICE: rate-limit the nested-process notice | FIX | §7 L4 hello rules | One notice per peer pid per generation, with a count updated in place. At most one new notice per thread per 60 s, and logs are rate-limited with a suppressed-count line |
| agy NICE: `is_bridge_cmdline` without a trailing NUL | FIX | §7 L3 cmdline rule, p1 test | The trailing NUL is optional, and both forms are tested |

Totals for revision 4: **6 FIX, 0 NARROW, 0 REJECT.** The counts are unchanged from revision 3, except for the new
constants `OUTPUT_BUDGET`, `MAX_WARNINGS` and `MAX_ERROR_DETAILS_BYTES`.

## Revision 5 resolution table (fourth review round)

Codex confirmed that capabilities truncation now ends within the budget, and raised four SHOULD items in the same area.

| Finding | Resolution | Where | Rationale |
| --- | --- | --- | --- |
| Codex R4-1: unbounded JSON-RPC request id echoed in the response (a 900,000 B id plus 768 KiB of output is about 1.69 MB) | FIX | §1 "Request id bound", §9, p1 `MAX_REQUEST_ID_BYTES`, `request_id_ok` and boundary test | The encoded id is at most 256 B, checked before dispatch. Past that, the response is `-32600` with `id: null`, so the oversized id is never echoed. Tested at 256 and 257 B, as string, escaped and number ids |
| Codex R4-2: `delivery_unknown.details.receipt` (about 5 KiB) exceeds the 4 KiB `details` cap | FIX | §2 `ToolError.details`, §3 output budget, §9, p1 `MAX_RECEIPT_BYTES`, `MAX_ERROR_DETAILS_BYTES` and the replay test | The caps are sized from the worst case: the maximal spawn receipt is about 34 KiB with six-fold escaping. Receipts are capped at 48 KiB and `details` at 64 KiB, so a wrapped maximal receipt always fits. The test replays it and checks the fit and a byte-exact round trip |
| Codex R4-3: page assembly gets only JSON lengths but must account for transport cost, the envelope and the ack | FIX | §3 "Exact accounting" and "Minimum complete page", p1 `Cost`, `assemble`, `assemble_list` and tests | Escaping is per character, so both dimensions add up exactly. The assembler measures the complete envelope (snapshot, ack, maximal cursor, flags) and each unit in both dimensions, then cross-checks the final serialisation. The minimum page is at most 214 KiB of JSON and 642 KiB of cost, under the budget, so a valid page always advances and never becomes an error |
| Codex R4-4: the final guard used a caller-error code | FIX | §3 output budget, error table, p1 `ErrorCode` (26), `guard_output` and test | A new `internal_error` (not retryable). The guard changes only the response, never the operation's identity, stored receipt, state or outcome. An accepted mutation is still replayed by a same-key call. `internal_error` is never stored |

Totals for revision 5: **4 FIX, 0 NARROW, 0 REJECT.** Counts:
- `ErrorCode::ALL` is **26** (25 codes plus `Unknown`). This supersedes the "25" in the earlier tables.
- `DeliveryState::ALL` is 9 and `DelegationStatusCode::ALL` is 6 (unchanged).

## Revision 6 resolution table (fifth review round)

Codex confirmed the receipt arithmetic (34,111 B, 34,184 B wrapped), the request-id bound and `internal_error`. One
SHOULD remained.

| Finding | Resolution | Where | Rationale |
| --- | --- | --- | --- |
| Codex R5-1: page accounting is not exact. Quotes are counted per piece (`Cost([1]) = 8`, not 10), separators are described differently in the contract and P1, and the maximal envelope cannot equal the final value | FIX (supersedes the revision 5 "exact accounting" row) | §3 "Selection, then measurement", "Minimum complete page" (642 KiB + 2 B), list bounds; p1 `Reservation`, `reserve`, `settle` (`Cost` removed) and the conservative-stop and correction tests | Every exactness claim is removed. Selection uses a reservation that is labelled an upper bound: the maximal envelope plus, per unit, `len(json)` + `len(escaped)` + 4. The real final page, in both copies, is then measured. Over the budget, the last unit is dropped and the page re-measured; the first unit is always kept. `internal_error` only if a one-unit page fails, which the minimum-page bound rules out. The contract and P1 now describe one mechanism |

Totals for revision 6: **1 FIX.** Counts unchanged: `ErrorCode::ALL` 26, `DeliveryState::ALL` 9,
`DelegationStatusCode::ALL` 6.
