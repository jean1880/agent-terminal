# Plan: turn checkpoints, diff panel, worktree tabs — REVIEWED 2026-09-30

Goal: borrow the three T3 Code capabilities that do not need a protocol adapter,
while keeping the terminal a PTY host that never parses the CLI:

1. **Turn checkpoints**: snapshot the tab's working tree into a hidden git ref
   whenever a turn ends, so every turn can be diffed and restored.
2. **Diff panel**: a read-only, per-tab side panel showing what changed:
   uncommitted, since the last turn, or since the tab opened.
3. **New Tab in Worktree…**: open a tab on a fresh `git worktree` + branch, so
   two agents can work on one repo in parallel without trampling each other.

Out of scope: remote/web/mobile access (violates *Localhost bound*), structured
approvals, and parsing CLI output beyond what `handoff.rs` already reads.

## Decisions (2026-09-30)

- D1: Checkpointing is **on by default** (`checkpoint.enabled: true`).
- D2: The default worktree location is the **hidden sibling
  `~/git/.<repo>.worktrees/<branch>`**, i.e. `<toplevel>/../.<repo>.worktrees/`.
  The leading dot keeps repo-index from indexing it as a project (F11).
  `worktree_root` overrides it.
- D3: **WP4 restore is in**, as the last WP after WP1–WP3 soak, carrying the
  review's data-loss fixes.

## Progress (update as work lands — resume from the first unchecked item)

Branch: `feat/checkpoints` (off `master` @ b1a7ceb0)

- [ ] WP0.1 bell per CLI (F4/F5): **needs the user** (a live turn in the GUI).
      The bell is not logged at info, so the journal cannot answer it. WP1
      adds an info-level bell log so this becomes checkable. It only gates
      the trigger choice, which has fallbacks.
- [ ] WP0.2 quiet-screen behaviour per CLI: **needs the user**, same session as WP0.1
- [x] WP0.3 `git restore` deletion semantics (F10): see F10
- [x] WP0.4 CI has git (F6): see F6
- [x] WP1 `git.rs` runner + repo discovery (`src/git.rs`: `git_raw`/`git_output`,
      `discover`, `git_installed`; `handoff::git` now wraps `git_raw`)
- [x] WP1 snapshot + checkpoint refs + retention (`snapshot_tree`, `take_checkpoint`)
- [x] WP1 tests: 19 in `git::tests`, all green. They cover the invariant,
      filters, a real split index, a linked worktree, a subdirectory tab, a
      nested repo, index.lock, a corrupt index, unborn HEAD, dedupe/chain,
      create-only, and the cap.
- [x] WP1 UI wiring + config (clippy clean, 138/138 tests):
      - `TabState.last_output` + `CheckpointTrack`
      - a 2 s in-memory poll (`start_checkpoint_watch`/`poll_checkpoints`)
        that fires after 8 s quiet
      - a bell trigger with an **info-level bell log** (for WP0.1)
      - `win.checkpoint-now` in the right-click menu, with toasts via a new
        `ToastOverlay` around the tab view (WP3 reuses it)
      - tooltip `Checkpoint n · HH:MM`, and a warning indicator icon on failure
      - flat config `checkpoints: bool` (default true) plus a Settings switch.
        This deviates from the plan's nested `checkpoint.enabled` to match
        the neighbouring flat bools.
- [ ] WP1 live check (needs the user, alongside WP0.1/0.2): run
      `dbus-run-session -- ./target/debug/agent-terminal`, open a tab in a
      scratch repo, finish a turn, then
      `git for-each-ref refs/agent-terminal` and
      `journalctl --user -t agent-terminal --since -10min -g 'Bell|Checkpoint'`
- [x] WP1 README (feature bullet, "Turn checkpoints" section with limits and a
      removal one-liner, project structure) and AGENTS.md (module layout,
      checkpoint invariant, testing note)
- [x] WP1 gate: fmt, clippy `-D warnings`, 138/138 tests.
      `xvfb-run` isn't installed locally, so the tests ran on the desktop display.
- [x] WP1 rust-reviewer round 1: CHANGES-REQUIRED, 0 blockers. Fixed:
      - the secret filter is now `is_secret_path`, which also checks directory
        components (.ssh/.aws/.gnupg/.kube/.docker/…) and more names. It
        deliberately does not substring-match `token`/`secret`, which would
        drop `tokenizer.rs`.
      - `apply_checkpoint` no longer holds the tab borrow across GTK setters;
        the mapping is a pure `describe_checkpoint`, now tested.
      - `running: bool` became `started: Option<Instant>`, expiring after 120 s,
        so a result lost in a tab drag can't wedge a tab.
      - the bell log names its demotion condition.
      - new tests for stripped `GIT_*` vars and for the `-dash.txt` and
        non-UTF-8 names.
      Gate: clippy clean, 141/141.
      Deliberately not done: a real-repo age-prune test (the pure test covers
      it), and a sparse-checkout test (documented limit).
