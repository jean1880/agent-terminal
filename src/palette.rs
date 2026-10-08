//! The app-wide colours, from the theme chosen in Settings.
//!
//! Every stylesheet (the brand chrome in `main.rs`, the chat view's, the loading screens') names
//! its colours (`@at_bg`, `@at_fg_dim`, `@at_accent`…) instead of writing them out; this module
//! defines those names for the selected [`ThemeChoice`] in one CSS provider, together with the
//! libadwaita variables (`--window-bg-color`…) and the legacy named colours older libadwaita
//! reads, and swaps it when the theme changes. So the theme restyles the whole window, not just
//! the terminals.
//!
//! The app's own theme keeps its hand-tuned values exactly. Every other theme derives the same
//! tokens from its terminal palette: background, foreground, and its red, green, yellow, blue
//! and magenta (the accent). The agents' brand colours (Claude, agy, Codex) are not themed:
//! they identify the agent.

use std::cell::RefCell;

use gtk4::gdk;

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

impl Chrome {
    /// The colours for `choice`.
    pub fn for_choice(choice: ThemeChoice) -> Chrome {
        match choice {
            ThemeChoice::AgentTerminal => Self::agent_terminal(),
            other => {
                let base = Theme::chrome_base(other);
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
            }
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
}

/// The colours in use (the app's own until [`apply`] first runs).
pub fn current() -> Chrome {
    CURRENT
        .with(|c| c.borrow().clone())
        .unwrap_or_else(Chrome::agent_terminal)
}

/// Restyles the whole app for `choice`: defines every named colour for it, replacing the last
/// theme's. Above the app's own stylesheets, so the names resolve however they load.
pub fn apply(choice: ThemeChoice) {
    let chrome = Chrome::for_choice(choice);
    let css = chrome.css();
    crate::icons::set_hero_tint(chrome.fg.hex());
    CURRENT.with(|c| *c.borrow_mut() = Some(chrome));
    let Some(display) = gdk::Display::default() else {
        return;
    };
    // The palettes are dark: popovers, menus and entries follow them instead of the light
    // default (their text would otherwise be pale on white).
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
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
        provider.load_from_data(&css);
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
            let css = Chrome::for_choice(choice).css();
            for (name, _) in Chrome::for_choice(choice).named() {
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
        let css = Chrome::for_choice(ThemeChoice::AgentTerminal).css();
        assert!(css.contains("@define-color at_bg #181425;"));
        assert!(css.contains("@define-color at_accent #b49bff;"));
        assert!(css.contains("@define-color at_fg_dim #918bbd;"));
    }

    /// A derived theme reads as its terminal does: its background, its text, its accent, and a
    /// darker header than body.
    #[test]
    fn a_derived_theme_follows_its_terminal() {
        let c = Chrome::for_choice(ThemeChoice::Dracula);
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
