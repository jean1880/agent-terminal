//! Bundled icons.
//!
//! Every icon the app draws ships inside the binary as a GResource (compiled by `build.rs`
//! from `assets/icons/`), so rendering never depends on the system icon theme or its
//! `icon-theme.cache`. A stale cache that lacks one symbolic name makes GTK skip the vector
//! icon and stretch a 16 px raster of a fallback name instead; bundling removes that failure
//! for every name. `tests/icons.rs` fails if a `"…-symbolic"` literal in `src/` has no file.

// Constants name bundled glyphs ahead of the buttons and rows that will use them.
#![allow(dead_code)]

use agent_core::adapter::Driver;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use tracing::{debug, warn};

/// Resource prefix the icon files are bundled under (`scalable/<context>/<name>.svg`).
pub const RESOURCE_PATH: &str = "/com/jdesroches/AgentTerminal/icons";

/// The theme the app resolves names from; it holds only what `assets/icons` bundles.
const BUNDLED_THEME: &str = "hicolor";

pub const CLAUDE_ICON: &str = "agent-claude-symbolic";
pub const AGY_ICON: &str = "agent-agy-symbolic";
pub const CODEX_ICON: &str = "agent-codex-symbolic";
pub const THINKING_ICON: &str = "agent-thinking-symbolic";
pub const MCP_ICON: &str = "agent-mcp-symbolic";
pub const SUBAGENT_ICON: &str = "agent-subagent-symbolic";
pub const APP_ICON: &str = "com.jdesroches.AgentTerminal-symbolic";
// Original glyphs for features whose buttons and rows are still being wired.
pub const HANDOFF_ICON: &str = "agent-handoff-symbolic";
pub const COMPACT_ICON: &str = "agent-compact-symbolic";
pub const USAGE_ICON: &str = "agent-usage-symbolic";
pub const WORKTREE_ICON: &str = "agent-worktree-symbolic";
pub const CHECKPOINT_ICON: &str = "agent-checkpoint-symbolic";
pub const EXTERNAL_ICON: &str = "agent-external-symbolic";

/// The icons this app owns (brand marks and original glyphs). Code takes these names from
/// the constants above; the test proves each has a file. Every other name is a plain literal
/// at its use site, covered by the source scan in `tests/icons.rs`.
#[allow(dead_code)] // read by tests/icons.rs; the list is the contract, not runtime data
pub const ICONS: &[&str] = &[
    HANDOFF_ICON,
    COMPACT_ICON,
    USAGE_ICON,
    WORKTREE_ICON,
    CHECKPOINT_ICON,
    EXTERNAL_ICON,
    CLAUDE_ICON,
    AGY_ICON,
    CODEX_ICON,
    THINKING_ICON,
    MCP_ICON,
    SUBAGENT_ICON,
    APP_ICON,
];

/// The brand mark for the agent a thread drives.
#[allow(dead_code)] // consumed by the thread rows and chat header once they show a mark
pub fn brand_icon(driver: Driver) -> &'static str {
    match driver {
        Driver::Claude => CLAUDE_ICON,
        Driver::Agy => AGY_ICON,
    }
}

/// The Codex mark: the user's own file if they dropped one at
/// `$XDG_DATA_HOME/agent-terminal/brand/codex.svg`, else the bundled original monogram.
///
/// Ceiling: a file override is shown as-is (a `FileIcon`), not recoloured to the text colour
/// as a symbolic themed icon is. Upgrade path: copy it into a private icon-theme dir.
#[allow(dead_code)] // consumed by the profile rows once Codex profiles are shown
pub fn codex_icon() -> gio::Icon {
    let custom = glib::user_data_dir().join("agent-terminal/brand/codex.svg");
    if custom.is_file() {
        gio::FileIcon::new(&gio::File::for_path(custom)).upcast()
    } else {
        gio::ThemedIcon::new(CODEX_ICON).upcast()
    }
}

/// Registers the bundled icons with the default display's icon theme. Call once, after GTK
/// is initialised. Returns whether the theme now has them; a failure is logged, not fatal,
/// since the app still draws (with whatever the system theme resolves).
pub fn register() -> bool {
    if let Err(err) = gio::resources_register_include!("icons.gresource") {
        warn!("Could not register the bundled icon resource: {err}");
        return false;
    }
    let Some(display) = gdk::Display::default() else {
        warn!("No display; bundled icons not added to an icon theme");
        return false;
    };
    let theme = gtk4::IconTheme::for_display(&display);
    theme.add_resource_path(RESOURCE_PATH);
    // The system theme outranks any resource path for a name it also has, and GTK renders its
    // symbolic icons from a 16 px raster at large sizes (blocky StatusPage heroes) and drops
    // files its SVG parser rejects. Resolving from hicolor + our resources + the toolkits' own
    // bundled icons takes the system theme and its cache out of the picture. This only affects
    // this process's theme object, not the user's setting.
    //
    // Set through GtkSettings (process-local, never persisted): the theme object follows that
    // setting and would revert a direct `set_theme_name`.
    gtk4::Settings::for_display(&display).set_gtk_icon_theme_name(Some(BUNDLED_THEME));
    debug!("Bundled icons registered under {RESOURCE_PATH}");
    true
}