- [x] WP1 rust-reviewer round 2 (delta): **PASS**. Its one nit (a late result
      from an attempt that expired after 120 s could clear a newer attempt's
      `started`) needs a snapshot longer than 120 s against 10 s step
      timeouts. Not acted on.
- [x] WP1 committed on `feat/checkpoints`. The repo's commit-msg hook strips
      the AI co-author trailer, so none is added.
- [x] WP2 logic: `src/diff.rs` (pure, 7 tests; `mod diff;` hooked up again),
      `RefEntry.parent` via `%(parent)`, `git::empty_tree`, and
      `git::tab_diff(dir, key, base)`, which returns
      NotRepo/Unavailable/Ready(TabDiff). It uses
      `diff --no-color --no-ext-diff --no-textconv -M`, a snapshot under
      `index-diff-<key>` so it never shares the checkpoint's private index,
      and skips fetching the text above 200k changed lines. 4 new real-repo
      tests pass.
- [x] WP2 theme diff colours: `Theme::diff_colours` (palette 2/1/6/8 + bold)
- [x] WP2 panel UI:
      - `src/window/diff_panel.rs`: dropdown, summary, refresh; status page
        with Retry; file list that jumps to its diff; tagged TextView; a
        generation counter that drops stale results
      - `win.toggle-diff` on Ctrl+Shift+D and in the right-click menu, as
        "Show or Hide Changes"
      - flat config `diff_panel_visible`/`diff_panel_width`, width applied on
        first allocation and saved on drag
      - refreshes on open, on a new checkpoint, on a base change, and on the
        button; recoloured on theme change and config reload
      - the panel smoke test sits inside `test_window_initialization`,
        because GTK is bound to one thread
      - README (feature, shortcut, "Diff panel" section, structure) and
        AGENTS.md (modules, `DiffBase::ALL` guard) updated
- [x] **WP1 bug found and fixed during WP2:** `fs::copy` gave the private
      index a fresh mtime, which switched off git's racy-clean check. A
      same-size edit made right after a commit was missed by **29 of 150**
      snapshots (measured with a repro loop). `keep_index_mtime` copies the
      original's mtime: **0 of 150**. A mechanism test pins it, because the
      race is timing-dependent. The fix ships in the WP2 commit.
- [x] WP2 gate: fmt, clippy clean, 154/154
- [x] WP2 rust-reviewer round 1: CHANGES-REQUIRED, 3 SHOULD. All fixed:
      1. overlapping refreshes shared `index-diff-<key>`. Each call now gets a
         unique tag. The test `overlapping_refreshes_of_one_tab_each_see_the_whole_tree`
         failed against the shared tag (an index.lock clash) and passes now.
      2 & 3. Placement state moved into `DiffPanel`: `placed` resets on each
         show and `placing` guards programmatic moves (re-entrancy too), so
         only a real drag is saved.
      The NITs are done too: the "This tab" semantics are in the README, the
      gc note is in `tab_diff`, and there's a racy-clean pointer.
- [x] **WP2 live check** (Broadway + Playwright, isolated `dbus-run-session`,
      temp `XDG_CONFIG_HOME`, a `bash` profile so no real CLI runs):
      - a baseline checkpoint about 10 s after launch, and another after an edit
      - Ctrl+Shift+D shows untracked + modified files at the saved 420 px
      - hide/show re-places at 420; a drag saved 520 to config
      - a tab opening with the panel visible uses the saved width
      - "Last turn" switches correctly
      Found and fixed live: the diff view was **white** on a dark terminal
      (now painted with the theme's bg/fg via one shared CssProvider), and
      the builder's `css_classes` had silently dropped `monospace` (now
      `add_css_class`). Popovers look light under Broadway; that's app-wide
      and not new.
- [x] WP2 gate: fmt, clippy clean, 155/155
- [x] WP2 rust-reviewer round 2: **PASS**
- [x] WP2 committed on `feat/checkpoints`
- [x] WP3 code:
      - `src/worktree.rs`: pure paths and branch checks, plus git
        add/remove/is_clean. A leading `-` is refused, and `--` goes before
        paths. The main tree comes from `--git-common-dir`, so a worktree tab
        can spawn another.
      - `win.new-tab-worktree` (Ctrl+Shift+G) and `new-tab-worktree-profile`,
        plus right-click "New Tab in Worktree…" and "New Tab in Worktree As".
      - the AlertDialog validates the branch and base off-thread, debounced
        250 ms with a generation counter; Create stays insensitive until the
        newest check passes; the location is previewed; ignored files are
        noted; creation re-checks.
      - `TabState.worktree` and a tooltip of `⎇ branch · checkpoint`
        (`tab_tooltip`, tested).
      - closing a worktree tab offers a Remove toast only if the worktree is
        clean and no other open tab uses it. It never forces and never
        deletes the branch.
      - restore of a vanished folder shows a toast.
      - flat config `worktree_root` with a Settings row validated as you type.
      Deviations: the actions aren't disabled outside a repo; activating one
      explains instead, since a menu item can't carry a tooltip. There's no
      window-subtitle branch; the tooltip covers it.
