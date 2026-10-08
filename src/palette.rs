//! The app-wide colours, from the theme chosen in Settings.
//!
//! Every stylesheet (the brand chrome in `main.rs`, the chat view's, the loading screens') names
//! its colours (`@at_bg`, `@at_fg_dim`, `@at_accent`…) instead of writing them out; this module
//! defines those names for the selected [`ThemeChoice`] in one CSS provider, together with the
//! libadwaita variables (`--window-bg-color`…) and the legacy named colours older libadwaita
//! reads, and swaps it when the theme changes. So the theme restyles the whole window, not just
//! the terminals.
//!
//! The app's own theme keeps its hand-tuned values exactly. Every other bundled theme derives the
//! same tokens from its terminal palette: background, foreground, and its red, green, yellow,
//! blue and magenta (the accent). The agents' brand colours (Claude, agy, Codex) are not themed:
//! they identify the agent.
//!
//! [`ThemeChoice::System`] has no values of its own. Each name is an alias of a colour the
//! desktop's GTK theme defines ([`SYSTEM_COLOURS`]), libadwaita's own colours are left alone and
//! the colour scheme is not forced, so light or dark, the accent colour and any `gtk.css` all come
//! through. What Rust code needs as values (terminal and diff colours, Pango markup) is read
//! back from the resolved style, again whenever the desktop's style changes.

use std::cell::{Cell, RefCell};

use gtk4::gdk;
use gtk4::prelude::*;

use crate::config::ThemeChoice;
use crate::theme::Theme;

