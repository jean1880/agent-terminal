//! The composer: a GtkSourceView prompt with a typeahead popover.
//!
//! # Typeahead: a custom popover, not `GtkSourceCompletion`
//!
//! GtkSourceCompletion was the plan's first choice and was rejected after trying it on paper
//! against what the composer needs:
//! - its providers re-filter and re-sort proposals themselves, while ranking must be
//!   `agent_core::commands` (T3's scoring, built-ins first, shadowing rules);
//! - choosing a built-in must *run* it locally and never insert text, which fights the
//!   proposal-activation model;
//! - `@file` replies arrive asynchronously as `ControlResult` envelopes keyed by request id,
//!   which needs the debounce and stale-reply cancellation done here anyway;
//! - each provider and proposal is a GObject subclass, which is a lot of ceremony for a list.
//!
//! So the popover is a plain `gtk4::Popover` holding a `ListBox`, never taking focus from the
//! text view, with all decisions (keys, insertion, stale replies) in the pure
//! [`super::typeahead`] module.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use agent_core::commands::{
    builtins, completion_items, detect_trigger, CompletionItem, CompletionKind, Trigger,
    TriggerKind,
};
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use sourceview5::prelude::*;

use super::cards::label;
use super::typeahead::{apply_completion, Key, KeyOutcome, LatestRequest, Popup};

/// Debounce for `@file` suggestions (a round trip to the agent CLI).
const FILE_DEBOUNCE: Duration = Duration::from_millis(120);

/// One row of the popup.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Command(CompletionItem),
    File(String),
}

/// What the composer asks of the view.
pub trait ComposerHost {
    /// Enter on a non-empty prompt (already trimmed). The host decides between a built-in and
    /// sending it.
    fn submit(&self, text: &str);
    fn interrupt(&self);
    /// Whether a turn is running (Esc interrupts, the button stops).
    fn running(&self) -> bool;
    /// A built-in was chosen from the popup.
    fn run_builtin(&self, name: &str);
    /// Candidates for a `/` or `$` trigger.
    fn command_items(&self, trigger: &Trigger) -> Vec<CompletionItem>;
    /// Asks for `@` suggestions; `None` when the agent cannot answer them.
    fn request_files(&self, query: &str) -> Option<String>;
    fn placeholder(&self) -> String;
}

pub struct Composer {
    root: gtk4::Box,
    view: sourceview5::View,
    buffer: sourceview5::Buffer,
    send: gtk4::Button,
    placeholder: gtk4::Label,
    popover: gtk4::Popover,
    pop_scroller: gtk4::ScrolledWindow,
    list: gtk4::ListBox,
    popup: RefCell<Popup>,
    entries: RefCell<Vec<Entry>>,
    trigger: RefCell<Option<Trigger>>,
    files: Rc<RefCell<LatestRequest>>,
    file_timer: Rc<RefCell<Option<glib::SourceId>>>,
    /// Suppresses re-triggering while the composer itself edits the buffer.
    editing: Cell<bool>,
    host: RefCell<Option<std::rc::Weak<dyn ComposerHost>>>,
}

