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
installed theme, cleaned for GTK's SVG parser (groups, style and font attributes removed) and
renamed with an `at-` prefix (`at-go-up-symbolic.svg`) so the bundled copy never collides with the
system theme's name; the shapes are unchanged. Adapted files remain under CC BY-SA 3.0. Files in
`scalable/actions/` (each as `at-<name>-symbolic.svg`):
action-unavailable, applications-engineering, chat-message-new, dialog-error, dialog-information,
dialog-question, dialog-warning, document-edit, document-open-recent, document-properties,
edit-copy, edit-undo, folder, format-text-italic, go-bottom, go-down, go-next, go-up, list-add,
media-playback-stop, network-server, object-select, pan-down, pan-end, pan-up, security-high,
security-medium, sidebar-show, system-search, system-users, utilities-terminal, view-dual,
view-list-bullet, view-refresh, window-close, x-office-document.

### GNOME Icon Development Kit (CC0 1.0)

<https://gitlab.gnome.org/Teams/Design/icon-development-kit> was surveyed for glyphs. Its sources
are stroke-based with `gpa:` attributes that GTK older than 4.20 cannot recolour, so no file is
copied from it; the Adwaita files above are its filled, GTK-compatible export. Credit: Jakub
Steiner and the GNOME Design Team.

### Simple Icons (CC0 1.0)

`scalable/apps/agent-claude-symbolic.svg` (`claude.svg`) and
`scalable/apps/agent-agy-symbolic.svg` (`googlegemini.svg`), from
<https://github.com/simple-icons/simple-icons>, drawn at 16x16 through a 24x24 viewBox. Brand marks remain
trademarks of their owners; used only to identify the agent the user connects to.

### Original artwork (no licence needed)

Drawn for agent-terminal: `agent-thinking`, `agent-mcp`, `agent-subagent`, `agent-handoff`,
`agent-compact`, `agent-usage`, `agent-worktree`, `agent-checkpoint`, `agent-external` (in
`scalable/actions/`), the Codex monogram `agent-codex` and the symbolic app icon
`ca.nuvek.AgentTerminal-symbolic` (in `scalable/apps/`). The Codex monogram is not
OpenAI's logo; a user may place their own mark at `$XDG_DATA_HOME/agent-terminal/brand/codex.svg`
and it is used instead (never committed).

### Rendering note

GTK 4.22 renders a symbolic icon at 48 px and above (an `AdwStatusPage` icon at 128 px) as a
blocky upscale of a 16 px raster, even from a clean, parseable SVG (verified with Broadway
screenshots; the system Adwaita file and a bundled copy behave the same). Small icons (up to
32 px) are crisp through the normal path. Large "hero" icons therefore bypass it:
`icons::hero_paintable` rasterises the SVG with gdk-pixbuf at `logical size x scale factor`
(re-rendered when the scale factor changes). The empty state uses the full-colour app icon
(`assets/ca.nuvek.AgentTerminal.svg`, bundled under `/ca/nuvek/AgentTerminal/art`);
other status pages draw the bundled symbolic glyph tinted to the chrome's label colour.
Bundled SVGs stay clean: no `transform`, `style`, font attributes or groups
(`tests/icons.rs`).
