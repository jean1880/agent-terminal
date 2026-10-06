# Third-party notices

## T3 Code

Parts of `crates/agent-core` port logic from T3 Code (<https://github.com/pingdotgg/t3code>):
- the session transition policy;
- the context-handoff budget;
- the slash-command ranking;
- the shape of the canonical event stream.

MIT License

Copyright (c) 2026 T3 Tools Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Bundled icons (`assets/icons/`)

Compiled into a GResource by `build.rs`; `src/icons.rs` registers it.

### Adwaita icon theme (CC BY-SA 3.0)

The GNOME Project's Adwaita symbolic icons, <https://gitlab.gnome.org/GNOME/adwaita-icon-theme>,
licensed under CC BY-SA 3.0 (<https://creativecommons.org/licenses/by-sa/3.0/>). Copied from the
installed theme and normalised for GTK's SVG renderer (groups, style and font attributes removed,
an identity `transform` added, see the rendering note); the shapes are unchanged. Adapted files
remain under CC BY-SA 3.0. Files in `scalable/actions/`:
action-unavailable, applications-engineering, chat-message-new, dialog-error, dialog-information,
dialog-question, dialog-warning, document-edit, document-open-recent, document-properties,
edit-copy, edit-undo, folder, format-text-italic, go-bottom, go-down, go-next, go-up, list-add,
media-playback-stop, network-server, object-select, pan-down, pan-end, pan-up, security-high,
security-medium, sidebar-show, system-search, system-users, utilities-terminal, view-dual,
view-list-bullet, view-refresh, window-close, x-office-document (all `*-symbolic.svg`).

### GNOME Icon Development Kit (CC0 1.0)

<https://gitlab.gnome.org/Teams/Design/icon-development-kit> was surveyed for glyphs. Its sources
are stroke-based with `gpa:` attributes that GTK older than 4.20 cannot recolour, so no file is
copied from it; the Adwaita files above are its filled, GTK-compatible export. Credit: Jakub
Steiner and the GNOME Design Team.

### Simple Icons (CC0 1.0)

`scalable/apps/agent-claude-symbolic.svg` (`claude.svg`) and
`scalable/apps/agent-agy-symbolic.svg` (`googlegemini.svg`), from
<https://github.com/simple-icons/simple-icons>, scaled from 24x24 to 16x16. Brand marks remain
trademarks of their owners; used only to identify the agent the user connects to.

### Original artwork (no licence needed)

Drawn for agent-terminal: `agent-thinking`, `agent-mcp`, `agent-subagent`, `agent-handoff`,
`agent-compact`, `agent-usage`, `agent-worktree`, `agent-checkpoint`, `agent-external` (in
`scalable/actions/`), the Codex monogram `agent-codex` and the symbolic app icon
`com.jdesroches.AgentTerminal-symbolic` (in `scalable/apps/`). The Codex monogram is not
OpenAI's logo; a user may place their own mark at `$XDG_DATA_HOME/agent-terminal/brand/codex.svg`
and it is used instead (never committed).

### Rendering note

GTK 4.22 converts a symbolic SVG that its node parser accepts into a render node that comes out
as a blocky upscale of a 16 px raster at large sizes (an `AdwStatusPage` icon at 128 px). A
`transform` attribute on the path makes that parser refuse the file ("Failed to convert ...
attribute 'transform' is invalid"), so GTK falls back to its pixbuf SVG loader, which renders
the vector at the requested size. Every bundled path therefore carries
`transform="translate(0 0)"`; `tests/icons.rs` enforces it. The "Failed to convert" lines under
`GTK_DEBUG=icontheme` are expected.
