# Plan: Agent Terminal 3.0 UX Remediation (Approvals, Navigation, Diffs & Accessibility)

**Status:** APPROVED (Reviewed by `plan-reviewer`, ready for execution)  
**Date:** 2026-10-07  
**Branch:** `feat/v3-chat-first`  
**Owner:** Argus / Antigravity  
**Hive Topic:** `#20: agent-terminal-ux-council` (Findings #209–#214)

---

## 1. Objectives & Scope

Remediate critical usability, attention, navigation, and accessibility defects identified in the UX Review and confirmed by the Council Deliberation and Plan Reviewer:
1. **P0 (Blocker) - Interruption Awareness**: Pending approvals and questions scroll off-screen and lack persistent indicators on active threads.
2. **P0 (Blocker) - Navigation Desynchronisation**: Open tabs missing from SQLite summaries cause `Alt+1..9` tab switching to desynchronise the active `TabView` page from the sidebar row selection.
3. **P1 (High) - Diff Panel Layout Protection**: Toggling the diff panel (520px width) squashes the chat view to 0px on standard window widths.
4. **P1 (High) - WCAG 2.1 AA Contrast**: Secondary text opacity (2.8:1 contrast) and folder labels fail WCAG AA.
5. **P1 (High) - Session-Wide Diffs in Workspace / Non-Git Directories**: Launching threads in `~` or workspace parents disables diffs completely ("Not a git repository"), even when agents edit files in git repos.

---

## 2. Slice Specifications & Implementation Tasks

### Slice 1: Sticky Interruption Shelf (P0)
**Goal**: Ensure an operator never misses a pending approval or question, even if scrolled up or if output pushes cards off-screen.

- **Files to create/modify**:
  - `src/chat/view/interruption.rs` (new module):
    - `InterruptionShelf`: A container wrapped in a `gtk4::Revealer` (slide-down transition), docked in `ChatView`'s bottom clamp directly above `Composer`.
    - Contents:
      - Icon: `at-security-medium-symbolic` (amber/shield for approvals) or `at-dialog-question-symbolic` (purple/question).
      - Title & Command preview: e.g. `Approval required: run_command` / `$ cargo clean`.
      - Actions:
        - `[Deny]` button (destructive style).
        - `[Allow]` button (suggested action style).
        - `[Jump to Card]` icon button (triggers `transcript.scroll_to_card(id)`).
    - Lifecycle:
      - Automatically reveals when `chat.approval == true` or an unhandled question card is present.
      - Automatically collapses when resolved via either the shelf or in-stream card.
  - `src/chat/view.rs`:
    - Integrate `InterruptionShelf` into `Inner`:
      ```rust
      let column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
      column.append(&inner.plan.revealer);
      column.append(&inner.interruption.revealer);
      column.append(inner.composer.widget());
      ```
    - In `refresh_status()` / row event updates, feed the pending query to `interruption.update()`.
  - `src/chat/view/composer.rs`:
    - Safety Latch: When an approval is active, update placeholder text to *"Awaiting approval above..."*.
- **Verification**:
  - Unit tests in `src/chat/view.rs` proving the shelf reveals when an approval envelope is added and hides on `mark_approval_sent`.
  - Live GUI check via the preview MCP's `preview_app` driving Broadway with Playwright to verify visual reveal and button responsiveness.

---

### Slice 2: TabView & Sidebar Selection Synchronization (P0)
**Goal**: Guarantee `Alt+1..9` keyboard switching and sidebar row selection are always 1:1 synchronized by ensuring open tabs are never missing from sidebar models.

- **Files to modify**:
  - `src/window/imp/threads.rs`:
    - Fix `sidebar_rows()`: If a tab exists in `imp.tabs` whose `thread_id` is not yet in `imp.summaries` (e.g. freshly created or unstored thread), synthesize a `SidebarRow` from the tab's `chat` state so `selected_row_key()` can always find the active row in `sidebar.keys`.
    - In `wire_thread_pages`, verify `refresh_sidebar()` selects the synthesized row and scrolls the sidebar list if needed.
- **Verification**:
  - Unit tests in `src/window/sidebar_model.rs` and `threads.rs` proving all open tabs are present in `sidebar_rows()` even with empty SQLite summaries.

---

### Slice 3: Diff Panel Minimum Width & Layout Protection (P1)
**Goal**: Prevent toggling the diff panel from collapsing the chat view to 0px width on standard displays.

- **Files to modify**:
  - `src/window/imp.rs`:
    - In `setup_shell` / `wire_diff_panel`:
      - Set `paned.set_shrink_start_child(false)` so GTK refuses to shrink chat below its natural minimum.
      - Apply `stack.set_size_request(440, -1)` directly on the paned start child stack widget during paned construction (`src/window/imp.rs:2044`), avoiding unwraps.
      - In `toggle_diff_panel()`: Clamp `diff_panel_width` so that available window width minus sidebar preserves at least 440px for chat.
- **Verification**:
  - Automated test verifying paned position clamps and never squashes start child below 440px.

---

### Slice 4: WCAG 2.1 AA Contrast Tokens (P1)
**Goal**: Meet WCAG 2.1 AA contrast requirements (>= 4.5:1 for normal text).

- **Files to modify**:
  - `src/main.rs`:
    - Update `.folder-label`: change color from `#8a84b8` to `#a8a2dc` (contrast 5.5:1 against `#141120`, passing AA).
    - Update `.dim-label`: change from `opacity: 0.55` to explicit high-contrast token `#9c97c7` (contrast 5.2:1 against `#181425`, passing AA).
    - Update `.thread-badge.badge-approval`: ensure contrast of badge mark against row background.
- **Verification**:
  - Run contrast calculations confirming all modified text styles exceed 4.5:1 against `#181425` and `#141120`.

---

### Slice 5: Workspace & Non-Git Session Diff Fallback (P1)
**Goal**: Allow inspecting diffs across touched repositories even when a thread starts in `~` or a parent workspace.

- **Files to modify**:
  - `crates/agent-kit/src/diff.rs` & `src/window/diff_panel.rs`:
    - When `tab.dir` is not a git repository root:
      - Asynchronously query modified paths via `store_job` (preserving UI-store boundaries).
      - Locate enclosing git repository roots for touched files.
      - If repositories are detected, aggregate modified files into a repository tree in `DiffPanel` instead of showing a hard error page.
- **Verification**:
  - Test running a turn in `~` touching a repo file, opening Diff Panel, and verifying that changes are listed cleanly.

---

## 3. Rollback & Safety Guardrails
- All modifications are pure Rust & GTK within `src/` and `crates/`.
- No live infra mutations, external dependencies, or network calls introduced.
- Continuous gate verification via `~/scripts/rust-gate.sh -p agent-terminal`.
- Undo: `git checkout feat/v3-chat-first` / standard git revert.
