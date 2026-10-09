# Agent Terminal: In-App Auto-Approvals & Memory State Investigation

**Date**: 2026-10-09  
**Status**: Implemented & Verified (Commit `cbb410f7`)  
**Components**: `agent-terminal` (`src/always_allow.rs`, `src/approval_server.rs`, `src/chat/`, `src/window/imp/`)

---

## 1. Executive Summary

This document captures the forensic findings and technical implementation details for two critical areas in `agent-terminal`:
1. **Memory Spike & Process Group Investigation**: Root-cause analysis of memory accumulation during long-running agent streaming turns, and why clicking "Stop" immediately clears physical memory.
2. **In-App Auto-Approval & Pattern Permission Management**: Complete architecture, data models, UI flows, and security constraints for handling agent approvals natively within `agent-terminal` with wildcards (`*`, `?`), cross-agent rule importing, and an interactive loosening dialogue.

---

## 2. Forensic Findings: Memory Leak & Process Tree State

### 2.1 The Issue Observed
- `agent-terminal` was observed consuming over 2 GB of RAM during heavy coding sessions.
- Clicking the **"Stop"** button caused memory usage to immediately plummet back to baseline.
- Restarting an agent turn quickly caused memory usage to jump back up.

### 2.2 Root-Cause Analysis
1. **Cgroup Scope Aggregation**:
   - `agent-terminal` runs inside a systemd user scope (e.g. `app-gnome-ca.nuvek.AgentTerminal-<pid>.scope`).
   - System monitors (such as GNOME System Monitor or `systemd-cgtop`) measure total physical RSS for the **entire cgroup**, which includes:
     - The GTK4 application itself (`agent-terminal`)
     - Child agent processes (`agy`, `claude`, `codex`)
     - Subagents spawned by the primary agent
     - Heavy compiler/tool child processes invoked by agents (`cargo check`, `rustc`, `node`, `esbuild`)
2. **Process Group Termination on "Stop"**:
   - When the user clicks "Stop", `AgentProcess::interrupt()` executes:
     ```rust
     libc::kill(-pid, SIGINT);
     ```
     Targeting the negative process group ID (`-pid`) sends `SIGINT` to the child agent and all its subprocesses.
   - When child compiler or runtime processes terminate, the Linux kernel immediately frees their physical memory pages, causing the dramatic drop in observed cgroup RSS.
3. **GTK Main-Loop Starvation & SQLite Synchronous Writes**:
   - While streaming deltas arrive rapidly over stdout, synchronous SQLite writes in `TranscriptStore` can contend with the main UI thread.
   - Retaining fully instantiated `ChatView` widgets with large DOM-like widget subtrees for all open tabs keeps significant memory resident in the GTK display server.

### 2.3 Follow-Up Remediation Plan (Next Phase)
- **Inactive Tab Eviction**: Evict `chat.view` (`chat.view = None`) when switching tabs if `!chat.running`, re-instantiating only when selected.
- **Transcript History Delta Compaction**: Coalesce streaming deltas before writing to SQLite and compact history chunks in memory.

---

## 3. In-App Auto-Approval & Pattern Permission Engine

### 3.1 The Approval Hang Problem
- The Antigravity approval hook (`~/.gemini/config/hooks.json`) redirects all tool invocations through `agent-terminal --approval-hook`.
- Previously, `approval_server.rs` strictly rejected build runners (`cargo`, `make`, `npm`, `rustc`) from ever being remembered or auto-approved due to concerns over model-editable files.
- Without pattern matching or cross-agent importing, every single command blocked on the hook Unix socket, causing timeouts, frozen agents, and severe operator fatigue.

### 3.2 Implemented Solution Architecture

#### A. Core Engine (`src/always_allow.rs`)
- **Data Model**:
  ```rust
  pub enum PatternKind {
      Exact,
      Prefix,
      Wildcard,
  }

  pub struct Rule {
      pub workspace: String,
      pub tool: String,
      pub detail: String,
      pub kind: PatternKind,
  }
  ```
- **Wildcard Matcher**: O(N*M) linear matching for glob wildcards (`*` matches any sequence of characters, `?` matches any single character).
- **Workspace Scoping**: Rules support exact workspace canonical paths or global wildcards (`workspace = "*"`).
- **Import Parsers**:
  - `permissions.toml`: imports `shell`, `shell_glob`, `shell_exact`, and `mcp` arrays.
  - Claude `settings.json`: imports `Bash(cmd:*)` and `mcp__server__tool`.
  - Antigravity `settings.json`: imports `command(cmd)`.