/// An opaque sRGB colour, components 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    pub const WHITE: Rgb = Rgb(1.0, 1.0, 1.0);

    pub fn from_hex(hex: u32) -> Rgb {
        let c = |shift: u32| f64::from((hex >> shift) & 0xff) / 255.0;
        Rgb(c(16), c(8), c(0))
    }

    fn from_rgba(c: &gdk::RGBA) -> Rgb {
        Rgb(
            f64::from(c.red()),
            f64::from(c.green()),
            f64::from(c.blue()),
        )
    }

    /// `c` drawn over `under`: what a translucent colour (Adwaita's text is one) looks like.
    fn over(c: &gdk::RGBA, under: Rgb) -> Rgb {
        under.mix(Rgb::from_rgba(c), f64::from(c.alpha()))
    }

    pub fn to_rgba(self) -> gdk::RGBA {
        gdk::RGBA::new(self.0 as f32, self.1 as f32, self.2 as f32, 1.0)
    }

    /// `self` moved `t` (0..=1) of the way to `other`.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let m = |a: f64, b: f64| a + (b - a) * t;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    /// Darker (`f` < 1) or lighter (`f` > 1), clamped.
    pub fn shade(self, f: f64) -> Rgb {
        let s = |a: f64| (a * f).clamp(0.0, 1.0);
        Rgb(s(self.0), s(self.1), s(self.2))
    }

    pub fn hex(self) -> String {
        let b = |a: f64| (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("#{:02x}{:02x}{:02x}", b(self.0), b(self.1), b(self.2))
    }

    /// `rgba(…)` at `alpha`, for the CSS variables that want a translucent colour.
    pub fn rgba(self, alpha: f64) -> String {
        let b = |a: f64| (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("rgba({}, {}, {}, {alpha})", b(self.0), b(self.1), b(self.2))
    }
}

/// Every named colour the stylesheets use, by role.
#[derive(Debug, Clone, PartialEq)]
pub struct Chrome {
    /// The window and chat background.
    pub bg: Rgb,
    /// Header bars, the sidebar, code blocks: a step darker.
    pub bg_deep: Rgb,
    /// Wells a little below the background (diff and output panes).
    pub bg_sunken: Rgb,
    /// Cards, popovers, the composer.
    pub surface: Rgb,
    /// Hovered or nested surfaces.
    pub surface_raised: Rgb,
    /// Fields and chips set into a surface.
    pub surface_strong: Rgb,
    /// A surface tinted with the accent (selection, the user's own messages).
    pub accent_surface: Rgb,
    pub border: Rgb,
    pub border_strong: Rgb,
    pub border_hover: Rgb,
    /// Titles and the text you type.
    pub fg_strong: Rgb,
    /// Body text.
    pub fg: Rgb,
    /// Secondary text (folder names, labels).
    pub fg_soft: Rgb,
    /// Captions and hints.
    pub fg_dim: Rgb,
    /// The faintest text (timestamps, placeholders).
    pub fg_faint: Rgb,
    pub accent: Rgb,
    pub accent_hover: Rgb,
    pub success: Rgb,
    pub success_soft: Rgb,
    pub warning: Rgb,
    pub warning_soft: Rgb,
    pub danger: Rgb,
    pub danger_soft: Rgb,
    /// Text on a danger background.
    pub danger_text: Rgb,
    pub danger_bg: Rgb,
    pub danger_border: Rgb,
    /// Background work, links.
    pub info: Rgb,
}

/// The system theme: each name, as an expression of the desktop theme's named colours (the ones
/// libadwaita documents, which a `gtk.css` theme redefines). Mixes and alphas only place a role
/// between two of the desktop's colours; none brings a colour of its own. Same names, same
/// order as [`Chrome::named`].
pub const SYSTEM_COLOURS: [(&str, &str); 27] = [
    ("at_bg", "@window_bg_color"),
    ("at_bg_deep", "@headerbar_bg_color"),
    ("at_bg_sunken", "@view_bg_color"),
    ("at_surface", "@card_bg_color"),
    (
        "at_surface_raised",
        "mix(@window_bg_color, @window_fg_color, 0.08)",
    ),
    (
        "at_surface_strong",
        "mix(@window_bg_color, @accent_bg_color, 0.14)",
    ),
    (
        "at_accent_surface",
        "mix(@window_bg_color, @accent_bg_color, 0.28)",
    ),
    ("at_border", "alpha(@window_fg_color, 0.15)"),
    ("at_border_strong", "alpha(@window_fg_color, 0.25)"),
    ("at_border_hover", "alpha(@accent_color, 0.6)"),
    ("at_fg_strong", "@window_fg_color"),
    ("at_fg", "@window_fg_color"),
    ("at_fg_soft", "alpha(@window_fg_color, 0.85)"),
    ("at_fg_dim", "alpha(@window_fg_color, 0.65)"),
    ("at_fg_faint", "alpha(@window_fg_color, 0.45)"),
    ("at_accent", "@accent_color"),
    (
        "at_accent_hover",
        "mix(@accent_color, @window_fg_color, 0.15)",
    ),
    ("at_success", "@success_color"),
    (
        "at_success_soft",
        "mix(@success_color, @window_fg_color, 0.25)",
    ),
    ("at_warning", "@warning_color"),
    (
        "at_warning_soft",
        "mix(@warning_color, @window_fg_color, 0.25)",
    ),
    ("at_danger", "@error_color"),
    ("at_danger_soft", "mix(@error_color, @window_fg_color, 0.2)"),
    ("at_danger_text", "mix(@error_color, @window_fg_color, 0.5)"),
    (
        "at_danger_bg",
        "mix(@window_bg_color, @error_bg_color, 0.14)",
    ),
    (
        "at_danger_border",
        "mix(@window_bg_color, @error_color, 0.42)",
    ),
    ("at_info", "@accent_color"),
];

/// The class a probe label takes to be coloured `name`, so its value can be read back.
fn probe_class(name: &str) -> String {
    format!("at-probe-{}", name.replace('_', "-"))
}

/// The system theme's stylesheet: the [`SYSTEM_COLOURS`] aliases, and a probe rule per name
/// ([`resolve_system`] reads them). Nothing of libadwaita's is redefined.
fn system_css() -> String {
    let mut css = String::new();
    for (name, value) in SYSTEM_COLOURS {
        css.push_str(&format!("@define-color {name} {value};\n"));
    }
    for (name, _) in SYSTEM_COLOURS {
        css.push_str(&format!(
            "label.{} {{ color: @{name}; }}\n",
            probe_class(name)
        ));
    }
    css
}

/// The system theme's colours as they now resolve: each name read from a probe label's style.
/// Translucent ones are composited over the background, as they are drawn.
fn resolve_system() -> Chrome {
    let read = |name: &str| {
        let probe = gtk4::Label::new(None);
        probe.add_css_class(&probe_class(name));
        probe.color()
    };
    let bg = Rgb::from_rgba(&read("at_bg"));
    let c = |name: &str| Rgb::over(&read(name), bg);
    Chrome {
        bg,
        bg_deep: c("at_bg_deep"),
        bg_sunken: c("at_bg_sunken"),
        surface: c("at_surface"),
        surface_raised: c("at_surface_raised"),
        surface_strong: c("at_surface_strong"),
        accent_surface: c("at_accent_surface"),
        border: c("at_border"),
        border_strong: c("at_border_strong"),
        border_hover: c("at_border_hover"),
        fg_strong: c("at_fg_strong"),
        fg: c("at_fg"),
        fg_soft: c("at_fg_soft"),
        fg_dim: c("at_fg_dim"),
        fg_faint: c("at_fg_faint"),
        accent: c("at_accent"),
        accent_hover: c("at_accent_hover"),
        success: c("at_success"),
        success_soft: c("at_success_soft"),
        warning: c("at_warning"),
        warning_soft: c("at_warning_soft"),
        danger: c("at_danger"),
        danger_soft: c("at_danger_soft"),
        danger_text: c("at_danger_text"),
        danger_bg: c("at_danger_bg"),
        danger_border: c("at_danger_border"),
        info: c("at_info"),
    }
}

impl Chrome {
    /// The colours of a bundled palette; `None` for the system theme, whose colours are the
    /// desktop's (see [`resolve_system`]).
    pub fn for_choice(choice: ThemeChoice) -> Option<Chrome> {
        match choice {
            ThemeChoice::System => None,
            ThemeChoice::AgentTerminal => Some(Self::agent_terminal()),
            other => Theme::chrome_base(other).map(|base| {
                Self::derived(
                    Rgb::from_rgba(&base.background),
                    Rgb::from_rgba(&base.foreground),
                    Rgb::from_rgba(&base.accent),
                    [
                        Rgb::from_rgba(&base.green),
                        Rgb::from_rgba(&base.yellow),
                        Rgb::from_rgba(&base.red),
                        Rgb::from_rgba(&base.blue),
                    ],
                )
            }),
        }
    }

    /// The app's own palette, as hand-tuned (the values the stylesheets were written with).
    fn agent_terminal() -> Chrome {
        let h = Rgb::from_hex;
        Chrome {
            bg: h(0x181425),
            bg_deep: h(0x120f1d),
            bg_sunken: h(0x171226),
            surface: h(0x1d1830),
            surface_raised: h(0x241d38),
            surface_strong: h(0x2a2344),
            accent_surface: h(0x3a2d6b),
            border: h(0x2d2444),
            border_strong: h(0x3b3160),
            border_hover: h(0x52467f),
            fg_strong: h(0xebe9ff),
            fg: h(0xc8c8ff),
            fg_soft: h(0xa8a2dc),
            fg_dim: h(0x918bbd),
            fg_faint: h(0x6d6794),
            accent: h(0xb49bff),
            accent_hover: h(0xc4b0ff),
            success: h(0x4ee8b0),
            success_soft: h(0x8ff3cd),
            warning: h(0xffc46b),
            warning_soft: h(0xffd08a),
            danger: h(0xff7878),
            danger_soft: h(0xff9a9a),
            danger_text: h(0xffc4c4),
            danger_bg: h(0x3a1f2b),
            danger_border: h(0x7a3b4c),
            info: h(0x7cc7ff),
        }
    }

    /// Every token from a theme's background, foreground and accent, and its success, warning,
    /// danger and info colours (green, yellow, red, blue). The proportions reproduce the app's
    /// own palette from its own base colours, near enough.
    pub fn derived(
        bg: Rgb,
        fg: Rgb,
        accent: Rgb,
        [success, warning, danger, info]: [Rgb; 4],
    ) -> Chrome {
        Chrome {
            bg,
            bg_deep: bg.shade(0.75),
            bg_sunken: bg.shade(0.9),
            surface: bg.mix(fg, 0.035),
            surface_raised: bg.mix(fg, 0.07),
            surface_strong: bg.mix(accent, 0.12),
            accent_surface: bg.mix(accent, 0.25),
            border: bg.mix(accent, 0.14),
            border_strong: bg.mix(accent, 0.24),
            border_hover: bg.mix(accent, 0.4),
            fg_strong: fg.mix(Rgb::WHITE, 0.55),
            fg,
            fg_soft: fg.mix(bg, 0.18),
            fg_dim: fg.mix(bg, 0.32),
            fg_faint: fg.mix(bg, 0.52),
            accent,
            accent_hover: accent.mix(Rgb::WHITE, 0.2),
            success,
            success_soft: success.mix(Rgb::WHITE, 0.35),
            warning,
            warning_soft: warning.mix(Rgb::WHITE, 0.25),
            danger,
            danger_soft: danger.mix(Rgb::WHITE, 0.2),
            danger_text: danger.mix(Rgb::WHITE, 0.6),
            danger_bg: bg.mix(danger, 0.14),
            danger_border: bg.mix(danger, 0.42),
            info,
        }
    }

    /// The names the stylesheets use, and their colours.
    fn named(&self) -> [(&'static str, Rgb); 27] {
        [
            ("at_bg", self.bg),
            ("at_bg_deep", self.bg_deep),
            ("at_bg_sunken", self.bg_sunken),
            ("at_surface", self.surface),
            ("at_surface_raised", self.surface_raised),
            ("at_surface_strong", self.surface_strong),
            ("at_accent_surface", self.accent_surface),
            ("at_border", self.border),
            ("at_border_strong", self.border_strong),
            ("at_border_hover", self.border_hover),
            ("at_fg_strong", self.fg_strong),
            ("at_fg", self.fg),
            ("at_fg_soft", self.fg_soft),
            ("at_fg_dim", self.fg_dim),
            ("at_fg_faint", self.fg_faint),
            ("at_accent", self.accent),
            ("at_accent_hover", self.accent_hover),
            ("at_success", self.success),
            ("at_success_soft", self.success_soft),
            ("at_warning", self.warning),
            ("at_warning_soft", self.warning_soft),
            ("at_danger", self.danger),
            ("at_danger_soft", self.danger_soft),
            ("at_danger_text", self.danger_text),
            ("at_danger_bg", self.danger_bg),
            ("at_danger_border", self.danger_border),
            ("at_info", self.info),
        ]
    }

    /// The stylesheet that defines every name, and maps libadwaita's own colours onto them.
    pub fn css(&self) -> String {
        let mut css = String::new();
        for (name, colour) in self.named() {
            css.push_str(&format!("@define-color {name} {};\n", colour.hex()));
        }
        // libadwaita before 1.6 reads these names; 1.6 and later the variables below.
        let legacy = [
            ("accent_color", self.accent),
            ("accent_bg_color", self.accent),
            ("accent_fg_color", self.bg),
            ("window_bg_color", self.bg),
            ("window_fg_color", self.fg),
            ("view_bg_color", self.bg),
            ("view_fg_color", self.fg),
            ("headerbar_bg_color", self.bg_deep),
            ("headerbar_fg_color", self.fg),
            ("sidebar_bg_color", self.bg_deep),
            ("dialog_bg_color", self.surface),
            ("dialog_fg_color", self.fg),
            ("popover_bg_color", self.surface),
            ("popover_fg_color", self.fg),
        ];
        for (name, colour) in legacy {
            css.push_str(&format!("@define-color {name} {};\n", colour.hex()));
        }
        css.push_str(":root {\n");
        for (name, colour) in legacy {
            css.push_str(&format!(
                "    --{}: {};\n",
                name.replace('_', "-"),
                colour.hex()
            ));
        }
        css.push_str(&format!("    --card-bg-color: {};\n", self.fg.rgba(0.05)));
        css.push_str("}\n");
        css
    }
}

thread_local! {
    static PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
    static CURRENT: RefCell<Option<Chrome>> = const { RefCell::new(None) };
    /// Whether the system theme is the one applied, so a desktop change re-resolves it.
    static SYSTEM_ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// Whether the desktop's style is being watched (connected once per process).
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    /// Whether a re-resolve is queued: one desktop switch notifies several properties at once,
    /// and they share one refresh.
    static REFRESH_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Recolours what holds colours as values (terminals, diff panels) after the system
    /// theme re-resolved. Set by the app.
    static ON_SYSTEM_CHANGE: Cell<Option<fn()>> = const { Cell::new(None) };
}

/// The colours in use (the app's own until [`apply`] first runs).
pub fn current() -> Chrome {
    CURRENT
        .with(|c| c.borrow().clone())
        .unwrap_or_else(Chrome::agent_terminal)
}

/// What to run after the system theme follows a change of the desktop's style: recolour what
/// took its colours as values.
pub fn set_on_system_change(f: fn()) {
    ON_SYSTEM_CHANGE.with(|slot| slot.set(Some(f)));
}

fn set_current(chrome: Chrome) {
    crate::icons::set_hero_tint(chrome.fg.hex());
    CURRENT.with(|c| *c.borrow_mut() = Some(chrome));
}

/// Restyles the whole app for `choice`: defines every named colour for it, replacing the last
/// theme's. Above the app's own stylesheets, so the names resolve however they load.
pub fn apply(choice: ThemeChoice) {
    let system = choice == ThemeChoice::System;
    SYSTEM_ACTIVE.with(|s| s.set(system));
    let Some(chrome) = Chrome::for_choice(choice) else {
        apply_system();
        return;
    };
    let css = chrome.css();
    set_current(chrome);
    if gdk::Display::default().is_none() {
        return;
    }
    // The palettes are dark: popovers, menus and entries follow them instead of the light
    // default (their text would otherwise be pale on white).
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    load(&css);
}

/// The system theme: the desktop's colour scheme, the aliases, and the colours they resolve to.
fn apply_system() {
    if gdk::Display::default().is_none() {
        set_current(Chrome::agent_terminal());
        return;
    }
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
    load(&system_css());
    set_current(resolve_system());
    watch_system();
}

/// Re-resolves the system theme when the desktop's style changes: dark or light, accent,
/// contrast, or the GTK theme. On idle, once the new stylesheet is in.
fn watch_system() {
    if WATCHING.with(|w| w.replace(true)) {
        return;
    }
    let refresh = || {
        if REFRESH_PENDING.with(|p| p.replace(true)) {
            return;
        }
        gtk4::glib::idle_add_local_once(|| {
            REFRESH_PENDING.with(|p| p.set(false));
            if !SYSTEM_ACTIVE.with(Cell::get) {
                return;
            }
            set_current(resolve_system());
            if let Some(f) = ON_SYSTEM_CHANGE.with(Cell::get) {
                f();
            }
        });
    };
    let style = adw::StyleManager::default();
    // `accent-color` exists from libadwaita 1.6; on older ones it never notifies, and the
    // accent never changes either.
    for property in ["dark", "accent-color", "high-contrast"] {
        style.connect_notify_local(Some(property), move |_, _| refresh());
    }
    if let Some(settings) = gtk4::Settings::default() {
        for property in ["gtk-theme-name", "gtk-application-prefer-dark-theme"] {
            settings.connect_notify_local(Some(property), move |_, _| refresh());
        }
    }
}

/// Replaces the theme stylesheet with `css`.
fn load(css: &str) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    PROVIDER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let provider = slot.get_or_insert_with(|| {
            let provider = gtk4::CssProvider::new();
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
            );
            provider
        });
        provider.load_from_data(css);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every theme defines every name: a stylesheet naming one a theme lacks would draw it
    /// transparent (or not at all).
    #[test]
    fn every_theme_defines_every_name() {
        for choice in ThemeChoice::ALL {
            let Some(chrome) = Chrome::for_choice(choice) else {
                assert_eq!(
                    choice,
                    ThemeChoice::System,
                    "only the system theme has no palette"
                );
                continue;
            };
            let css = chrome.css();
            for (name, _) in chrome.named() {
                assert!(
                    css.contains(&format!("@define-color {name} #")),
                    "{choice}: {name}"
                );
            }
            assert!(
                css.contains("--window-bg-color:"),
                "{choice}: libadwaita variables"
            );
        }
    }

    /// The system theme defines the same names, each from the desktop's colours only, and
    /// leaves libadwaita's own colours alone so the desktop's theme shows through.
    #[test]
    fn the_system_theme_only_aliases_the_desktops_colours() {
        let names: Vec<&str> = Chrome::agent_terminal()
            .named()
            .iter()
            .map(|(n, _)| *n)
            .collect();
        let system: Vec<&str> = SYSTEM_COLOURS.iter().map(|(n, _)| *n).collect();
        assert_eq!(system, names);
        for (name, value) in SYSTEM_COLOURS {
            assert!(!value.contains('#'), "{name}: a colour of its own");
            assert!(value.contains('@'), "{name}: not from the desktop");
        }
        let css = system_css();
        assert!(!css.contains("define-color window_bg_color"));
        assert!(!css.contains("--window-bg-color"));
        assert!(css.contains("label.at-probe-at-bg { color: @at_bg; }"));
    }

    /// Every `@at_*` name a stylesheet uses is one the palette defines: GTK drops a property
    /// whose colour does not resolve, so a typo would silently unstyle it.
    #[test]
    fn the_stylesheets_name_only_defined_colours() {
        let defined: Vec<&str> = Chrome::agent_terminal()
            .named()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        let sheets = [
            ("main.rs", include_str!("main.rs")),
            ("style.css", include_str!("chat/view/style.css")),
            ("loading.css", include_str!("window/loading.css")),
        ];
        for (sheet, text) in sheets {
            for (at, _) in text.match_indices("@at_") {
                let name: String = text[at + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                assert!(defined.contains(&name.as_str()), "{sheet}: @{name}");
            }
        }
    }

    /// The app's own theme is exactly what the stylesheets were tuned with.
    #[test]
    fn the_default_theme_keeps_its_values() {
        let css = Chrome::agent_terminal().css();
        assert!(css.contains("@define-color at_bg #181425;"));
        assert!(css.contains("@define-color at_accent #b49bff;"));
        assert!(css.contains("@define-color at_fg_dim #918bbd;"));
    }

    /// A derived theme reads as its terminal does: its background, its text, its accent, and a
    /// darker header than body.
    #[test]
    fn a_derived_theme_follows_its_terminal() {
        let c = Chrome::for_choice(ThemeChoice::Dracula).expect("a bundled palette");
        assert_eq!(c.bg.hex(), "#282a36");
        assert_eq!(c.fg.hex(), "#f8f8f2");
        assert!(c.bg_deep.0 < c.bg.0 && c.bg_deep.2 < c.bg.2);
        assert_ne!(c.accent, c.bg);
    }

    #[test]
    fn colour_maths() {
        let black = Rgb(0.0, 0.0, 0.0);
        assert_eq!(black.mix(Rgb::WHITE, 0.5).hex(), "#808080");
        assert_eq!(Rgb::from_hex(0x336699).hex(), "#336699");
        assert_eq!(Rgb::WHITE.shade(2.0).hex(), "#ffffff", "clamped");
        assert_eq!(Rgb::from_hex(0xff0000).rgba(0.5), "rgba(255, 0, 0, 0.5)");
    }
}
