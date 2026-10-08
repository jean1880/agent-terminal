//! Bundled icons.
//!
//! Every icon the app draws ships inside the binary as a GResource (compiled by `build.rs`
//! from `assets/icons/`), so rendering never depends on the system icon theme or its cache.
//! Bundled copies of system icons carry an `at-` prefix so the resource wins with no name
//! conflict; Adwaita stays the theme and remains the fallback for anything not bundled.
//! `tests/icons.rs` fails if a `"…-symbolic"` literal in `src/` has no file.
//!
//! Small icons (buttons, rows, cards, up to 32 px) go through the normal GTK icon path. GTK 4.22
//! renders a symbolic icon at 48 px and above as a blocky upscale of a 16 px raster, so large
//! "hero" icons (status pages) are rasterised from the SVG at the exact device size instead:
//! see [`hero_paintable`].

// Constants name bundled glyphs ahead of the buttons and rows that will use them.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};

use agent_core::adapter::Driver;
use gtk4::gdk::subclass::prelude::*;
use gtk4::prelude::*;
use gtk4::{gdk, gdk_pixbuf, gio, glib};
use tracing::{debug, warn};

/// Resource prefix the icon files are bundled under (`scalable/<context>/<name>.svg`).
pub const RESOURCE_PATH: &str = "/com/jdesroches/AgentTerminal/icons";

/// The full-colour app icon, bundled as a resource for hero use (see `build.rs`).
pub const APP_ART: &str = "/com/jdesroches/AgentTerminal/art/com.jdesroches.AgentTerminal.svg";
/// [`APP_ART`] without its three cursor dots, which the startup splash draws (and animates) as
/// widgets of their own.
pub const APP_ART_BARE: &str =
    "/com/jdesroches/AgentTerminal/art/com.jdesroches.AgentTerminal.svg#bare";
/// Marks a hero source as [`APP_ART`]-style art with its cursor dots removed.
const BARE_SUFFIX: &str = "#bare";

/// The colour symbolic heroes are drawn in (the chrome's label colour; the app is dark-only).
const HERO_TINT: &str = "#c8c8ff";
/// The fill the bundled symbolic SVGs are authored with, replaced by [`HERO_TINT`].
const SVG_INK: &str = "#2e3436";

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
/// at its use site (`at-…` copies of Adwaita glyphs), covered by the source scan in
/// `tests/icons.rs`.
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
pub fn brand_icon(driver: Driver) -> &'static str {
    driver.info().brand_icon
}

/// The brand mark as a `gio::Icon`: Codex honours the user's override file ([`codex_icon`]),
/// the others are the bundled themed icon.
pub fn driver_icon(driver: Driver) -> gio::Icon {
    match driver {
        Driver::Codex => codex_icon(),
        other => gio::ThemedIcon::new(brand_icon(other)).upcast(),
    }
}

/// The Codex mark: the user's own file if they dropped one at
/// `$XDG_DATA_HOME/agent-terminal/brand/codex.svg`, else the bundled original monogram.
///
/// Ceiling: a file override is shown as-is (a `FileIcon`), not recoloured to the text colour
/// as a symbolic themed icon is. Upgrade path: copy it into a private icon-theme dir.
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
    gtk4::IconTheme::for_display(&display).add_resource_path(RESOURCE_PATH);
    debug!("Bundled icons registered under {RESOURCE_PATH}");
    true
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct HeroPaintable {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub logical_px: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for HeroPaintable {
        const NAME: &'static str = "AgentTerminalHeroPaintable";
        type Type = super::HeroPaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for HeroPaintable {}

    impl PaintableImpl for HeroPaintable {
        fn flags(&self) -> gdk::PaintableFlags {
            gdk::PaintableFlags::STATIC_SIZE
        }
        fn intrinsic_width(&self) -> i32 {
            self.logical_px.get()
        }
        fn intrinsic_height(&self) -> i32 {
            self.logical_px.get()
        }
        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            let (Some(texture), Some(snapshot)) = (
                self.texture.borrow().clone(),
                snapshot.downcast_ref::<gtk4::Snapshot>(),
            ) else {
                return;
            };
            snapshot.append_texture(
                &texture,
                &gtk4::graphene::Rect::new(0.0, 0.0, width as f32, height as f32),
            );
        }
    }
}

glib::wrapper! {
    /// A square paintable that shows an SVG rasterised at exactly its device size.
    pub struct HeroPaintable(ObjectSubclass<imp::HeroPaintable>) @implements gdk::Paintable;
}

