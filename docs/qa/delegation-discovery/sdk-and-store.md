# P0 discovery: D8 MCP SDK choice and the store backup/upgrade-lock proof

Scout-Infra, 2026-10-14, branch `feat/cross-agent-runtime`. All evidence was produced by the
scratch project `target/delegation/scratch/` (git-ignored, its own `[workspace]`, never a member of
the app workspace). Every database used lives under `target/delegation/scratch/run/`. The live
store (`$XDG_STATE_HOME/agent-terminal/threads.db`) was never opened.

Toolchain: rustc 1.98.1 (local stable). Bundled SQLite 3.53.2 (rusqlite 0.40.2 /
libsqlite3-sys 0.38.2, the versions in `Cargo.lock`).

| Scratch crate | Purpose |
| --- | --- |
| `mcp-rmcp` | Minimal rmcp stdio server, one tool, `structuredContent` + `isError` |
| `mcp-hand` | Hand-rolled JSON-RPC stdio server on `serde_json` only |
| `mcp-baseline` | `serde_json` stdin echo, for binary-size deltas |
| `store-proof` | Backup + upgrade-lock sequence (T1–T5) and the `durable_call` pragma switch (T6) |
| `exchange.jsonl`, `exchange-2026.jsonl` | Hand-written JSON-RPC requests fed on stdin |

Rerun: `cargo build --release --manifest-path target/delegation/scratch/Cargo.toml`, then
`target/delegation/scratch/target/release/store-proof` (prints `ALL PASS`), and
`target/delegation/scratch/target/release/mcp-rmcp < target/delegation/scratch/exchange.jsonl`.

---

## A. D8: MCP SDK

### Recommendation

**Use `rmcp = { version = "=3.5.1", default-features = false, features = ["server", "transport-io"] }`**
with `tokio = { version = "1", default-features = false, features = ["rt", "time", "io-std"] }`, on a
`current_thread` runtime built only inside the headless `--cross-agent-mcp` entry point. Pin the
minor (`~3.5`, or `=3.5.1` while the contract is frozen); the protocol moves fast.

Why rmcp over hand-rolled: the spec changed its lifecycle in 2026-07-28 (SEP-2567: no
`initialize`; protocol version, client info and capabilities travel in per-request `_meta`;
`server/discover`; MRTR `resultType`). rmcp 3.5.1 already serves **both** lifecycles from the same
handler (proven below). A hand-rolled server would have to track that and every later revision
ourselves. The cost (about 15 new crates, about 1.4 MB of binary) is all permissive-licensed and
passes the repo's `deny.toml` as is.

### Facts (verified)

| Item | Result |
| --- | --- |
| Current release | `rmcp` 3.5.1 (crates.io, 2026-10-14), Apache-2.0, repo `modelcontextprotocol/rust-sdk` |
| MSRV | rmcp `rust-version = 1.88`; repo is `1.89`. Highest in the tree: `uuid` 1.28.0 at **exactly 1.89.0**, so a future uuid bump could exceed the floor: pin it in the lockfile and keep CI at 1.89. tokio 1.71, schemars 1.74, syn 3 1.71, chrono 1.62. Not compiled on a 1.89 toolchain (none installed); declared fields only. |
| Protocol versions | `KNOWN_VERSIONS` 2024-11-05 … 2026-07-28; `LATEST_WITH_INITIALIZE` = 2025-11-25 |
| Minimal tokio | `rt` + `time` + `io-std`, `current_thread`. **`enable_time()` is mandatory**: without it rmcp panics at `service.rs:1771` ("timers are disabled") after the first reply. No I/O driver needed (`io-std` uses the blocking pool). rmcp itself turns on `tokio-macros`. |
| `server` feature | Pulls `schemars` (+ derive) unconditionally, even with `macros` off |
| Normal deps | 50 crates for the server (excluding the bin). New to the app's `Cargo.lock`: rmcp, tokio, tokio-util, tokio-macros, futures, schemars, schemars_derive, serde_derive_internals, ref-cast(+impl), dyn-clone, chrono, num-traits, uuid, pastey (≈15). The rest (futures-util, bytes, syn 2/3, thiserror, indexmap, hashbrown 0.17, getrandom 0.4, zmij, slab, tracing-attributes) are already locked. |
| Licences | MIT, Apache-2.0, MIT OR Apache-2.0, `Unlicense OR MIT` (memchr), Unicode-3.0. `cargo deny --workspace check -s` with the repo's `deny.toml`: **advisories ok, bans ok (1 warning: syn 2 + 3 duplicate, already the case in the app), licences ok, sources ok**. |
| Binary size (release, lto, strip, cgu=1) | baseline 378,968 B; hand-rolled 410,576 B (**+32 KB**); rmcp 1,853,776 B (**+1.42 MB**) |
| Input validation | rmcp's stdio path does **not** validate arguments against `inputSchema` and does not reject unknown tool names (`get_tool` is only consulted by the streamable-HTTP server). Exchange ids 5 and 6 below. The handler must validate name and arguments itself and return `-32602` / `isError`. |

