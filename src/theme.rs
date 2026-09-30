//! Terminal color schemes.
//!
//! Each [`Theme`] is built from infallible [`RGBA::new`] values (no runtime
//! string parsing, so applying a theme can never panic). Themes are cheap to
//! build, so one is constructed on demand each time it is applied to a tab.

use crate::config::ThemeChoice;
use gtk4::gdk::RGBA;
use vte4::prelude::*;
use vte4::Terminal;

/// Converts 8-bit sRGB components to a fully opaque [`RGBA`].
fn rgb(r: u8, g: u8, b: u8) -> RGBA {
    RGBA::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        1.0,
    )
}

/// Converts a `0xRRGGBB` hex color to a fully opaque [`RGBA`].
fn hex(color: u32) -> RGBA {
    rgb(
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
    )
}

/// Colours for the diff panel: the terminal's own background and text, and a
/// foreground for each kind of diff line.
pub struct DiffColours {
    pub background: RGBA,
    pub text: RGBA,
    pub added: RGBA,
    pub removed: RGBA,
    pub hunk: RGBA,
    pub meta: RGBA,
    pub file: RGBA,
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
    /// Builds the requested theme and applies it to a terminal.
    pub fn apply(terminal: &Terminal, choice: ThemeChoice) {
        Self::for_choice(choice).apply_to(terminal);
    }

    fn for_choice(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::Antigravity => Self::antigravity(),
            ThemeChoice::Dracula => Self::scheme(
                0x282a36,
                0xf8f8f2,
                0xf8f8f2,
                0x44475a,
                [
                    0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
                    0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
                ],
            ),
            ThemeChoice::Nord => Self::scheme(
                0x2e3440,
                0xd8dee9,
                0xd8dee9,
                0x434c5e,
                [
                    0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                    0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
                ],
            ),
            ThemeChoice::GruvboxDark => Self::scheme(
                0x282828,
                0xebdbb2,
                0xebdbb2,
                0x504945,
                [
                    0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                    0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
                ],
            ),
            ThemeChoice::SolarizedDark => Self::scheme(
                0x002b36,
                0x839496,
                0x93a1a1,
                0x073642,
                [
                    0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                    0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
                ],
            ),
            ThemeChoice::OneDark => Self::scheme(
                0x282c34,
                0xabb2bf,
                0x528bff,
                0x3e4451,
                [
                    0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
                    0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
                ],
            ),
            ThemeChoice::Monokai => Self::scheme(
                0x272822,
                0xf8f8f2,
                0xf8f8f0,
                0x49483e,
                [
                    0x272822, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf8f8f2,
                    0x75715e, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf9f8f5,
                ],
            ),
        }
    }

    /// Builds a theme from hex colors. `bold` and the selection foreground both
    /// use `fg`, which suits the standard schemes below.
    fn scheme(bg: u32, fg: u32, cursor: u32, selection: u32, palette: [u32; 16]) -> Self {
        Self {
            foreground: hex(fg),
            background: hex(bg),
            bold: hex(fg),
            cursor: hex(cursor),
            highlight_bg: hex(selection),
            highlight_fg: hex(fg),
            palette: palette.map(hex),
        }
    }

    /// The hand-tuned Antigravity brand theme (the default).
    fn antigravity() -> Self {
        let foreground = rgb(200, 200, 255);
        let bold = rgb(180, 155, 255); // accent violet, reused for cursor + magenta

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

    /// Colours for the diff panel, taken from the theme's own palette so it
    /// matches the terminal beside it.
    pub fn diff_colours(choice: ThemeChoice) -> DiffColours {
        let theme = Self::for_choice(choice);
        DiffColours {
            background: theme.background,
            text: theme.foreground,
            added: theme.palette[2],
            removed: theme.palette[1],
            hunk: theme.palette[6],
            meta: theme.palette[8],
            file: theme.bold,
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
