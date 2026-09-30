# Agent Terminal 🚀

A standalone GTK4 terminal application written in Rust, purpose-built for driving
an AI coding CLI (Claude, Antigravity/`agy`, or Gemini) in a focused, tabbed
window.

> **Renamed in 2.0.0.** This was `antigravity-terminal` (and, before that,
> `gemini-terminal`). The binary, package and config directory are now
> `agent-terminal`; existing settings are adopted automatically on first run —
> see [Upgrading](#upgrading-from-1x-).

## Features ✨

- **Native GTK4 & VTE4**: high-performance terminal rendering with a Libadwaita
  header bar.
- **Tabbed sessions**: multiple terminals in one window.
  - `Ctrl + Shift + T` (or the header **+**) opens a new tab rooted in the current
    tab's directory. The tab bar auto-hides when only one tab is open.
  - **New Tab in Folder…** opens a folder picker and roots a new tab there —
    useful because a running CLI session cannot re-root itself.
- **Resume a session by ID**: `agent-terminal --resume <session-id>` opens a tab
  in the running window (or a new one), resuming that conversation from the
  directory it was recorded in. Or browse for it: **Resume Session…** on the
  **+** dropdown and the right-click menu lists recent sessions by title, folder
  and age, with search; **Resume Session As** browses another CLI's sessions
  (Claude or Agy). See [Resuming sessions](#resuming-sessions).
- **Hand a task to another CLI**: when Claude or Agy runs out of quota, a banner
  on the tab offers **Continue in <other CLI>**. It opens a tab in the same
  folder whose first prompt points at a brief the terminal wrote from the
  transcript and the git state. **Continue In** on the menus does the same at
  any time. See [Handing off between CLIs](#handing-off-between-clis).
- **Turn checkpoints**: in a git repository, the working tree is snapshotted
  into hidden refs whenever a turn ends, without touching your branch, index
  or stash. See [Turn checkpoints](#turn-checkpoints).
- **Diff panel**: `Ctrl + Shift + D` shows what changed beside the terminal:
  uncommitted work, the last turn, or everything since the tab opened. See
  [Diff panel](#diff-panel). Its undo arrow reverts the last turn, keeping what
  it replaces so the undo can itself be undone. See [Undoing a turn](#undoing-a-turn).
- **New Tab in Worktree**: `Ctrl + Shift + G` creates a branch in its own git
  worktree and opens a tab there, so two agents can work on one repository
  without trampling each other. See [Worktree tabs](#worktree-tabs).
- **Sessions survive a crash**: if the CLI exits non-zero the tab stays open with
  its scrollback intact and a bar explaining what happened, offering **Restart**
  and **Close Tab**. A clean exit still closes the tab as you would expect.
- **Attention when a turn finishes**: a bell in a background tab marks that tab,
  with an optional desktop notification — so an agent that finishes while you are
  elsewhere actually reaches you. The notification also fires for the tab in view
  when the window itself is in the background, and is cleared once you look.
- **Scrollback search**: `Ctrl + Shift + F`, with case-sensitivity and regex
  toggles.
- **Session restore** (off by default): reopens the tabs, and their directories,
  of the last window closed, each as a fresh session.
- **Selectable colour themes**: Antigravity (default), Dracula, Nord, Gruvbox
  Dark, Solarized Dark, One Dark, and Monokai — applied live to every open tab.
- **Profiles**: any CLI, with its own arguments, directory and environment,
  defined in `config.json` — no rebuild. Auto-detection picks the first one
  installed. Resolution runs off the UI thread and is cached for the life of the
  process. See [Profiles](#profiles).
- **Settings**: starting directory (validated as you type), scrollback lines,
  font and font scale, cursor shape and blink, profile, theme, notifications and
  session restore — each applied immediately, to every window, and persisted to
  `~/.config/agent-terminal/config.json`. Hand edits to that file are picked up
  while the app runs (profile and indicator lists in the header refresh in new
  windows). A file that cannot be parsed is copied aside
  (`config.json.invalid-…`) and reported before defaults can replace it; an
  edit that does not parse yet suspends saving until it is fixed.
- **Shortcuts**:
  | Keys | Action |
  |---|---|
  | `Ctrl + Shift + T` | New tab |
  | `Ctrl + Shift + W` | Close tab |
  | `Ctrl + Shift + R` | Restart the current session |
  | `Ctrl + Shift + F` | Search the scrollback |
  | `Ctrl + Shift + D` | Show or hide the diff panel |
  | `Ctrl + Shift + G` | New tab in a new worktree |
  | `Ctrl + Shift + C` / `V` | Copy / paste |
  | `Ctrl + Tab` / `Ctrl + Shift + Tab` | Next / previous tab |
  | `Ctrl + Page Down` / `Page Up` | Next / previous tab |
  | `Alt + 1`…`8`, `Alt + 9` | Jump to tab, or the last tab |
  | `Ctrl + Alt + 1`…`9` | New tab as the Nth profile, in the current folder |
  | `Ctrl + Plus` / `Minus` | Zoom (persisted) |
  | `Ctrl + 0` | Reset zoom |
  | `Ctrl + Left-Click` | Open a hovered hyperlink |

  These are caught before the terminal sees them, so the CLI never receives
  them. Two a CLI might otherwise use: `Ctrl + Minus`, which terminals send as
  `^_` (undo in readline), and `Alt + 1`…`9`, readline's numeric arguments.
- **Observability**: logs to the systemd journal, with a panic hook, so a
  desktop-launched failure is diagnosable after the fact:
  ```bash
  journalctl --user -t agent-terminal -b
  ```
  When run from a terminal, logs also print to stderr. Level defaults to `info`
  and is overridable via `RUST_LOG`.
- **Standalone identity**: treated as a unique application by your window manager
  (won't group with standard terminals). All assets are embedded in the binary.
- **Sixel support**: inline image rendering.

## Upgrading from 1.x ⬆️

The apt package is renamed, so `apt upgrade` pulls in `agent-terminal` and
removes `antigravity-terminal` via a transitional package.

On first launch, settings at `~/.config/antigravity-terminal/config.json` are
copied to `~/.config/agent-terminal/config.json`. **The old file is deliberately
left in place** so that reinstalling 1.x still finds its configuration.

## Prerequisites 🛠️

- **Rust & Cargo** (1.92+)
- **GTK 4.10+** (`libgtk-4-dev`)
- **VTE 2.91 GTK4** (`libvte-2.91-gtk4-dev`)
- **libadwaita 1.5+** (`libadwaita-1-dev`)

## Quick Start ⚡

```bash
make deps          # install system dependencies
make start-local   # build (debug) and run
```

> **Running a local build while the packaged one is open?** GTK's single-instance
> handling will hand your launch to the already-running process, so your build
> never actually runs. Use its own bus:
> ```bash
> dbus-run-session -- ./target/debug/agent-terminal
> ```

## Advanced Usage 🔧

### Profiles
Profiles are the sessions offered in Settings, on the **+** button's dropdown,
and in the right-click **New Tab As** menu. Adding a CLI is a config edit, not a
rebuild:

```jsonc
{
  "profiles": [
    { "name": "Claude",  "command": "claude" },
    { "name": "Codex",   "command": "codex", "args": ["--full-auto"] },
    // Root a profile in a specific project, with its own environment.
    { "name": "Infra",   "command": "claude",
      "dir": "~/git/ansible-homelab",
      "env_file": "~/.config/agent-terminal/infra.env" }
  ],
  "default_profile": "Claude"   // omit, or null, to use the first one installed
}
```

`env_file` is sourced in a subshell and its *exported environment* merged into
the session. Nothing it prints reaches the terminal — output before `exec` breaks
the CLI's terminal handshake, which is why the old "startup script" setting could
never work.

### Resuming sessions
```bash
agent-terminal --resume 215bf7f7-88e4-4070-b41b-332500303534
agent-terminal --resume <id> --dir ~/git/some-project   # skip the lookup
```

A launch while Agent Terminal is already running hands the request to that
instance and exits, so the tab opens in your existing window. Bad IDs and
directories are reported on the terminal you typed the command in, with exit
status 2.

Resuming is declared per profile, so the binary knows no CLI's conventions:

```jsonc
{ "name": "Claude", "command": "claude",
  "resume_args": ["--resume", "{id}"],          // {id} is the session ID
  "session_store": "~/.claude/projects",        // where <id>.jsonl transcripts live
  "session_title": "/aiTitle" }                 // JSON pointer to a session's title
```

**Resume Session…** opens a browser of the store's 200 most recently active
sessions, newest first. Each row shows the session's title, the folder it will
resume in, and how long ago it was last active. Type to filter by title, folder
or ID; Enter resumes the top match. **Enter ID…** in its header falls back to
pasting an ID. The latest title in a transcript wins, since a CLI may retitle a
session as it goes; a session without one is listed by ID. Only the first and
last 256 KiB of each transcript are read, so a store of multi-MiB transcripts
still lists quickly. A store that cannot be read says so rather than showing an
empty list.

`session_store` exists because Claude only resumes a session from the project
directory it was recorded in. The transcript is looked up (directly in the store
or one directory down) and its first recorded `cwd` becomes the tab's directory.
If it can't be found, the tab opens in the profile's default directory and a
dialog explains how to pass `--dir`.

Agy keeps one history log instead of a transcript per session, so its profile
declares `"session_format": "agy-history"`. Its sessions are titled by their
first prompt and resumed with `--conversation <id>` in the workspace they were
recorded in. Only the last 4 MiB of the log is read.

Claude and Agy profiles saved by an older release are filled in with the
settings above on load; so are the hand-off settings below. An explicit empty
list, such as `"resume_args": []`, opts out. Other CLIs can resume once they
declare `resume_args`. Restarting a resumed tab resumes it again, with the
tab's own profile, rather than starting a new session.

### Handing off between CLIs
When one CLI runs out of quota mid-task, **Continue In ▸ <profile>** (on the
**+** dropdown and the right-click menu) opens a tab running the other one in
the same folder. It starts with a prompt that points at a *hand-off brief*. The
out-of-quota CLI cannot summarize its own work, so the terminal writes the
brief from what is on disk:

- the original request, the latest requests, the last reply and the files the
  session edited, from the transcript (Agy's log records requests only);
- `git status`, `git diff HEAD --stat` and the last five commits;
- the end of the screen, if no transcript can be found.

Briefs are written to `$XDG_STATE_HOME/agent-terminal/handoffs/` (by default
`~/.local/state/…`). The directory is `0700`, each file is `0600`, and briefs
older than 7 days are pruned. Credentials in common formats (`ghp_…`, `sk-…`,
`AIza…`, `Bearer …`, `*_KEY=…`, `"password": …`, PEM blocks) are masked as
`****` plus their last four characters. The masking works by pattern, so a
secret in an unrecognized format can get through, and each brief says so. The
source tab stays open, so you can resume it once its quota resets. The
receiving CLI may ask permission before reading a file outside the project.

The terminal also watches for the quota running out. A new Claude tab is started
under an ID the terminal picks (`--session-id`), so its transcript is known, and
the watcher looks for a `rate_limit` error there. A CLI with no such record uses
`limit_markers`, text matched near the bottom of the screen. Either way, the tab
shows a banner offering **Continue in <next profile>**, which hides again once
the session replies normally. A transcript that can't be read leaves the banner
as it was rather than reporting that all is well.

It also raises a desktop notification with the same **Continue in …** button,
so you can hand off without opening the window. Clicking the notification
itself brings that tab forward. The notification is withdrawn if the session
recovers or the tab closes. Nothing switches until you click. Turn it off with
**Notify When Out of Quota** in Settings (`notify_on_quota`). It is on by
default, unlike bell notifications.

```jsonc
{ "name": "Agy", "command": "agy",
  "prompt_args": ["--prompt-interactive", "{prompt}"],   // how to start with a prompt
  "limit_markers": ["RESOURCE_EXHAUSTED", "quota exceeded"] },
{ "name": "Claude", "command": "claude",
  "prompt_args": ["{prompt}"],
  "session_id_args": ["--session-id", "{id}"] }          // start under a chosen ID
```

Agy's quota message has not been captured yet, so its default markers are the
Google API's error names. Adjust them once you have seen the real message.

### Status indicators
Optional header-bar lights driven by a file or a command. Empty by default.

```jsonc
{
  "indicators": [
    // Non-empty file contents mean "needs attention"; the contents are the detail.
    { "label": "Config drift",
      "source": { "type": "file", "path": "~/reports/drift.txt" },
      "refresh_secs": 300 },

    // A non-zero exit means "needs attention"; stdout is the detail.
    { "label": "Backups",
      "source": { "type": "command", "argv": ["check-backups", "--quiet"],
                  "timeout_secs": 15 },
      "refresh_secs": 900,
      "action": "show-output" }
  ]
}
```

Each indicator has **three** states, not two: OK, needs-attention, and *unknown*.
A source that cannot be read gets its own icon and says so — it is never quietly
reported as healthy. Commands run off the UI thread and are killed at their
timeout. `"action": "send-to-terminal"` adds a button that types the detail into
the running session, behind a preview.

### Turn checkpoints
In a tab whose folder is inside a git repository, the working tree is
snapshotted each time a turn ends: when the CLI rings the bell, or when its
output has been quiet for 8 seconds. **Checkpoint Now** on the right-click menu
takes one on demand. The tab's tooltip shows the latest one. A snapshot that
fails marks the tab with a warning icon, and the tooltip on that icon says why.

Snapshots are commits recorded only as refs under `refs/agent-terminal/<tab>/`.
They never touch your branch, index, working tree or stash. Each captures
tracked changes plus untracked, non-ignored files. It leaves out files over
5 MiB, more than 2,000 new files, nested repositories, and names that look
like secrets (`.env*`, `*.pem`, `*.key`, `id_rsa*`, …). Each tab keeps 50
checkpoints, and any checkpoint older than 7 days is deleted.

A normal `git push` does not send these refs. `git push --mirror`, or a local
`git clone` of the repository, **does**. Known limits: a dirty submodule's
contents, and files ignored by `.gitignore`, are not captured. Tracked Git LFS
files are stored in `.git/lfs/objects` with no size cap.

Turn it off in Settings (**Checkpoint Each Turn**). To remove every checkpoint
from a repository:

```bash
git for-each-ref --format='delete %(refname)' refs/agent-terminal | git update-ref --stdin
```

### Diff panel
`Ctrl + Shift + D`, or **Show or Hide Changes** on the right-click menu, opens a
read-only panel beside the terminal. It shows a file list with line counts, and
a coloured unified diff below it. Clicking a file jumps to its diff. The
dropdown picks what to compare against:

| Base | Shows |
|---|---|
| **Uncommitted** | HEAD against the working tree now, untracked files included |
| **Last turn** | What the tab's latest checkpoint changed |
| **This tab** | Everything since the state the tab's first checkpoint was taken on top of, up to now |

A tab's first checkpoint usually comes a few seconds after it opens, once the
CLI's startup output goes quiet. Uncommitted work already present when the tab
opened, and not changed before that first checkpoint, counts as the tab's in
**This tab**.

The panel refreshes when it opens, when a new checkpoint is taken, when the
base changes, and on its refresh button. It never polls. Git's external diff
and textconv drivers are not run. Diffs above 1 MiB or 20,000 lines are cut
short, and one with more than 200,000 changed lines shows only its file
list. Whether new tabs open with the panel, and its width, are remembered.

### Undoing a turn
With the diff panel on **Last turn** or **This tab**, the undo arrow puts the
working tree back to the diff's left side: how it was before the last turn, or
when the tab started. A confirmation lists every file that will change (`~`) or
be deleted (`−`), and warns if the session still looks busy.

- **The current state is saved first**, as
  `refs/agent-terminal/<tab>/pre-restore-<time>`, and the **Undo** on the
  "Changes undone" message puts it back.
- **Nothing checkpoints never capture is touched**: ignored files, files over
  5 MiB, names that look like secrets, nested repositories. The confirmation
  names them.
- **Commits and staged changes are never touched.** Only the working tree
  changes; HEAD and the index stay as they are.
- **If a file changes after you open the confirmation** (the agent still
  writing), nothing is restored and you're asked to try again, so nothing
  unsaved can be lost.
- Deletions stay inside the repository and never follow a symlinked folder.
- Afterwards the result is checked against the checkpoint. Anything that still
  differs is reported, with the `git restore` command that puts the saved state
  back by hand.

### Worktree tabs
**New Tab in Worktree…** (`Ctrl + Shift + G`, or the right-click menu, where
**New Tab in Worktree As** also picks the CLI) asks for a new branch and what to
start it from. It defaults to the current tab's branch. Both are checked as you
type, and **Create** stays disabled until they pass. It then runs
`git worktree add -b <branch>` and opens a tab in the new worktree.

Worktrees go in a hidden folder beside the repository,
`<parent>/.<repo>.worktrees/<branch>`. They're hidden so tools that scan every
project folder don't index a second copy of the repository. Set **Worktree
Folder** in Settings to use `<folder>/<repo>/<branch>` instead. Ignored files
(`node_modules`, `.env`, build output) aren't copied into a new worktree.

Closing a worktree tab offers **Remove**, but only when the worktree has
nothing uncommitted, no untracked files, no ignored files, and no other open
tab is in it. Ignored files count because `git worktree remove` would delete
them without asking. The in-use check runs again when you click. Removal never
uses `--force` and never deletes the branch. Anything else is simply kept.
Submodules and repositories with a separate git directory aren't supported.
To clean up by hand:

```bash
git worktree list
git worktree remove <path>      # refuses if it has changes
git branch -d <branch>          # refuses if it is unmerged
```

### Generating a Debian package (.deb)
```bash
make package
```
*Requires `cargo-deb`; the Makefile installs it if missing.* Runtime dependencies
are derived from the built binary via `$auto`, so they cannot drift from what it
actually links.

### Development workflow
- **Build**: `cargo build` · **Run**: `make start-local`
- **Lint**: `cargo fmt` and `cargo clippy --all-targets -- -D warnings`
- **Test**: `cargo test`

## Project Structure 📁

- `src/main.rs` — entry point, application setup, logging, global CSS, accelerators.
- `src/window/imp.rs` — window implementation: tabs, spawning, input, settings.
- `src/window/mod.rs` — the `AgentTerminalWindow` GObject wrapper.
- `src/config.rs` — persisted settings and the 1.x migration, with tests.
- `src/theme.rs` — terminal colour schemes.
- `src/utils.rs` — pure logic: profile resolution, startup command, env files,
  status indicators, path resolution, session stores.
- `src/handoff.rs` — pure logic for hand-offs: brief building, redaction and
  quota detection.
- `src/git.rs` — git plumbing for turn checkpoints and the diff panel: repo
  discovery, snapshots through a private index, checkpoint refs, retention and
  diffs.
- `src/diff.rs` — pure logic for the diff panel: bases, numstat parsing, line
  classification and truncation.
- `src/window/diff_panel.rs` — the diff panel's widgets.
- `src/restore.rs` — undoing a turn: saving the current state first, restoring
  a checkpoint to the working tree, contained deletion and the check after.
- `src/worktree.rs` — New Tab in Worktree: default locations, branch checks,
  and creating and (clean-only) removing worktrees.

## Contributing 🤝

1. Branch: `git checkout -b feature/cool-new-thing`.
2. Keep it green: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and
   `cargo test`.
3. Commit using Conventional Commits.

## License 📄

MIT — see [LICENSE](LICENSE).