### Wire proof (rmcp, initialize-era client, `exchange.jsonl`)

```
→ initialize {protocolVersion:"2025-11-25"}
← {"id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"rmcp","version":"3.5.1"}}}
→ notifications/initialized            (no reply, correct)
→ tools/list
← {"id":2,"result":{"tools":[{"name":"echo",…,"inputSchema":{…},"outputSchema":{…}}]}}
→ tools/call echo {"text":"hi"}
← {"id":3,"result":{"content":[{"type":"text","text":"{\"echo\":\"hi\",\"len\":2}"}],"structuredContent":{"echo":"hi","len":2},"isError":false}}
→ tools/call echo {"text":"no","fail":true}
← {"id":4,"result":{"content":[…],"structuredContent":{"code":"refused","message":"refused: no"},"isError":true}}
→ tools/call nope {}          ← id 5 reached the handler (scratch handler did not check the name)
→ tools/call echo {"text":5}  ← id 6 reached the handler (no schema validation by rmcp)
```

Note `serverInfo` defaults to `rmcp`/`3.5.1` (`Implementation::from_build_env()` picks up rmcp's
own build env): set it explicitly with `.with_server_info(Implementation::new("agent-terminal", VERSION))`.

### Wire proof (rmcp, 2026-07-28 stateless client, `exchange-2026.jsonl`)

```
→ server/discover {_meta:{io.modelcontextprotocol/protocolVersion:"2026-07-28", clientInfo, clientCapabilities}}
← {"id":1,"result":{"resultType":"complete","supportedVersions":["2024-11-05",…,"2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private",…}}
→ tools/call echo (no initialize, _meta on the request)
← {"id":2,"result":{"resultType":"complete",…,"structuredContent":{"echo":"stateless","len":9},"isError":false}}
```

### Liftable server skeleton (no macros)

```rust
use rmcp::{model::*, service::{RequestContext, RoleServer}, ErrorData as McpError, ServerHandler, ServiceExt};

impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agent-terminal", env!("CARGO_PKG_VERSION")))
    }
    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>)
        -> Result<ListToolsResult, McpError> { Ok(ListToolsResult::with_all_items(tools())) }
    async fn call_tool(&self, req: CallToolRequestParams, _: RequestContext<RoleServer>)
        -> Result<CallToolResponse, McpError> {
        // validate req.name and req.arguments here: rmcp does not
        Ok(CallToolResult::structured(json!({ … })).into())          // success
        // Ok(CallToolResult::structured_error(json!({ … })).into())  // isError: true
    }
}

fn run_bridge() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build()?;
    rt.block_on(async { Bridge.serve(rmcp::transport::stdio()).await?.waiting().await?; Ok(()) })
}
```

`Tool::new(name, description, Arc<JsonObject>)`, then set `tool.output_schema = Some(Arc::new(…))`.
`CallToolResult::structured*` writes both `structuredContent` and the serialised text block.

