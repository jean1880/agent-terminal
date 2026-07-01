//! The Antigravity terminal color theme.
//!
//! Built once per thread (GTK runs single-threaded) and reused for every tab,
//! using the infallible [`RGBA::new`] rather than parsing color strings at
//! runtime — so applying the theme can never panic and costs nothing per tab.

use gtk4::gdk::RGBA;
use vte4::prelude::*;
use vte4::Terminal;

thread_local! {
    static ANTIGRAVITY: Theme = Theme::build();
}

/// Converts 8-bit sRGB components to a fully opaque [`RGBA`].
fn rgb(r: u8, g: u8, b: u8) -> RGBA {
    RGBA::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        1.0,
    )
}

/// A resolved terminal color scheme: foreground/background, cursor, selection,
/// and the 16-color ANSI palette.
pub struct Theme {
    foreground: RGBA,
    background: RGBA,
    bold: RGBA,
    cursor: RGBA,
    highlight_bg: RGBA,
    highlight_fg: RGBA,
    palette: [RGBA; 16],
}

impl Theme {
    /// Applies the shared Antigravity theme to a terminal.
    pub fn apply(terminal: &Terminal) {
        ANTIGRAVITY.with(|theme| theme.apply_to(terminal));
    }

    fn build() -> Self {
        let foreground = rgb(200, 200, 255);
        let bold = rgb(180, 155, 255); // accent violet, reused for cursor + magenta

        // Harmonious pastel 16-color ANSI palette. Index 5 (magenta) reuses the
        // accent violet; index 7 (white) reuses the foreground.
        let palette = [
            rgb(45, 40, 62),    // 0  black (dark grey-violet)
            rgb(255, 120, 120), // 1  red
            rgb(78, 232, 176),  // 2  green
            rgb(255, 224, 102), // 3  yellow
            rgb(116, 192, 252), // 4  blue
            bold,               // 5  magenta
            rgb(102, 217, 232), // 6  cyan
            foreground,         // 7  white
            rgb(170, 162, 185), // 8  bright black
            rgb(255, 135, 135), // 9  bright red
            rgb(99, 241, 195),  // 10 bright green
            rgb(255, 236, 153), // 11 bright yellow
            rgb(165, 216, 255), // 12 bright blue
            rgb(208, 191, 255), // 13 bright magenta
            rgb(154, 230, 242), // 14 bright cyan
            rgb(230, 230, 255), // 15 bright white
        ];

        Self {
            foreground,
            background: rgb(24, 20, 37),
            bold,
            cursor: bold,
            highlight_bg: rgb(68, 58, 94),
            highlight_fg: rgb(230, 230, 255),
            palette,
        }
    }

    fn apply_to(&self, terminal: &Terminal) {
        let palette: [&RGBA; 16] = std::array::from_fn(|i| &self.palette[i]);
        terminal.set_colors(Some(&self.foreground), Some(&self.background), &palette);
        terminal.set_color_bold(Some(&self.bold));
        terminal.set_color_cursor(Some(&self.cursor));
        terminal.set_color_highlight(Some(&self.highlight_bg));
        terminal.set_color_highlight_foreground(Some(&self.highlight_fg));
    }
}