- [x] WP3 gate: fmt, clippy clean, 160/160
- [x] WP3 live check (Broadway):
      - Ctrl+Shift+G opens the dialog with the base defaulting to `master`
        and a preview of `.live.worktrees/feat-live-check`
      - Enter creates it and opens a tab there (`git worktree list` confirms)
      - its diff panel reads "No changes"
      - `exit` closes the tab and the "is clean — Remove" toast appears
      - Remove takes the worktree away and keeps the branch
        (`git branch --list` confirms)
      - re-entering the now-existing branch shows "already exists" with
        Create disabled
- [x] WP3 docs: README (feature, shortcut, "Worktree tabs" section,
      structure) and AGENTS.md (module, never-forced rule)
- [x] WP3 review round 1: CHANGES-REQUIRED, 3 SHOULD. All fixed:
      - "clean" now includes **ignored files**, because `worktree remove`
        deletes them silently; tested.
      - in-use is re-checked when Remove is clicked.
      - add/remove get a 120 s timeout, with a `git worktree prune` hint on
        timeout.
      Also: `main_toplevel` refuses submodules and separate git dirs (tested
      with `--separate-git-dir`), and there are tests for names that read as
      options. Gate: 162/162.
- [x] WP3 review round 2: **PASS**
- [x] WP3 committed on `feat/checkpoints`
- [ ] WP3 worktree tabs
- Decisions (2026-09-30, after WP3):
  - **build WP4 now**, which waives the soak.
  - **ship as 2.1.0**: bump, merge `feat/checkpoints` into master, push.
    WP4 is included in the same release.
- [ ] WP4 restore. The design as built:
      - "Undo these changes…" in the diff panel for the Last turn / This tab
        bases restores the working tree to the diff's **left side**. That's
        the state before the last turn, or the state the tab started from.
      - a pinned `pre-restore-<nanos>` ref is taken first, even when nothing
        changed
      - a confirm dialog lists the files that will change or be deleted, the
        files left untouched (never captured), and a mid-turn warning
      - `git restore --source --worktree -- :/`, then contained deletion of
        untracked additions
      - a post-check against the target tree
      - an Undo toast that re-runs the same flow against the pinned ref
      - code in a new `src/restore.rs`
- [x] WP4 code:
      - `restore::{pin, prepare, apply, summary, contained}`
      - `apply` **refuses if the tree moved since the pin**. An agent writing
        while the dialog is up would otherwise lose unpinned work.
      - `TabDiff.undo_to` (the left side; `None` for Uncommitted)
      - an Undo button in the panel
      - `confirm_restore`: prepare off-thread, a destructive AlertDialog with
        the summary, apply off-thread, then a "Changes undone" toast whose
        Undo re-runs the flow against the pin; an incomplete result lists
        what differs and the manual `git restore` line.
      Tests: 8 new (round trip + undo, HEAD/index unchanged, a secret left
      untouched, emptied dirs removed, a pin even when unchanged, a nested
      repo left alone, a stale pin refused, summary text, containment, a
      symlinked parent refused). Gate: clippy clean, 169/169.
- [x] WP4 live check via `preview_app` (first real use of the tool):
      - a simulated turn (a tracked edit plus a new file in a new dir)
      - Last turn shows exactly those changes
      - Undo shows a dialog with correct `−`/`~` marks (confirmed zoomed)
      - Undo Changes deletes `gen/`, reverts `notes.txt` and keeps the
        pre-tab `added.txt`
      - the toast's Undo re-dialogs and brings everything back
      - `preview_stop` left no processes.
      Cosmetic fix: the confirmation label is no longer selectable, since it
      opened with all its text selected.
- [x] WP4 docs: README ("Undoing a turn", feature, structure) and AGENTS.md
      (undo invariants, plus preview_app for live GUI checks)
- [x] WP4 review round 1: CHANGES-REQUIRED. **A real data-loss path was
      found and fixed:** a file the target checkpoint has but the current pin
      skipped (grown past 5 MiB, a secret-looking name, ignored since) was
      overwritten by `git restore` with no copy anywhere. Those paths are now
      "protected": excluded from restore, the changed list and the post-check.
      The test was mutation-checked: without the fix, the 5 MiB file became
      "small\n". Also:
      - restore gets a 120 s timeout, and any failure after the stale check
        names the pin commit and the `git restore` command to recover
      - a ceiling comment on the check-to-restore window
      - the pin is discarded on "nothing to undo" and on cancel
      Gate: 172/172.