impl Composer {
    pub fn new() -> Rc<Self> {
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        root.add_css_class("composer");

        let buffer = sourceview5::Buffer::new(None);
        buffer.set_highlight_matching_brackets(false);
        // Without the app's dark scheme, GtkSourceView's default draws its caret dark on our
        // dark background, so there is no visible cursor (the CSS caret-color backs this up).
        super::cards::apply_scheme(&buffer);
        let view = sourceview5::View::with_buffer(&buffer);
        view.set_wrap_mode(gtk4::WrapMode::WordChar);
        view.set_accepts_tab(false);
        view.set_top_margin(10);
        view.set_bottom_margin(10);
        view.set_left_margin(14);
        view.set_right_margin(14);
        view.add_css_class("composer-view");
        view.set_hexpand(true);

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(220);
        scroller.set_min_content_height(24);
        scroller.set_child(Some(&view));
        scroller.set_hexpand(true);
        // Clip to the field's rounded corners (its tint is drawn by the view, see style.css).
        scroller.set_overflow(gtk4::Overflow::Hidden);
        scroller.add_css_class("composer-field");

        let placeholder = label("", &["composer-placeholder"]);
        placeholder.set_can_target(false);
        placeholder.set_valign(gtk4::Align::Start);
        placeholder.set_margin_top(10);
        placeholder.set_margin_start(16);
        placeholder.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&scroller));
        overlay.add_overlay(&placeholder);
        overlay.set_hexpand(true);
        root.append(&overlay);

        let send = gtk4::Button::from_icon_name("at-go-up-symbolic");
        send.add_css_class("circular");
        send.add_css_class("send-button");
        send.set_valign(gtk4::Align::End);
        send.set_tooltip_text(Some("Send (Enter)"));
        root.append(&send);

        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Single);
        list.add_css_class("typeahead-list");
        list.set_can_focus(false);
        let pop_scroller = gtk4::ScrolledWindow::new();
        pop_scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        pop_scroller.set_propagate_natural_height(true);
        pop_scroller.set_max_content_height(300);
        // Width comes from the list: `min_content_width` is ignored under PolicyType::Never.
        list.set_size_request(480, -1);
        pop_scroller.set_child(Some(&list));
        let popover = gtk4::Popover::new();
        popover.set_autohide(false);
        popover.set_has_arrow(false);
        popover.set_can_focus(false);
        popover.set_position(gtk4::PositionType::Top);
        // Left edge on the trigger rather than centred on it.
        popover.set_halign(gtk4::Align::Start);
        popover.add_css_class("typeahead");
        popover.set_child(Some(&pop_scroller));
        popover.set_parent(&view);

        let composer = Rc::new(Self {
            root,
            view,
            buffer,
            send,
            placeholder,
            popover,
            pop_scroller,
            list,
            popup: RefCell::new(Popup::default()),
            entries: RefCell::new(Vec::new()),
            trigger: RefCell::new(None),
            files: Rc::new(RefCell::new(LatestRequest::default())),
            file_timer: Rc::new(RefCell::new(None)),
            editing: Cell::new(false),
            host: RefCell::new(None),
        });
        composer.connect_signals();
        composer
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.root
    }

    pub fn set_host(&self, host: std::rc::Weak<dyn ComposerHost>) {
        *self.host.borrow_mut() = Some(host);
        self.refresh_placeholder();
    }

    fn host(&self) -> Option<Rc<dyn ComposerHost>> {
        self.host.borrow().as_ref().and_then(std::rc::Weak::upgrade)
    }

    pub fn grab_focus(&self) {
        self.view.grab_focus();
    }

    /// The popover is parented to the text view and must be unparented before it goes.
    pub fn dispose(&self) {
        self.cancel_file_timer();
        if self.popover.parent().is_some() {
            self.popover.unparent();
        }
    }

    pub fn text(&self) -> String {
        let (s, e) = self.buffer.bounds();
        self.buffer.text(&s, &e, false).to_string()
    }

    pub fn set_text(&self, text: &str) {
        self.editing.set(true);
        self.buffer.set_text(text);
        self.editing.set(false);
        self.refresh_placeholder();
        self.refresh_trigger();
    }

    pub fn clear(&self) {
        self.set_text("");
        self.close_popup();
    }

    /// Send ⇄ stop.
    pub fn set_running(&self, running: bool) {
        if running {
            self.send.set_icon_name("at-media-playback-stop-symbolic");
            self.send.add_css_class("stop");
            self.send.set_tooltip_text(Some("Stop (Esc)"));
        } else {
            self.send.set_icon_name("at-go-up-symbolic");
            self.send.remove_css_class("stop");
            self.send.set_tooltip_text(Some("Send (Enter)"));
        }
    }

    pub fn refresh_placeholder(&self) {
        if let Some(host) = self.host() {
            // Only on a change: this runs after every batch of events.
            let text = host.placeholder();
            if self.placeholder.text() != text {
                self.placeholder.set_text(&text);
            }
        }
        self.placeholder.set_visible(self.buffer.char_count() == 0);
    }

    /// What the placeholder currently says.
    #[cfg(test)]
    pub fn placeholder_text(&self) -> String {
        self.placeholder.text().to_string()
    }

    fn connect_signals(self: &Rc<Self>) {
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, keyval, _, state| match weak.upgrade() {
            Some(c) => c.on_key(keyval, state),
            None => glib::Propagation::Proceed,
        });
        self.view.add_controller(keys);

        let weak = Rc::downgrade(self);
        self.buffer.connect_changed(move |_| {
            if let Some(c) = weak.upgrade() {
                c.refresh_placeholder();
                if !c.editing.get() {
                    c.refresh_trigger();
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.buffer.connect_cursor_position_notify(move |_| {
            if let Some(c) = weak.upgrade() {
                if !c.editing.get() {
                    c.refresh_trigger();
                }
            }
        });

        let weak = Rc::downgrade(self);
        self.send.connect_clicked(move |_| {
            let Some(c) = weak.upgrade() else {
                return;
            };
            let Some(host) = c.host() else {
                return;
            };
            if host.running() && c.text().trim().is_empty() {
                host.interrupt();
            } else {
                c.submit();
            }
            c.view.grab_focus();
        });

        let weak = Rc::downgrade(self);
        self.list.connect_row_activated(move |_, row| {
            if let Some(c) = weak.upgrade() {
                let index = usize::try_from(row.index()).unwrap_or(0);
                c.popup.borrow_mut().select(index);
                c.accept(index);
                c.view.grab_focus();
            }
        });

        // Losing focus (clicking elsewhere) closes the popup.
        let focus = gtk4::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(c) = weak.upgrade() {
                // Deferred: a click on a popup row moves focus before it activates.
                let weak = Rc::downgrade(&c);
                glib::timeout_add_local_once(Duration::from_millis(150), move || {
                    if let Some(c) = weak.upgrade() {
                        if !c.view.has_focus() {
                            c.close_popup();
                        }
                    }
                });
            }
        });
        self.view.add_controller(focus);
    }

    fn on_key(&self, keyval: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        let key = match keyval {
            gdk::Key::Up | gdk::Key::KP_Up => Key::Up,
            gdk::Key::Down | gdk::Key::KP_Down => Key::Down,
            gdk::Key::Tab | gdk::Key::ISO_Left_Tab => Key::Tab,
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => Key::Enter,
            gdk::Key::Escape => Key::Escape,
            _ => Key::Other,
        };
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        if !(shift || ctrl) {
            let token = self.current_token();
            let outcome = self.popup.borrow_mut().key(key, &token);
            match outcome {
                KeyOutcome::NotHandled => {}
                KeyOutcome::Moved(i) => {
                    self.select_row(i);
                    return glib::Propagation::Stop;
                }
                KeyOutcome::Accept(i) => {
                    self.accept(i);
                    return glib::Propagation::Stop;
                }
                KeyOutcome::Dismiss => {
                    self.popover.popdown();
                    self.files.borrow_mut().cancel();
                    return glib::Propagation::Stop;
                }
            }
        }
        match key {
            Key::Enter if !shift => {
                self.submit();
                glib::Propagation::Stop
            }
            Key::Escape => match self.host() {
                Some(host) if host.running() => {
                    host.interrupt();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            },
            _ => glib::Propagation::Proceed,
        }
    }

    fn submit(&self) {
        let text = self.text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        let owned = trimmed.to_owned();
        self.clear();
        host.submit(&owned);
    }

    fn cursor_offset(&self) -> (String, usize) {
        let text = self.text();
        let iter = self.buffer.iter_at_mark(&self.buffer.get_insert());
        // Char offset → byte offset.
        let chars = usize::try_from(iter.offset()).unwrap_or(0);
        let byte = text
            .char_indices()
            .nth(chars)
            .map_or(text.len(), |(b, _)| b);
        (text, byte)
    }

    fn current_token(&self) -> String {
        let (text, _) = self.cursor_offset();
        self.trigger
            .borrow()
            .as_ref()
            .and_then(|t| text.get(t.range.clone()).map(str::to_owned))
            .unwrap_or_default()
    }

    /// Re-detects the trigger under the cursor and refreshes the candidates.
    fn refresh_trigger(&self) {
        let (text, cursor) = self.cursor_offset();
        let trigger = detect_trigger(&text, cursor);
        let token = trigger
            .as_ref()
            .and_then(|t| text.get(t.range.clone()))
            .unwrap_or("")
            .to_owned();
        let same = *self.trigger.borrow() == trigger;
        *self.trigger.borrow_mut() = trigger.clone();
        let Some(trigger) = trigger else {
            self.close_popup();
            return;
        };
        if same && self.popover.is_visible() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        match trigger.kind {
            TriggerKind::Slash | TriggerKind::Skill => {
                self.files.borrow_mut().cancel();
                self.cancel_file_timer();
                let entries = host
                    .command_items(&trigger)
                    .into_iter()
                    .map(Entry::Command)
                    .collect();
                self.show_entries(entries, &token);
            }
            TriggerKind::File => {
                // Debounced: each keystroke restarts the timer and drops the in-flight reply.
                self.files.borrow_mut().cancel();
                self.cancel_file_timer();
                let query = trigger.query.clone();
                let host = Rc::downgrade(&host);
                let files = Rc::downgrade(&self.files);
                let timer = Rc::downgrade(&self.file_timer);
                let id = glib::timeout_add_local_once(FILE_DEBOUNCE, move || {
                    // The source is spent: forget its id so nothing removes it twice.
                    if let Some(timer) = timer.upgrade() {
                        timer.borrow_mut().take();
                    }
                    if let (Some(host), Some(files)) = (host.upgrade(), files.upgrade()) {
                        if let Some(id) = host.request_files(&query) {
                            files.borrow_mut().issue(id);
                        }
                    }
                });
                *self.file_timer.borrow_mut() = Some(id);
            }
        }
    }

    fn cancel_file_timer(&self) {
        if let Some(id) = self.file_timer.borrow_mut().take() {
            id.remove();
        }
    }

    /// A `ControlResult` for an `@file` request. Returns whether it was ours.
    pub fn file_reply(&self, request: &str, files: Vec<String>) -> bool {
        if !self.files.borrow_mut().accept(request) {
            return false;
        }
        let token = self.current_token();
        let still_file = matches!(&*self.trigger.borrow(), Some(t) if t.kind == TriggerKind::File);
        if still_file {
            self.show_entries(files.into_iter().map(Entry::File).collect(), &token);
        }
        true
    }

    pub fn is_waiting_for(&self, request: &str) -> bool {
        self.files.borrow().is_waiting_for(request)
    }

    fn show_entries(&self, entries: Vec<Entry>, token: &str) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        for e in &entries {
            self.list.append(&entry_row(e));
        }
        let open = self.popup.borrow_mut().set_items(entries.len(), token);
        *self.entries.borrow_mut() = entries;
        if open {
            self.select_row(0);
            self.place_popover();
            self.popover.popup();
        } else {
            self.popover.popdown();
        }
    }

    fn place_popover(&self) {
        let range_start = self.trigger.borrow().as_ref().map_or(0, |t| t.range.start);
        let text = self.text();
        let char_offset = text.get(..range_start).map_or(0, |s| s.chars().count());
        let iter = self
            .buffer
            .iter_at_offset(i32::try_from(char_offset).unwrap_or(0));
        let rect = self.view.iter_location(&iter);
        let (x, y) =
            self.view
                .buffer_to_window_coords(gtk4::TextWindowType::Widget, rect.x(), rect.y());
        self.popover
            .set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, rect.height().max(1))));
    }

    fn select_row(&self, i: usize) {
        let index = i32::try_from(i).unwrap_or(0);
        if let Some(row) = self.list.row_at_index(index) {
            self.list.select_row(Some(&row));
            // Keep the selected row in view without moving focus off the text view.
            if let Some(bounds) = row.compute_bounds(&self.list) {
                let adj = self.pop_scroller.vadjustment();
                let (y, h) = (f64::from(bounds.y()), f64::from(bounds.height()));
                if y < adj.value() {
                    adj.set_value(y);
                } else if y + h > adj.value() + adj.page_size() {
                    adj.set_value(y + h - adj.page_size());
                }
            }
        }
    }

    fn close_popup(&self) {
        self.popup.borrow_mut().close();
        self.popover.popdown();
        self.files.borrow_mut().cancel();
        self.cancel_file_timer();
    }

    fn accept(&self, index: usize) {
        let Some(entry) = self.entries.borrow().get(index).cloned() else {
            return;
        };
        let Some(trigger) = self.trigger.borrow().clone() else {
            return;
        };
        let (text, _) = self.cursor_offset();
        self.close_popup();
        match entry {
            Entry::Command(item) if item.kind == CompletionKind::Builtin => {
                let builtin = builtins().iter().find(|b| b.name == item.name);
                match builtin {
                    Some(b) if super::typeahead::runs_immediately(b) => {
                        // Run locally; the typed trigger goes away.
                        let (new, cursor) = apply_completion(&text, trigger.range.clone(), "");
                        self.replace(&new, cursor);
                        if let Some(host) = self.host() {
                            host.run_builtin(b.name);
                        }
                    }
                    _ => {
                        // Needs an argument: complete the name and let the user type it.
                        let (new, cursor) =
                            apply_completion(&text, trigger.range.clone(), &item.insert_text);
                        self.replace(&new, cursor);
                    }
                }
            }
            Entry::Command(item) => {
                let (new, cursor) =
                    apply_completion(&text, trigger.range.clone(), &item.insert_text);
                self.replace(&new, cursor);
            }
            Entry::File(path) => {
                let insert = format!("@{path} ");
                let (new, cursor) = apply_completion(&text, trigger.range.clone(), &insert);
                self.replace(&new, cursor);
            }
        }
    }

    fn replace(&self, text: &str, cursor_byte: usize) {
        self.editing.set(true);
        self.buffer.set_text(text);
        let chars = text.get(..cursor_byte).map_or(0, |s| s.chars().count());
        let iter = self
            .buffer
            .iter_at_offset(i32::try_from(chars).unwrap_or(i32::MAX));
        self.buffer.place_cursor(&iter);
        self.editing.set(false);
        *self.trigger.borrow_mut() = None;
        self.refresh_placeholder();
    }
}