/// The SVG for `source`: a resource path (full colour, as authored) or a bundled symbolic icon
/// name (drawn in [`HERO_TINT`]).
fn hero_svg(source: &str) -> Option<glib::Bytes> {
    let lookup = |path: &str| gio::resources_lookup_data(path, gio::ResourceLookupFlags::NONE).ok();
    if let Some(path) = source.strip_suffix(BARE_SUFFIX) {
        let bare = without_cursor_dots(&String::from_utf8_lossy(&lookup(path)?));
        return Some(glib::Bytes::from_owned(bare.into_bytes()));
    }
    if source.starts_with('/') {
        return lookup(source);
    }
    ["actions", "apps"].iter().find_map(|context| {
        let bytes = lookup(&format!("{RESOURCE_PATH}/scalable/{context}/{source}.svg"))?;
        let svg = String::from_utf8_lossy(&bytes).replace(SVG_INK, HERO_TINT);
        Some(glib::Bytes::from_owned(svg.into_bytes()))
    })
}

/// `svg` with every `<circle …/>` element removed: in the app icon those are exactly the three
/// cursor dots (the guard test below holds it to that).
fn without_cursor_dots(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(start) = rest.find("<circle") {
        out.push_str(&rest[..start]);
        match rest[start..].find("/>") {
            Some(end) => rest = &rest[start + end + 2..],
            None => {
                // Malformed: keep the remainder as authored rather than cut it.
                rest = &rest[start..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

impl HeroPaintable {
    /// Rasterises `source` at `logical_px * scale` device pixels.
    fn render(&self, source: &str, scale: i32) {
        let imp = self.imp();
        let device = (imp.logical_px.get() * scale.max(1)).max(1);
        let texture = hero_svg(source).and_then(|bytes| {
            let stream = gio::MemoryInputStream::from_bytes(&bytes);
            gdk_pixbuf::Pixbuf::from_stream_at_scale(
                &stream,
                device,
                device,
                true,
                gio::Cancellable::NONE,
            )
            .map_err(|err| warn!("Could not rasterise {source}: {err}"))
            .ok()
            .map(|pixbuf| gdk::Texture::for_pixbuf(&pixbuf))
        });
        imp.texture.replace(texture);
        self.invalidate_contents();
    }
}

/// A crisp paintable for a large icon (48 px and up). `source` is a resource path (the
/// full-colour [`APP_ART`], say) or a bundled symbolic icon name. It is rasterised at
/// `logical_px * widget.scale_factor()` and again whenever the widget's scale factor changes.
pub fn hero_paintable(
    source: &str,
    logical_px: i32,
    widget: &impl IsA<gtk4::Widget>,
) -> gdk::Paintable {
    let hero: HeroPaintable = glib::Object::new();
    hero.imp().logical_px.set(logical_px);
    hero.render(source, widget.scale_factor());
    let source = source.to_owned();
    widget.connect_notify_local(Some("scale-factor"), {
        let hero = hero.clone();
        move |widget, _| hero.render(&source, widget.scale_factor())
    });
    hero.upcast()
}

/// Sets a status page's icon from a bundled symbolic name, drawn crisp at the page's icon size
/// (128 px, or 64 px for a `compact` page). Call it after any `compact` class is added.
pub fn set_status_icon(page: &adw::StatusPage, source: &str) {
    let px = if page.has_css_class("compact") {
        64
    } else {
        128
    };
    page.set_paintable(Some(&hero_paintable(source, px, page)));
}

#[cfg(test)]
mod tests {
    use super::*;

    const ART: &str = include_str!("../assets/com.jdesroches.AgentTerminal.svg");

    /// The splash lays its dots over the bare art at the authored centres (see
    /// `window::loading`): if the icon's cursor changes, so must the splash.
    #[test]
    fn bare_art_drops_exactly_the_cursor_dots() {
        assert_eq!(
            ART.matches("<circle").count(),
            3,
            "the icon has three cursor dots"
        );
        for (cx, fill) in [(64, "#e8846b"), (80, "#5b9cf6"), (96, "#4cc38a")] {
            assert!(
                ART.contains(&format!(
                    "<circle cx=\"{cx}\" cy=\"66\" r=\"5.5\" fill=\"{fill}\"/>"
                )),
                "dot at {cx} moved or recoloured"
            );
        }
        let bare = without_cursor_dots(ART);
        assert!(!bare.contains("<circle"));
        assert!(bare.contains("<polyline"), "the prompt chevron stays");
        assert!(bare.trim_end().ends_with("</svg>"));
        assert_eq!(
            without_cursor_dots("a<circle"),
            "a<circle",
            "malformed input is kept"
        );
    }
}