- [x] WP4 review round 2: **PASS**. Its non-UTF-8 nit is closed fail-safe:
      `prepare` refuses (and drops its pin) when a skipped path's name can't
      be matched exactly; tested. 173/173.
- [x] WP4 committed on `feat/checkpoints`
- (superseded) WP4 live check note: **Do the live check with the preview
      MCP's `preview_app`** (added to `~/scripts/crates/preview-mcp` on
      2026-09-30 at the user's request, after the hand-run
      broadwayd/dbus-run-session shells; reviewed PASS, release built, needs
      a `/mcp` reconnect). Suggested call: `preview_app { command:
      ~/git/agent-terminal/target/debug/agent-terminal, config_files:
      {"agent-terminal/config.json": <bash profile, starting_directory =
      scratch repo>} }`, then playwright on the returned url.
- [x] Release: bumped to 2.1.0, merged `feat/checkpoints` into master
      (`96b45611`), gate re-run on master (173/173), pushed; the
      debian-maintainer build was triggered via its MCP
- [ ] Still open, needs the user: WP0.1/WP0.2. Run a turn in each CLI and
      check `journalctl --user -t agent-terminal -g Bell`: which CLIs ring the
      bell at the end of a turn, and whether any never goes quiet. Then
      demote the bell log to debug (`imp.rs`, comment names this).

Notes:
- **Deviation from the plan text (step 4):** untracked files are staged with
  `git update-index --add --remove -- <paths>` (batches of 500 as argv), not
  `add --pathspec-from-file`. `add` aborts the whole snapshot when a listed
  file has vanished (likely with a live agent) and would glob-expand a `*` in
  a filename. update-index takes paths literally and `--remove` tolerates
  vanished ones. So there is no paths file. The test `odd *name.txt` pins it.
- Snapshot commands also pass `-c core.splitIndex=false -c core.fsmonitor=false
  -c core.untrackedCache=false`, so the private index never writes shared-index
  files or fsmonitor tokens into the repo. Reading a split real index still
  works, and the test proves it.