Tokio confinement: the runtime is constructed only in `run_bridge()`, reached from the
`--cross-agent-mcp` argument before any GTK/`adw::Application` setup. Tokio is linked into the one
shipped binary (standalone philosophy), but the GTK process never starts a runtime. If even linking
is unwanted, the bridge can be a second `[[bin]]` in a GTK-free crate; that changes packaging
(three asset lists), so it is a P1 decision, not required by D8.

### Alternative: hand-rolled (`mcp-hand`, 109 LOC, works for the same exchange)

Pros: +32 KB, no new crates, no tokio. Cons and spec risk:

- It implements only the initialize-era lifecycle. Covering 2026-07-28 adds `server/discover`,
  per-request `_meta` parsing and validation, `resultType`, version negotiation for both modes:
  realistic total **400–600 LOC + tests**, plus cancellation (`notifications/cancelled`), `ping`,
  progress, pagination cursors and error-code conformance.
- Every spec revision (two lifecycle-relevant revisions in the last 11 months) is ours to track and
  re-test against three clients (Claude, Codex, agy).
- Only worth it if the dependency budget is a hard constraint. Then freeze one protocol version
  (2025-11-25) and refuse others explicitly. Not recommended.

---

## B. SQLite backup + upgrade lock: **PROVEN** (with three amendments)

rusqlite **needs the `backup` feature**: in 0.40.2 the module is `#[cfg(feature = "backup")] pub mod
backup;`. Use `features = ["bundled", "backup"]` in `crates/agent-kit/Cargo.toml` (P3 change; not
made here).

### Proven sequence