fn entry_row(entry: &Entry) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.set_can_focus(false);
    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    b.add_css_class("typeahead-row");
    match entry {
        Entry::Command(item) => {
            let (badge, sigil) = match item.kind {
                CompletionKind::Builtin => ("app", "/"),
                CompletionKind::Agent => ("agent", "/"),
                CompletionKind::Skill => ("skill", "$"),
            };
            let badge = label(badge, &["kind-badge", &format!("badge-{badge}")]);
            badge.set_valign(gtk4::Align::Center);
            b.append(&badge);
            b.append(&label(
                &format!("{sigil}{}", item.name),
                &["typeahead-name"],
            ));
            if let Some(h) = &item.hint {
                b.append(&label(h, &["typeahead-hint"]));
            }
            let d = label(&item.description, &["typeahead-desc"]);
            d.set_hexpand(true);
            d.set_xalign(1.0);
            d.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            b.append(&d);
        }
        Entry::File(path) => {
            let badge = label("file", &["kind-badge", "badge-file"]);
            badge.set_valign(gtk4::Align::Center);
            b.append(&badge);
            let p = label(path, &["typeahead-name"]);
            p.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
            p.set_hexpand(true);
            b.append(&p);
        }
    }
    row.set_child(Some(&b));
    row
}

/// Candidates for a `/` or `$` trigger (wraps `completion_items` so the host stays small).
pub fn items_for(
    trigger: &Trigger,
    commands: &[agent_core::event::AgentCommand],
    caps: &agent_core::caps::Capabilities,
) -> Vec<CompletionItem> {
    completion_items(builtins(), commands, caps, trigger)
}