- Inherited `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/… are stripped from
  every git call.

## Review log

- 2026-09-30, plan-reviewer: 3 BLOCKER, 11 SHOULD, 3 NICE — all folded in below.
  The only one not adopted is "cut WP4", since D3 keeps it. Its blockers are
  addressed instead.
- 2026-09-30, plan-reviewer re-review: all 3 blockers RESOLVED. One new
  BLOCKER (`ls-files --directory` bypassed the per-file filters) was fixed in
  WP1 step 4 and gets a dedicated test.

## What exists today (b1a7ceb0)

- `TabState` (`src/window/imp.rs:24`) has a fixed `dir`, a unique `key`, and
  `screen_dirty`. There is no per-tab git knowledge.
- `connect_bell` (`imp.rs:1826`) is the only turn-end signal, and it is only
  used for attention/notification.
- `handoff.rs:579` has a private `git()` helper that caps output lines and
  decodes lossily. It is fine for briefs, but unusable for `-z`/numstat/full diffs.
- `utils::run_command` (`utils.rs:400`) takes a pre-built `Command`, so env can
  be set, but it forces `stdin(Stdio::null())` (`utils.rs:410`).
- The tab content is `Box[quota_banner, exit_bar, search_bar, stack]`
  (`imp.rs:1678`). There is no side-panel slot.
- New-tab entry points: `new_tab`, `new_tab_in_folder(profile)`, and the
  `New Tab in Folder As` menu built by `build_profile_menu` (`imp.rs:3345`).
- Dependencies: gtk4, vte4, libadwaita, serde, tracing. No git library.
- Free accelerators: `Ctrl+Shift+D` and `Ctrl+Shift+G` (`main.rs:114-143`).

## Facts and assumptions

| # | Claim | Status | Evidence / how to verify |
|---|-------|--------|--------------------------|
| F1 | git 2.53 is installed locally | VERIFIED | `git --version` |
| F2 | `run_command` takes a `Command` but nulls stdin | VERIFIED | `utils.rs:400`, `utils.rs:410` |
| F3 | The snapshot sequence (temp index in the git dir → `add -u` from the toplevel → `add --pathspec-from-file=<file>` → `write-tree`) leaves the real index, HEAD, worktree and stash untouched | By git semantics; **the WP1 invariant test must prove it** for this exact sequence | — |
| F4 | Claude Code rings the terminal bell at turn end with the user's config (`preferredNotifChannel` unset, so `auto`) | **UNVERIFIED** | WP0 |
| F5 | agy/gemini ring the bell at turn end | **UNVERIFIED** | WP0 |
| F6 | CI has `git` on PATH | VERIFIED | tests run in GitHub Actions `ubuntu-latest` (`.github/workflows/ci.yml:13,47`, `xvfb-run -a cargo test`), which ships git. debian-maintainer only builds (`config.yaml:141`), with no tests. CI has **no git identity**, so test fixtures must pass `-c user.name/email` |
| F7 | Claude keys sessions on cwd, so a worktree tab gets its own session list and resume works | **UNVERIFIED** (`find_session_dir`, `utils.rs:577`, maps id → cwd; the relationship does not follow from it) | WP3 manual check: resume a worktree tab |
| F8 | Refs under `refs/agent-terminal/*` are not pushed by a default `git push`; `--mirror` and local clones **do** carry them | By git semantics | documented in the README |
| F9 | `screen_dirty` is consumed by the quota poll (`replace(false)`, `imp.rs:3585`) | VERIFIED | grep |
| F10 | `git restore --source=<c> --worktree -- :/` reverts modified files and **deletes files that are in the index but not in `<c>`**, but **leaves untracked files** | VERIFIED (git 2.53, scratch repo: `staged.txt` deleted, `untracked.txt` kept, `a.txt` reverted) | So WP4 step 3's explicit deletion is required for untracked additions, and its idempotence note holds |
| F13 | This machine runs a global gitleaks pre-commit hook | VERIFIED (fired on a scratch `git commit`) | `commit-tree` runs no hooks, so snapshots are unaffected. Test fixtures should use `commit-tree` or `-c core.hooksPath=/dev/null` to stay fast and hermetic |
| F11 | repo-index indexes every non-dot directory under `~/git` | VERIFIED | `scripts/crates/repo-index/src/main.rs:232-242` |
| F12 | `ls-files --others --exclude-standard` (no `--directory`) lists files in a new dir individually, and a nested repo as one `dir/` entry | VERIFIED (git 2.53) | scratch repo: output `newdir/.env`, `newdir/ok.txt`, `vendor/x/` |

## Design decisions

- **New module `src/git.rs`**:
  - Its runner, `git_raw(dir, args, env) -> Result<Vec<u8>, String>`, returns
    raw bytes. It is built on `run_command`, with no line cap and no lossy decode.
  - `handoff::git()` becomes a thin wrapper over `git_raw` that keeps its
    cap/lossy behaviour, so there is one runner and two presentations.
  - The pure helpers (parsers, path derivation, filters) are the unit-tested
    part. AGENTS.md lists `git.rs` under module layout, and under "pure
    logic" **only for those helpers**, since the module shells out.
- **Hardened git environment on every call:** `GIT_TERMINAL_PROMPT=0` and
  `GIT_OPTIONAL_LOCKS=0`, so status and diff refreshes never take
  `index.lock` and race the agent's own git. `LC_ALL=C` makes parsed output
  stable.
- **Shell out to `git`; add no `git2`/`gix` dependency** (standalone rule,
  handoff precedent, no libgit2 system dependency).
- **Everything git-related runs through `gio::spawn_blocking`.** The
  timeouts are 10 s for snapshots and 5 s for queries.
- **Turn-end trigger = bell, OR output quiet for 8 s after activity.**
  - This does **not** reuse `screen_dirty` (F9). It uses a new per-tab
    `last_output: Rc<Cell<Instant>>`, set by the same `contents_changed`
    handler, plus a `glib::timeout` debounce.
  - Identical trees are deduplicated, so over-triggering costs one `git add`.
  - WP0 checks whether a status-line clock or spinner keeps a tab from ever
    going quiet. If so, the bell is that CLI's only trigger and the
    checkpoint-now action covers the gaps.
- **Constants, not knobs, for v1** (review: too many config fields). The caps,
  retention, quiet period and diff limit are `const`s in `git.rs`. Config
  exposes only `checkpoint.enabled`, `worktree_root` and the diff-panel
  visibility/width.
- **Three states everywhere** (AGENTS rule): *not a repo* (feature silently
  absent), *ok*, and *git failed* (visible, never collapsed into "no changes").
- **Tab indicator precedence** (the page has one indicator icon):
  quota > exit > git-failed. Attention uses `needs_attention`, not the icon,
  so it does not compete.

---

## WP0: verify before building (≈1 h, no code shipped)

1. **Bell (F4/F5):** run a debug build under `dbus-run-session` with
   `RUST_LOG=debug`, finish one turn each in Claude, agy and gemini, then
   `journalctl --user -t agent-terminal --since -10min -g Bell`.
2. **Quiet-screen:** in the same run, check whether an idle CLI keeps
   emitting `contents_changed` (a clock or spinner).
3. **Restore semantics (F10):** in a scratch repo, add a file after a
   commit-tree snapshot, run `git restore --source=<c> --worktree -- :/`, and
   record whether the file survives.
4. **CI git (F6):** read the pipeline's build image.

Record the outcomes in the F-table. WP1 does not start while F4, F5 and F6
are all unknown.

## WP1: `git.rs` foundation + checkpoints (core, ship first)

### Repo discovery
- `RepoInfo { toplevel, git_dir, index_path, head: Option<Oid>, branch: Option<String> }`
  comes from a single `rev-parse` call (`--show-toplevel --absolute-git-dir
  --git-path index` + `HEAD` + `--abbrev-ref HEAD`). An unborn HEAD gives `head: None`.
- `TabState` gains `repo: RepoState` (`Unknown | NotRepo | Repo(RepoInfo) | Error(String)`),
  resolved off-thread after spawn. It is re-resolved before each snapshot or
  diff, because an agent may `git init` or switch branches mid-session.

### Snapshot (never touches user state)
Every command runs **from `toplevel`**, not the tab dir, which may be a subdirectory.

1. **Temp index inside the git dir:** `<git_dir>/agent-terminal/idx-<key>`.
   Being inside the git dir is what makes a split index (`sharedindex.*`)
   still resolve; `$XDG_RUNTIME_DIR` would not.
2. **Seed it:**
   - If `index.lock` is present, **skip this trigger** (the agent is
     mid-operation). This is a silent retry-on-next-trigger, not an error.
   - Otherwise copy the real index. If the copy is missing, or a later step
     reports a bad index checksum (a torn read), re-seed with
     `git read-tree HEAD`. For an unborn HEAD, use an empty index.
   - Seeding from HEAD rather than leaving the index empty is what lets
     `add -u` see tracked files.
3. **Tracked changes:** `git add -u` (whole-tree since git 2.0).
4. **Untracked files:** `git ls-files -z --others --exclude-standard`, listing
   **individual files**. Deliberately no `--directory`: it collapses a wholly
   untracked directory into one `dir/` entry, and staging that pathspec would
   bypass every per-file filter below (review 2, BLOCKER). Then filter:
   - **Skip nested repos:** without `--directory`, git still reports an
     untracked nested repository as a single `dir/` entry, because it does
     not descend into it. Any entry ending in `/` is therefore dropped, and
     no gitlinks get created. This is git behaviour, so the tests pin it.
   - **Secret denylist** (the handoff redaction mandate): skip basenames
     matching `.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`,
     `id_ed25519*`, `*.kdbx`, `credentials*`, `*.tfstate*`.
   - **Size/count caps:** skip files over 5 MiB, stop after 2,000 files.
   - Write the survivors NUL-separated to `<git_dir>/agent-terminal/paths-<key>`
     (F2: no stdin), then run `git add --pathspec-from-file=<file> --pathspec-file-nul`.
   - Keep the skipped paths (with a reason) on the checkpoint record for WP4.
     Log the count at info.
5. `git write-tree` → tree. If it equals the tab's last checkpoint tree, stop (dedupe).
6. `git -c commit.gpgsign=false commit-tree <tree> -p <prev | HEAD>`, with a
   fixed `GIT_AUTHOR_*`/`GIT_COMMITTER_*` of
   `agent-terminal <agent-terminal@localhost>`. An unborn HEAD with no previous
   checkpoint gets no parent. The message is
   `checkpoint <n> · tab <key> · <profile> · <iso-time>`. The skipped-path list
   goes in the message body, capped at 50 lines.
7. `git update-ref refs/agent-terminal/<key>/<n:04> <commit> 0000000000000000000000000000000000000000`:
   **create-only**, so no ref can ever be overwritten.
8. Delete the temp index and the paths file (also on every error path, via a
   drop guard).

### Known limits (documented in the README, not solved)
- **Submodules:** the pointer is captured, but a dirty submodule's own
  contents are not.
- **LFS:** `git add -u` runs the clean filter, so tracked LFS files fill
  `.git/lfs/objects`. The 5 MiB cap applies to untracked files only.
- **Sparse checkout:** skip-worktree entries are carried over from the index
  unchanged.
- **Ignored files** (`node_modules`, build output) are never captured.

### Retention (constants)
- There are at most 50 refs per tab; the oldest is deleted past the cap. Up to
  50 loose refs per tab is fine, and `git pack-refs` tidies them; the README
  mentions it.
- When a tab first resolves a repo, it deletes `refs/agent-terminal/*` whose
  commit is older than 7 days. We never run gc ourselves.

### Undo / rollback of the feature itself
- `git for-each-ref --format='delete %(refname)' refs/agent-terminal | git update-ref --stdin`
  removes every ref (README). `checkpoint.enabled = false` turns it off.

### Tests (tempfile repos, skipped when `git` is absent)
- The snapshot captures modified, deleted and untracked files. It excludes
  ignored, oversized, denylisted files and nested repos, and records the skips.
- **Invariant test (exact sequence):** real `index` bytes, `HEAD`,
  `git stash list`, and the worktree file listing are identical before and
  after. It runs once in a normal repo, **once in a linked worktree**, and
  **once with `core.splitIndex=true`**.
- **A new untracked directory** `newdir/` holding `.env`, a 6 MiB file, and a
  normal file: only the normal file is in the snapshot tree (review 2).
- An untracked nested repo `vendor/x/.git` is listed as `vendor/x/` and is
  absent from the tree, with no gitlink.
- The torn-index re-seed is attempted **once**. A second failure gives the
  git-failed state for that trigger only. Re-seeding from HEAD drops
  staged-only state from the *snapshot's* view, which is harmless because
  `add -u` plus the untracked pass re-add the worktree content.
- The tab dir is a subdirectory, yet changes elsewhere in the repo are captured.
- A present `index.lock` means a skip, with no error state.
- Unchanged tree → one checkpoint. Unborn HEAD → parentless first checkpoint.
- Create-only `update-ref` refuses an existing ref.
- Retention: count cap, then age prune by commit time.
- Pure unit tests: ref naming/parsing, the untracked filter (size, denylist,
  nested repo), and the `rev-parse` output parser.

### UI (minimal in WP1)
- A tab tooltip shows `Checkpoint n · <time>`.
- On failure the git-failed indicator appears, subject to the precedence
  above, with the error in its tooltip. No dialog is shown, since snapshots
  are background work.
- `win.checkpoint-now` goes in the right-click menu.

## WP2: diff panel

### Layout
- The tab content becomes `Box[banners…, Paned(h){ stack | diff_panel }]`,
  per tab and hidden by default.
- `win.toggle-diff` is bound to `Ctrl+Shift+D` and added to the right-click
  menu. Visibility and pane width persist in config (`diff_panel.visible`,
  `diff_panel.width`) and apply to new tabs.

### Content
- Header: a base dropdown with **Uncommitted**, **Last turn** and **This tab**,
  plus a refresh button and an "n files, +a −d" summary.
  - **Uncommitted:** HEAD → a fresh snapshot *tree* (WP1 steps 1–5, without
    commit/ref), so untracked files show up.
  - **Last turn:** checkpoint n-1 → n.
  - **This tab:** first checkpoint's parent → the current snapshot tree.
  - **Unborn HEAD or a parentless first checkpoint:** diff against the
    empty tree (`git hash-object -t tree /dev/null`, computed rather than
    hardcoded).
- Body: a file list (`git diff --numstat -z <a> <b>`) above a monospace
  `TextView` holding `git diff --no-color <a> <b>`.
  - Lines are coloured with `TextTag`s (add/del/hunk/header) taken from the
    active `Theme` palette, and follow theme changes.
  - The text is inserted as plain text, never markup.
  - Clicking a file scrolls to its header.
- **Caps (const):** at most 1 MiB or 20k lines, followed by a
  "Diff truncated — n more lines" footer.
- No GtkSourceView: four tags cover the colouring without a new system library.

### Refresh
- On panel open, after a new checkpoint, on the refresh button, and on base
  change. There is **no timer polling**.
- Each refresh is one `spawn_blocking`. A generation counter drops stale results.

### States
- *Not a repo* shows a status page.
- *git failed* shows the error and a Retry button.
- *Clean* shows "No changes".
- *No checkpoints yet* greys out Last turn/This tab, with a tooltip.

### Tests
- Pure: a diff line classifier, and a numstat parser (binary `-`,
  NUL-separated renames).
- Pure: truncation, and the empty-tree base choice.
- Smoke: building a tab with the panel does not panic under Xvfb.

## WP3: New Tab in Worktree…

### Entry points
- `win.new-tab-worktree` (`Ctrl+Shift+G`), plus **New Tab in Worktree…** and
  **New Tab in Worktree As ▸** in the **+** dropdown and the right-click menu,
  mirroring the folder entries.
- The actions are disabled unless the current tab's `repo` is `Repo(_)`; the
  tooltip explains why.

### Dialog (`adw::AlertDialog` with extra child)
- **Branch:** validated inline on every change (the AGENTS rule).
  `git check-ref-format --branch <name>` must pass and
  `git show-ref --verify refs/heads/<name>` must fail. The show-ref check runs
  off-thread and debounced. Create stays insensitive until the branch is valid.
- **Base:** the source tab's current branch/HEAD by default. Any commit-ish is
  accepted, verified with `git rev-parse --verify`.
- **Location preview** (read-only): `<toplevel>/../.<repo>.worktrees/<branch, / → ->`
  (D2), or `worktree_root/<repo>/<branch>` when configured. The path must not
  exist.
- **Note in the dialog:** "Ignored files (`node_modules`, `.env`, build output)
  are not copied into a new worktree."

### Create
- Run `git -C <toplevel> worktree add -b <branch> <path> <base>` off-thread,
  then `add_terminal_tab(profile, Some(path))`.
- On failure, `present_message` shows git's stderr, and no tab is opened.

### Lifecycle — never destructive by default
- `TabState` gains `worktree: Option<WorktreeInfo { path, branch, repo_toplevel }>`.
  The tooltip and window subtitle show `⎇ <branch>`.
- **Tab close (window stays open):** if `git status --porcelain` is empty, an
  `adw::Toast` offers "Worktree ⎇ x is clean — Remove", which runs
  `git worktree remove <path>`.
  - **Never `--force`, never deletes the branch.**
  - A dirty worktree is simply kept.
- **Window/app quitting:** the toast is not shown, and the worktree is kept.
  Cleanup is manual (README: `git worktree list`, `git worktree remove`,
  `git branch -d`).
- **Session restore of a removed worktree:** it uses the existing
  `directory_is_usable` fallback, extended with a toast naming the missing
  path, so a vanished worktree is reported rather than silently opening in
  `$HOME`.

### Tests
- Pure: default-path derivation (hidden sibling, slashes, `~`, override) and
  branch → dir sanitising.
- Integration: a worktree is created on a new branch from a base, and
  `worktree remove` (no force) refuses a dirty tree.
- Manual: resume a Claude session inside a worktree tab (F7).

## WP4: restore to checkpoint (after ≥1 week of WP1–WP3 soak)

This is the only destructive operation. Every step below exists to close a
data-loss path the review found.

- **Action:** **Restore to this checkpoint…** in the diff panel when the base
  is a checkpoint.
- **Confirmation (`adw::AlertDialog`):**
  - lists the files that will change or be deleted
  - warns if output changed within the quiet period (mid-turn)
  - **lists every path the target checkpoint or the pre-restore snapshot
    skipped** (ignored, over the cap, denylisted), stating these are
    untouched and will not match the checkpoint
  - says HEAD and the index are not changed
- **Steps:**
  1. **Pin an undo point:** take a snapshot and write it to
     `refs/agent-terminal/<key>/pre-restore-<unix-ts>` **even if its tree
     dedupes** against the last checkpoint. The ref is pinned explicitly, not
     inferred.
  2. `git restore --source=<ckpt> --worktree -- :/` (from the toplevel).
  3. Delete what the checkpoint lacks:
     `git diff --name-only -z --diff-filter=A <ckpt> <pre-restore>`.
     - Each path is canonicalised through its **parent** directory; a
       symlinked parent that resolves outside the canonical toplevel is
       rejected.
     - `std::fs::remove_file`, never following a final symlink.
     - Directories are never removed recursively; empty ones are only
       removed after their files, with `remove_dir`.
     - Whether step 2 already deletes these (F10) does not matter: step 3 is
       idempotent.
  4. Post-check: a fresh snapshot tree must equal `<ckpt>`'s tree modulo the
     skipped paths. On a mismatch, show the differing paths and offer undo.
- **Undo:** restore to the pinned `pre-restore-*` ref.
- **Tests:**
  - Round trip: modify, add and delete files, then restore; the tree equals
    the checkpoint.
  - Undo returns to the pre-restore tree, even when the pre-restore tree
    deduped.
  - HEAD and index are unchanged.
  - A symlinked parent pointing outside the toplevel is refused.
  - A skipped (oversized) file survives untouched and appears in the
    dialog's list.

---

## Config additions (serde `default`, no migration needed)

```jsonc
"checkpoint": { "enabled": true },
"diff_panel": { "visible": false, "width": 520 },
"worktree_root": null
```

Settings dialog: a "Git" group with the checkpoint switch, a
diff-panel-on-new-tabs switch, and the worktree root (validated as you type,
like the starting directory).

## Order, gates, release

WP0 → WP1 → WP2 → WP3 → (soak ≥1 week) → WP4. Each WP is its own commit/PR,
with:
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`,
  then the **rust-reviewer** agent.
- A manual check via `dbus-run-session -- ./target/debug/agent-terminal` on a
  scratch repo.
- README (features, shortcuts, known limits, removal commands) and AGENTS.md
  (`git.rs`) updates in the same commit as the code.
- Ship via push → debian-maintainer apt pipeline, with a minor bump to 2.1.0.
  No `make install`.

## Risks

| Risk | Mitigation |
|------|------------|
| A large untracked dir bloats `.git/objects` | Size/count caps, skip logging, `enabled` switch |
| Secrets copied into the object DB | Denylist, 7-day retention, README note on `--mirror`/clones |
| Racing the agent's own git | Separate index, skip on `index.lock`, `GIT_OPTIONAL_LOCKS=0` on reads |
| Snapshot slow on huge repos | 10 s timeout, non-blocking tab warning, dedupe |
| Writing into the user's repo is surprising | Refs-only namespace, fixed author, one-line removal, off switch |
| Restore clobbering live or unsnapshotted work | WP4 last, pinned undo ref, skipped-path disclosure, path containment, post-check |
| Worktree left behind | Clean-only removal toast, never force, README cleanup |