1. **Upgrade lock.** Open `<db>.upgrade-lock` (`0600`, create, no truncate) and take
   `std::fs::File::lock()` (exclusive `flock`, stable since 1.89, the repo's MSRV). Blocking.
2. **Open without migrating.** `Connection::open_with_flags(db, READ_WRITE | NO_MUTEX)` (no
   `CREATE`), `busy_timeout`. Read the version **without writing**: check `sqlite_master` for
   `schema_version`; do not run today's `CREATE TABLE IF NOT EXISTS schema_version`. If 4: return
   the recorded backup and stop.
3. `PRAGMA foreign_keys = OFF` (outside any txn, as `migrate()` does), then **`BEGIN IMMEDIATE`**
   and **recheck** the version under the reservation (4: AlreadyCurrent; not 3: error).
4. **Backup from a SEPARATE read-only connection** (`READ_ONLY | NO_MUTEX`) into
   `backups/.tmp-<pid>-<nanos>.db`, pre-created `create_new` + `0600`, inside a `0700` directory
   whose mode and uid are checked. On the source: `BEGIN`, read the expected counts, `Backup::new(&src,
   &mut dst)`, **one `step(-1)` that must return `Done`**, `COMMIT`. Set the destination to
   `PRAGMA journal_mode=DELETE` (the WAL source's header otherwise makes the backup a WAL database),
   close it, `fsync` it.
5. **Verify** on a fresh read-only connection: `integrity_check == ["ok"]`, schema version 3, counts
   equal to the pinned source snapshot, file mode `0600`. Any failure deletes the temp file.
6. **Publish no-clobber**: `fs::hard_link(tmp, backups/threads-v3-<secs>-<pid>-<nanos>.db)`
   (`EEXIST` instead of overwrite), remove the temp name, `fsync` the directory.
7. **Migrate in the same reserved transaction** (no release/reacquire gap), record the published path
   in `meta('pre_v4_backup')` in that transaction, `foreign_key_check`, `COMMIT`. On any error,
   `ROLLBACK`. Then `foreign_keys = ON`, close, drop the lock.

### Test results (`store-proof`, all PASS, run twice)

| Test | Result |
| --- | --- |
| T1 WAL | External writer left 200 uncheckpointed frames (20,632-byte WAL, still open). Backup under RESERVED contains them (700 = 500 + 200 events), is `journal_mode=delete`, schema v3, `0600` in `0700`; live DB then v4, integrity ok. |
| T2 exclusion | During the reservation a second connection's `INSERT` gets `SQLITE_BUSY`; a reader still reads the unmodified v3 data. A prior owner holding a write transaction makes the upgrade refuse (`begin immediate: database is locked`), schema stays v3, nothing published. |
| T3 concurrency | Two openers released together: one `Migrated`, the other blocked (~830 ms behind an 800 ms hold) then `AlreadyCurrent` with the **same** backup path; exactly one backup, no temp files, still the v3 preimage. A restart through the same path: AlreadyCurrent, no new backup. |
| T4 failures | Injected after reserve, after backup (pre-verify), corrupt backup (truncated: `integrity_check` fails "malformed"), after verify (pre-publish), after publish+migrate (pre-commit): each leaves v3, the original counts and integrity ok, and no temp files. Each retry through the real path migrates. Only the pre-commit case leaves one published, valid v3 backup (orphan, never clobbered). |
| T5 (negative) | Backup **from the connection that holds `BEGIN IMMEDIATE`** returns `Busy` forever (with and without uncommitted writes). rusqlite's `run_to_completion` loops on `Busy` with no limit, so it **would hang**. This confirms the plan's ban; P3 must use `step(-1)` and treat anything but `Done` as failure. `VACUUM INTO` from a read-only connection also works during the reservation (no `backup` feature needed) but creates the file at umask mode (`644`). If used, it needs the `0700` dir and a `chmod`, and has no create_new guarantee. The backup API is preferred. |
| T6 durable_call | Same connection: NORMAL(1) → `PRAGMA synchronous=FULL` → reads 2 → `BEGIN IMMEDIATE; …; COMMIT` → NORMAL → reads 1. **Inside an open transaction the change is refused** (`"Safety level may not be changed inside a transaction"`, value stays 1). The setting is per-connection: another connection is unaffected. |

### Required changes to plan section 5

1. **Old app instances are not excluded by SQLite.** `BEGIN IMMEDIATE` only waits for a writer that
   is *inside* a transaction (T2b). An idle pre-upgrade `StoreWriter` between batches does not hold
   a lock, so the migration proceeds under it and its open connection keeps writing afterwards.
   That is safe only because v4 is additive. Keep "excluded by normal application ownership", and
   state the fact: the GApplication single instance is the real exclusion, and v4 must stay
   strictly additive so a still-running v3 writer cannot corrupt it.
2. **The preflight must not write before the lock.** Today `Store::open` creates the file, switches
   to WAL, and `migrate()` runs `CREATE TABLE IF NOT EXISTS schema_version` before `BEGIN IMMEDIATE`.
   The preflight opens with `READ_WRITE` (no `CREATE`) and reads `sqlite_master`. A missing DB file
   is "fresh install, no backup needed", which goes to the existing `Store::open`.
3. **Make two things explicit.** (a) Use bounded `Backup::step(-1)` and require `Done`; never
   `run_to_completion` (infinite on `Busy`/`Locked`). (b) The backup destination must be switched to
   `journal_mode=DELETE` before verify. Also record the published backup path inside the migration
   transaction (`meta`), so a concurrent opener reuses it. A failure after publish but before commit
   leaves a valid orphan backup; retain it, don't delete it.

`durable_call` as written is consistent with T6: drain/commit the batch, then set FULL, run the
control transaction, set NORMAL and read the value back. A pragma error at either switch must refuse
dispatch: the switch cannot be made inside the writer's open batch transaction.

Caveats: T3 ran both openers as threads in one process. `flock` locks are per open file
description, so the two separate `open()` calls conflict exactly as two processes would. A
cross-process repeat is a cheap P3 test. The backup is a byte-level page copy: it includes any
secrets already in the store and stays private (`0600`/`0700`), and it is never logged.
