//! Minimal stub implementation of terminal types when the `terminal` feature (VTE4) is disabled.
//!
//! Provides a `gtk4::Box`-derived [`Terminal`] GObject and dummy VTE types so that the application
//! compiles cleanly on macOS and other platforms where VTE4 is unavailable.

use gtk4::gdk::RGBA;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

pub mod prelude {
    // Empty prelude for stub compatibility
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorBlinkMode {
    System,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Ibeam,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Html,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyFlags(u32);

impl PtyFlags {
    pub const DEFAULT: Self = PtyFlags(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteFlags(u32);

impl WriteFlags {
    #[allow(non_upper_case_globals)]
    pub const Default: Self = WriteFlags(0);
}

#[derive(Debug, Clone)]
pub struct Regex;

impl Regex {
    pub fn for_search(_pattern: &str, _flags: u32) -> Result<Self, glib::Error> {
        Ok(Regex)
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Terminal;

    #[glib::object_subclass]
    impl ObjectSubclass for Terminal {
        const NAME: &'static str = "AgentTerminalStubTerminal";
        type Type = super::Terminal;
        type ParentType = gtk4::Box;
    }

    impl ObjectImpl for Terminal {}
    impl WidgetImpl for Terminal {}
    impl BoxImpl for Terminal {}
}

glib::wrapper! {
    pub struct Terminal(ObjectSubclass<imp::Terminal>)
        @extends gtk4::Box, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Orientable;
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

impl Terminal {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_font(&self, _font: Option<&gtk4::pango::FontDescription>) {}

    pub fn set_cursor_shape(&self, _shape: CursorShape) {}

    pub fn set_cursor_blink_mode(&self, _mode: CursorBlinkMode) {}

    pub fn set_scrollback_lines(&self, _lines: i64) {}

    pub fn set_font_scale(&self, _scale: f64) {}

    pub fn set_enable_sixel(&self, _enable: bool) {}

    pub fn set_allow_hyperlink(&self, _allow: bool) {}

    pub fn search_set_wrap_around(&self, _wrap: bool) {}

    pub fn search_set_regex(&self, _regex: Option<&Regex>, _flags: u32) {}

    pub fn search_find_next(&self) {}

    pub fn search_find_previous(&self) {}

    pub fn window_title(&self) -> Option<glib::GString> {
        None
    }

    pub fn hyperlink_hover_uri(&self) -> Option<glib::GString> {
        None
    }

    pub fn row_count(&self) -> i64 {
        0
    }

    pub fn column_count(&self) -> i64 {
        0
    }

    pub fn cursor_position(&self) -> (i64, i64) {
        (0, 0)
    }

    pub fn text_range_format(
        &self,
        _format: Format,
        _start_row: i64,
        _start_col: i64,
        _end_row: i64,
        _end_col: i64,
    ) -> (Option<glib::GString>, glib::GString) {
        (None, glib::GString::from(""))
    }

    pub fn write_contents_sync(
        &self,
        _stream: &gtk4::gio::MemoryOutputStream,
        _flags: WriteFlags,
        _cancellable: Option<&gtk4::gio::Cancellable>,
    ) -> Result<(), glib::Error> {
        Ok(())
    }

    pub fn copy_clipboard_format(&self, _format: Format) {}

    pub fn paste_clipboard(&self) {}

    pub fn feed_child(&self, _data: &[u8]) {}

    pub fn connect_contents_changed<F: Fn(&Self) + 'static>(&self, _f: F) -> glib::SignalHandlerId {
        self.connect_notify_local(None, move |_, _| {})
    }

    pub fn connect_window_title_changed<F: Fn(&Self) + 'static>(
        &self,
        _f: F,
    ) -> glib::SignalHandlerId {
        self.connect_notify_local(None, move |_, _| {})
    }

    pub fn connect_bell<F: Fn(&Self) + 'static>(&self, _f: F) -> glib::SignalHandlerId {
        self.connect_notify_local(None, move |_, _| {})
    }

    pub fn connect_child_exited<F: Fn(&Self, i32) + 'static>(
        &self,
        _f: F,
    ) -> glib::SignalHandlerId {
        self.connect_notify_local(None, move |_, _| {})
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_async<F1, F2>(
        &self,
        _pty_flags: PtyFlags,
        _working_directory: Option<&str>,
        _argv: &[&str],
        _envv: &[&str],
        _spawn_flags: glib::SpawnFlags,
        _child_setup: F1,
        _timeout: i32,
        _cancellable: Option<&gtk4::gio::Cancellable>,
        callback: F2,
    ) where
        F1: FnOnce() + 'static,
        F2: FnOnce(Result<glib::Pid, glib::Error>) + 'static,
    {
        let err = glib::Error::new(
            glib::FileError::Noent,
            "Terminal support is disabled on this platform",
        );
        callback(Err(err));
    }

    pub fn set_colors(
        &self,
        _foreground: Option<&RGBA>,
        _background: Option<&RGBA>,
        _palette: &[&RGBA],
    ) {
    }

    pub fn set_color_bold(&self, _color: Option<&RGBA>) {}

    pub fn set_color_cursor(&self, _color: Option<&RGBA>) {}

    pub fn set_color_highlight(&self, _color: Option<&RGBA>) {}

    pub fn set_color_highlight_foreground(&self, _color: Option<&RGBA>) {}
}