#### B. Approval Server Integration (`src/approval_server.rs`)
- **Fast-Path Decision**: In `ApprovalServer::serve`, incoming tool calls (`run_command`, `call_mcp_tool`) are evaluated against `AlwaysRules` before modal rendering. Matching commands reply with `Decision::Allow` immediately.
- **Shell Injection Filtering**: Commands containing shell chaining characters (`;`, `&`, `|`, `` ` ``, `$`, `>`, `<`) are rejected from being saved with `AllowAlways`.
- **Custom Rule Persistence**: `ApprovalServer::respond_with_rule` accepts an optional `Rule` parameter, allowing modified patterns from the UI to be persisted directly.

#### C. Interactive Pattern Modification Dialogue (`src/chat/view/cards.rs`)
- Clicking "Always allow" opens an `adw::AlertDialog`:
  - **Editable Entry**: Pre-filled with the command line or MCP tool path.
  - **Quick Preset Chips**: Buttons for `first_word *` and `prefix *` for rapid argument loosening.
  - **Workspace Toggle**: "Apply across all workspaces" check button.
  - **Risk Disclaimer**: Highlights that broad patterns permit unreviewed agent execution.
  - **Actions**: "Save Rule & Allow", "Allow Once", "Cancel".

#### D. Settings & Preferences UI (`src/window/imp/agents_prefs.rs`)
- Added **"In-App Approvals & Pattern Matching"** preference group:
  - `pattern_auto_approval` switch.
  - `confirm_rule_modification` switch.
  - **"Import Agent Permissions"**: Scans existing agent configs with immediate toast feedback.
  - **"Saved Permission Rules"**: Dialogue listing active rules with deletion buttons.

#### E. First-Run Setup Wizard (`src/window/imp/setup.rs`)
- Added auto-approval onboarding rows to `setup_agents_page`.
- Enabled by default for seamless workflow, with clear Canadian English copy explaining how it works and warning of broad wildcard risks.

---

## 4. Verification & Quality Gates

| Verification Step | Command | Result |
|---|---|---|
| Cargo Unit & Integration Tests | `cargo test -p agent-terminal` | 398 passed, 0 failed, 11 ignored |
| Bundled Icons Audit | `cargo test -p agent-terminal --test icons` | 6 passed, 0 failed |
| Clippy Linter | `cargo clippy -p agent-terminal` | 0 warnings, clean |
| Formatting Standard | `rustfmt --edition 2024` | Fully compliant |
| Release Build | `cargo build --release -p agent-terminal` | Clean build in 57.51s |
| Git Commit | `cbb410f7` | Conventional Commit, no AI attribution |

---

## 5. File Inventory

- [`src/always_allow.rs`](file:///home/jdesroches/git/agent-terminal/src/always_allow.rs): Pattern engine, linear wildcard matcher, agent permission importers.
- [`src/approval_server.rs`](file:///home/jdesroches/git/agent-terminal/src/approval_server.rs): Zero-latency hook evaluation, custom rule persistence, shell injection filtering.
- [`src/chat/mod.rs`](file:///home/jdesroches/git/agent-terminal/src/chat/mod.rs): `respond_approval_with_rule` in `ChatBackend` trait.
- [`src/chat/session.rs`](file:///home/jdesroches/git/agent-terminal/src/chat/session.rs): Codex pattern matching and custom rule handling.
- [`src/chat/view.rs`](file:///home/jdesroches/git/agent-terminal/src/chat/view.rs): `RowEvent::Approve` custom rule forwarding.
- [`src/chat/view/cards.rs`](file:///home/jdesroches/git/agent-terminal/src/chat/view/cards.rs): Interactive pattern modification dialogue with preset chips.
- [`src/chat/view/interruption.rs`](file:///home/jdesroches/git/agent-terminal/src/chat/view/interruption.rs): Shortcut handling for approval events.
- [`src/config.rs`](file:///home/jdesroches/git/agent-terminal/src/config.rs): `pattern_auto_approval` and `confirm_rule_modification` configuration flags.
- [`src/window/imp/agents_prefs.rs`](file:///home/jdesroches/git/agent-terminal/src/window/imp/agents_prefs.rs): Preferences group, import button, rules management dialogue.
- [`src/window/imp/setup.rs`](file:///home/jdesroches/git/agent-terminal/src/window/imp/setup.rs): Setup wizard onboarding group with risk disclosure.
- [`src/window/imp/threads.rs`](file:///home/jdesroches/git/agent-terminal/src/window/imp/threads.rs): `ThreadTabs` approval forwarding.
