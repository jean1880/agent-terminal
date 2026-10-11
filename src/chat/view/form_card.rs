//! The card for a form (or a link) an agent's tool server asks the user for.
//!
//! The server's words are quoted under a "<server> tool server" chip, never set in the app's or
//! the agent's voice. A field the user has not touched and that has no default is not answered
//! (see [`Draft`]), so Submit never sends a value nobody chose. The card judges an answer with
//! `form::answer`, the check the adapter makes again before sending. Nothing typed is kept: the
//! card sends it and the transcript stores only the outcome.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use agent_core::form::{self, FieldInput, Form, FormAction, FormField, TextFormat};
use gtk4::glib;
use gtk4::prelude::*;
use serde_json::Value;

use super::cards::{
    add_button, button_row, content_label, label, mono, wrapping, RowEvent, RowSink,
};
use super::form_draft::{choose_hint, number_value, Draft};
use super::model::{FormCard, FormState};

/// Options up to this many show as buttons; more become a dropdown.
const BUTTON_OPTIONS: usize = 6;

/// How a link is handed to the desktop: the URL, the window to attach to, and a callback with
/// the outcome. Replaced in tests so none opens a browser.
pub(super) type Launcher =
    Rc<dyn Fn(&str, Option<gtk4::Window>, Box<dyn FnOnce(Result<(), String>)>)>;

fn desktop_launcher() -> Launcher {
    Rc::new(|url, parent, done| {
        gtk4::UriLauncher::new(url).launch(
            parent.as_ref(),
            gtk4::gio::Cancellable::NONE,
            move |r| {
                done(r.map_err(|e| e.to_string()));
            },
        );
    })
}

/// One field's widgets.
struct FieldUi {
    root: gtk4::Box,
    error: gtk4::Label,
    /// The control that takes keyboard focus first.
    focus: gtk4::Widget,
    /// Controls that read as invalid while the error shows.
    marks: Vec<gtk4::Widget>,
    /// What the controls announce besides their name (the field's description).
    described: String,
    /// The user has been through the field (left it, or tried to submit): its error may show.
    visited: Cell<bool>,
}

struct Inner {
    root: gtk4::Box,
    request: String,
    form: Form,
    sink: RowSink,
    draft: RefCell<Draft>,
    fields: Vec<FieldUi>,
    body: gtk4::Box,
    status: gtk4::Label,
    sent_to: gtk4::Label,
    buttons: gtk4::FlowBox,
    submit: Option<gtk4::Button>,
    open: Option<gtk4::Button>,
    done: Option<gtk4::Button>,
    copy: Option<gtk4::Button>,
    opened: gtk4::Label,
    open_failed: gtk4::Label,
    /// The agent did not accept the last answer (shown above the buttons).
    refusal: gtk4::Label,
    state: Cell<FormState>,
    used_open: Cell<bool>,
    launcher: RefCell<Launcher>,
}

pub struct FormCardWidget {
    inner: Rc<Inner>,
}

/// Shows or hides a button of a [`button_row`], with the wrapper that would leave a gap.
fn show_in_row(button: &gtk4::Button, shown: bool) {
    button.set_visible(shown);
    if let Some(wrapper) = button.parent() {
        wrapper.set_visible(shown);
    }
}

fn heading_of(form: &Form) -> &'static str {
    match form {
        Form::Link { .. } => "A tool server wants you to open a link",
        _ => "A tool server wants information",
    }
}

impl FormCardWidget {
    pub fn new(card: &FormCard, sink: &RowSink) -> Self {
        let inner = Rc::new_cyclic(|weak: &Weak<Inner>| Inner::build(card, sink, weak));
        Self { inner }
    }

    pub fn root(&self) -> &gtk4::Box {
        &self.inner.root
    }

    pub fn update(&self, card: &FormCard) {
        self.inner.update(card);
    }

    /// Takes keyboard focus to the first field (or, for a link or a form with no fields, the
    /// first button): the user asked to be taken to this card. Never called on its own.
    pub fn focus_first(&self) -> bool {
        let inner = &self.inner;
        if !matches!(inner.state.get(), FormState::Pending) {
            return false;
        }
        if let Some(field) = inner.fields.first() {
            return field.focus.grab_focus();
        }
        inner.buttons.child_focus(gtk4::DirectionType::TabForward)
    }

    /// Tests may replace how links are opened.
    #[cfg(test)]
    pub(super) fn set_launcher(&self, launcher: Launcher) {
        *self.inner.launcher.borrow_mut() = launcher;
    }
}

impl Inner {
    fn build(card: &FormCard, sink: &RowSink, weak: &Weak<Inner>) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        root.add_css_class("form-card");
        root.add_css_class("question-card");
        root.add_css_class("card");

        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        let icon = gtk4::Image::from_icon_name("at-network-server-symbolic");
        icon.set_valign(gtk4::Align::Start);
        head.append(&icon);
        let heading = label(heading_of(&card.form), &["question-heading"]);
        wrapping(&heading);
        head.append(&heading);
        root.append(&head);

        // Whose words follow: the tool server's, quoted.
        let chip = label(&format!("{} tool server", card.server), &["question-chip"]);
        chip.set_halign(gtk4::Align::Start);
        chip.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        chip.set_max_width_chars(48);
        chip.set_tooltip_text(Some(&format!("{} tool server", card.server)));
        root.append(&chip);
        let message = content_label(card.message.trim(), &["md-quote", "form-message"]);
        wrapping(&message);
        message.set_visible(!card.message.trim().is_empty());
        message.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Message from the {} tool server",
            card.server
        ))]);
        root.append(&message);

        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        body.add_css_class("form-body");
        let mut fields = Vec::new();
        let mut draft = Draft::new(&[]);
        match &card.form {
            Form::Fields { fields: specs } => {
                draft = Draft::new(specs);
                for (index, spec) in specs.iter().enumerate() {
                    let last = index + 1 == specs.len();
                    let ui = build_field(index, spec, &draft.initial(index), weak, last);
                    body.append(&ui.root);
                    fields.push(ui);
                }
            }
            Form::Link { url } => build_link(url, &body),
            Form::Unknown => {}
        }
        root.append(&body);

        let opened = label(
            "Opened in your browser. Choose Done when you have finished.",
            &["form-note"],
        );
        wrapping(&opened);
        opened.set_visible(false);
        let open_failed = label(
            "Could not open your browser. Copy the link and open it yourself.",
            &["form-note", "form-error"],
        );
        wrapping(&open_failed);
        open_failed.set_visible(false);
        if matches!(card.form, Form::Link { .. }) {
            root.append(&opened);
            root.append(&open_failed);
        }

        // The reason or outcome above the buttons: the buttons wrap, and a wrapping label
        // beside a wrapping row is where GTK's height-for-width measuring disagrees with itself.
        let footer = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        // Text typed here goes to a tool server: say, in the app's own words, what never to type.
        if let Form::Fields { fields: specs } = &card.form {
            if specs
                .iter()
                .any(|f| matches!(f.input, FieldInput::Text { .. }))
            {
                let warn = label(
                    "agent-terminal never needs a password, key or code here. Don't paste one.",
                    &["form-note", "form-secret-note"],
                );
                wrapping(&warn);
                footer.append(&warn);
            }
        }
        let refusal = label(
            "Codex did not accept that answer. Try again, or decline.",
            &["form-error"],
        );
        wrapping(&refusal);
        refusal.set_visible(card.refused);
        footer.append(&refusal);
        let status = label("", &["approval-outcome"]);
        wrapping(&status);
        status.set_visible(false);
        let buttons = button_row();
        footer.append(&status);
        footer.append(&buttons);
        root.append(&footer);
        let sent_to = label(
            &format!("Sent to {}.", card.server),
            &["form-note", "dim-label"],
        );
        sent_to.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        sent_to.set_halign(gtk4::Align::End);
        root.append(&sent_to);

        let pill = |text: &str, classes: &[&str], tip: &str| {
            let b = gtk4::Button::with_label(text);
            b.add_css_class("pill");
            for c in classes {
                b.add_css_class(c);
            }
            b.set_tooltip_text(Some(tip));
            b
        };
        let submit = matches!(card.form, Form::Fields { .. }).then(|| {
            pill(
                "Submit",
                &["suggested-action"],
                "Send these answers to the tool server",
            )
        });
        let open = matches!(card.form, Form::Link { .. }).then(|| {
            pill(
                "Open in Browser",
                &["suggested-action"],
                "Open the link in your default browser",
            )
        });
        let done = matches!(card.form, Form::Link { .. }).then(|| {
            pill(
                "Done",
                &[],
                "Tell the tool server you have finished in the browser",
            )
        });
        let copy = matches!(card.form, Form::Link { .. })
            .then(|| pill("Copy Link", &[], "Copy the link to the clipboard"));
        let decline = pill(
            "Decline",
            &[],
            "Tell the tool server you will not answer; the agent carries on",
        );
        // No second way to say no: Decline is the answer, and the composer's Stop ends the turn.
        // Tab order: the fields above, then these, left to right.
        for b in [&open, &submit, &done].into_iter().flatten() {
            add_button(&buttons, b);
        }
        if let Some(copy) = &copy {
            add_button(&buttons, copy);
            show_in_row(copy, false);
        }
        add_button(&buttons, &decline);
        if let Some(copy) = &copy {
            if let Form::Link { url } = &card.form {
                let url = url.clone();
                super::cards::wire_copy(copy, move || url.clone());
            }
        }

        let wire_action = |button: &gtk4::Button, action: FormAction| {
            let weak = weak.clone();
            button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.act(action);
                }
            });
        };
        wire_action(&decline, FormAction::Decline);
        if let Some(done) = &done {
            wire_action(done, FormAction::Accept);
        }
        if let Some(submit) = &submit {
            let weak = weak.clone();
            submit.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.attempt();
                }
            });
        }
        if let Some(open) = &open {
            let weak = weak.clone();
            open.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.open_link();
                }
            });
        }

        let inner = Self {
            root,
            request: card.request.clone(),
            form: card.form.clone(),
            sink: sink.clone(),
            draft: RefCell::new(draft),
            fields,
            body,
            status,
            sent_to,
            buttons,
            submit,
            open,
            done,
            copy,
            opened,
            open_failed,
            refusal,
            state: Cell::new(card.state),
            used_open: Cell::new(false),
            launcher: RefCell::new(desktop_launcher()),
        };
        inner.refresh();
        inner.show_state(card.state);
        inner
    }

    /// The user pressed Decline or Done.
    fn act(&self, action: FormAction) {
        if self.state.get() != FormState::Pending {
            return;
        }
        (self.sink)(RowEvent::Form {
            request: self.request.clone(),
            action,
            content: None,
        });
    }

    /// Submit, or Enter in the last field: sends only a valid answer. An invalid one marks
    /// every field and takes the user to the first that needs them.
    fn attempt(&self) {
        if self.state.get() != FormState::Pending {
            return;
        }
        let checked = self.draft.borrow().check();
        match checked {
            Ok(sent) => (self.sink)(RowEvent::Form {
                request: self.request.clone(),
                action: FormAction::Accept,
                content: Some(Value::Object(sent)),
            }),
            Err(errors) => {
                for field in &self.fields {
                    field.visited.set(true);
                }
                self.refresh();
                let first = match (&self.form, errors.first()) {
                    (Form::Fields { fields }, Some(e)) => {
                        fields.iter().position(|f| f.name == e.name)
                    }
                    _ => None,
                };
                if let Some(ui) = first.and_then(|i| self.fields.get(i)) {
                    ui.focus.grab_focus();
                }
            }
        }
    }

    /// Open in Browser: only a link that still passes the host check, handed to the desktop.
    fn open_link(self: &Rc<Self>) {
        let Form::Link { url } = &self.form else {
            return;
        };
        if self.state.get() != FormState::Pending || form::link_host(url).is_none() {
            return;
        }
        let parent = self.root.root().and_downcast::<gtk4::Window>();
        let weak = Rc::downgrade(self);
        let launcher = self.launcher.borrow().clone();
        launcher(
            url,
            parent,
            Box::new(move |result| {
                let Some(inner) = weak.upgrade() else { return };
                let opened = result.is_ok();
                if let Err(error) = &result {
                    tracing::warn!(%error, "form card: could not open the link");
                }
                if opened {
                    inner.used_open.set(true);
                }
                inner.opened.set_visible(opened);
                inner.open_failed.set_visible(!opened);
                if let Some(copy) = &inner.copy {
                    show_in_row(copy, !opened);
                }
                inner.refresh();
            }),
        );
    }

    /// Re-judges the draft: field errors where the user has been, Submit and its reason.
    fn refresh(&self) {
        let draft = self.draft.borrow();
        for (index, field) in self.fields.iter().enumerate() {
            let error = if field.visited.get() {
                draft.error(index)
            } else {
                None
            };
            let note = if draft.left_out_unknown(index) {
                Some("This optional field was left out.".to_owned())
            } else {
                None
            };
            let shown = error.clone().or(note);
            field.error.set_text(shown.as_deref().unwrap_or(""));
            field.error.set_visible(shown.is_some());
            field.error.set_css_classes(&if error.is_some() {
                ["form-error"]
            } else {
                ["form-note"]
            });
            for mark in &field.marks {
                mark.update_state(&[gtk4::accessible::State::Invalid(if error.is_some() {
                    gtk4::AccessibleInvalidState::True
                } else {
                    gtk4::AccessibleInvalidState::False
                })]);
                let described = [Some(field.described.as_str()), error.as_deref()]
                    .into_iter()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                mark.update_property(&[gtk4::accessible::Property::Description(&described)]);
            }
        }
        let hint = draft.submit_hint();
        drop(draft);
        if let Some(submit) = &self.submit {
            submit.set_sensitive(hint.is_none());
            submit.set_tooltip_text(Some(
                hint.as_deref()
                    .unwrap_or("Send these answers to the tool server"),
            ));
        }
        if self.state.get() == FormState::Pending && self.submit.is_some() {
            self.say(hint.as_deref().unwrap_or(""));
        }
        if let (Some(open), Some(done)) = (&self.open, &self.done) {
            let used = self.used_open.get();
            for (button, suggested) in [(open, !used), (done, used)] {
                if suggested {
                    button.add_css_class("suggested-action");
                } else {
                    button.remove_css_class("suggested-action");
                }
            }
        }
    }

    /// The line above the buttons: why Submit is off, or how the form ended. Empty hides it.
    fn say(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
    }

    fn update(&self, card: &FormCard) {
        self.state.set(card.state);
        self.show_state(card.state);
        self.refusal
            .set_visible(card.refused && card.state == FormState::Pending);
        self.refresh();
    }

    fn show_state(&self, state: FormState) {
        let link = matches!(self.form, Form::Link { .. });
        let open = matches!(state, FormState::Pending | FormState::Sent);
        self.body.set_visible(open);
        self.body.set_sensitive(state == FormState::Pending);
        self.buttons.set_visible(open);
        self.buttons.set_sensitive(state == FormState::Pending);
        self.sent_to.set_visible(open);
        if !open || !link {
            self.opened.set_visible(false);
            self.open_failed.set_visible(false);
        }
        for class in ["resolved", "declined", "expired"] {
            self.root.remove_css_class(class);
        }
        let text = match state {
            FormState::Pending => None,
            FormState::Sent => Some("Sending…".to_owned()),
            FormState::Resolved(action) => {
                self.root.add_css_class("resolved");
                Some(
                    match (action, link) {
                        (FormAction::Accept, false) => "Submitted",
                        (FormAction::Accept, true) => "Done",
                        (FormAction::Decline, _) => "Declined",
                        (FormAction::Cancel, _) => "Dismissed",
                    }
                    .to_owned(),
                )
            }
            FormState::Expired => {
                self.root.add_css_class("expired");
                Some("Expired: the agent that asked has exited".to_owned())
            }
            FormState::Unanswerable => {
                self.root.add_css_class("expired");
                Some(
                    if matches!(self.form, Form::Unknown) {
                        "This kind of form needs a newer agent-terminal, so it cannot be answered here."
                    } else {
                        "agent-terminal cannot show this form, so it cannot be answered here."
                    }
                    .to_owned(),
                )
            }
        };
        if matches!(
            state,
            FormState::Resolved(FormAction::Decline | FormAction::Cancel)
        ) {
            self.root.add_css_class("declined");
        }
        if let Some(text) = text {
            self.say(&text);
        }
    }
}

/// The link block: "Opens <host>", a warning when the link needs one, and the whole link.
fn build_link(url: &str, body: &gtk4::Box) {
    let host = form::link_host(url).unwrap_or_default();
    let host_line = label("", &["form-host"]);
    host_line.set_markup(&format!("Opens <b>{}</b>", glib::markup_escape_text(&host)));
    wrapping(&host_line);
    body.append(&host_line);
    let concern = label(form::link_concern(url).unwrap_or(""), &["approval-warning"]);
    wrapping(&concern);
    concern.set_visible(form::link_concern(url).is_some());
    body.append(&concern);
    let frame = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    frame.add_css_class("mono-scroller");
    let text = mono(url);
    text.update_property(&[gtk4::accessible::Property::Label("The whole link")]);
    frame.append(&text);
    body.append(&frame);
}

/// The label row of a field: its name, and "Required" when it must be answered.
fn field_head(spec: &FormField) -> gtk4::Box {
    let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let name = label(&spec.label(), &["form-field-label"]);
    name.set_wrap(true);
    name.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    head.append(&name);
    if spec.required {
        let tag = label("Required", &["form-required"]);
        tag.set_valign(gtk4::Align::Center);
        head.append(&tag);
    }
    head
}

fn describe_limits(input: &FieldInput) -> Option<String> {
    match input {
        FieldInput::Text {
            min_length,
            max_length,
            ..
        } => match (min_length.filter(|m| *m > 0), max_length) {
            (Some(min), Some(max)) => Some(format!("{min} to {max} characters")),
            (Some(min), None) => Some(format!("At least {min} characters")),
            (None, Some(max)) => Some(format!("Up to {max} characters")),
            (None, None) => None,
        },
        FieldInput::Number {
            integer,
            minimum,
            maximum,
            ..
        } => {
            let kind = if *integer { "Whole number" } else { "Number" };
            let bound = |n: &f64| {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    n.to_string()
                }
            };
            Some(match (minimum, maximum) {
                (Some(min), Some(max)) => format!("{kind}, {} to {}", bound(min), bound(max)),
                (Some(min), None) => format!("{kind}, {} or more", bound(min)),
                (None, Some(max)) => format!("{kind}, {} or less", bound(max)),
                (None, None) => kind.to_owned(),
            })
        }
        _ => None,
    }
}

/// Wires the hooks every field has: its value reaches the draft, and leaving it lets its
/// error show.
fn wire(ui_root: &gtk4::Box, index: usize, weak: &Weak<Inner>) {
    let focus = gtk4::EventControllerFocus::new();
    let weak = weak.clone();
    let root = ui_root.downgrade();
    focus.connect_leave(move |_| {
        // Switching to another window is not leaving the field.
        let away = root
            .upgrade()
            .and_then(|r| r.root())
            .and_downcast::<gtk4::Window>()
            .is_some_and(|w| !w.is_active());
        if away {
            return;
        }
        if let Some(inner) = weak.upgrade() {
            if let Some(field) = inner.fields.get(index) {
                field.visited.set(true);
            }
            inner.refresh();
        }
    });
    ui_root.add_controller(focus);
}

/// What differs between a text field and a number field, which are otherwise the same entry.
struct EntryKind {
    placeholder: &'static str,
    purpose: gtk4::InputPurpose,
    /// What it starts with (its default), if anything.
    start: Option<String>,
    /// What typed text means: nothing, or the value the draft holds.
    value: fn(&str) -> Value,
}

/// A single-line field. It is an `Entry` even for a number (a spin button always holds a number,
/// so a field nobody touched would send 0). Enter in the last field tries to submit.
fn entry_field(
    kind: EntryKind,
    accessible: &str,
    required: bool,
    (index, last): (usize, bool),
    weak: &Weak<Inner>,
) -> gtk4::Entry {
    let entry = gtk4::Entry::new();
    entry.set_hexpand(true);
    entry.set_placeholder_text(Some(kind.placeholder));
    entry.set_input_purpose(kind.purpose);
    if let Some(start) = &kind.start {
        entry.set_text(start);
    }
    entry.update_property(&[
        gtk4::accessible::Property::Label(accessible),
        gtk4::accessible::Property::Required(required),
    ]);
    let value = kind.value;
    entry.connect_changed({
        let weak = weak.clone();
        move |entry| record(&weak, index, value(entry.text().as_str()))
    });
    if last {
        entry.connect_activate({
            let weak = weak.clone();
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.attempt();
                }
            }
        });
    }
    entry
}

fn record(weak: &Weak<Inner>, index: usize, value: Value) {
    if let Some(inner) = weak.upgrade() {
        // Dropped before refresh, which borrows the draft again.
        inner.draft.borrow_mut().set(index, value);
        inner.refresh();
    }
}

fn build_field(
    index: usize,
    spec: &FormField,
    initial: &Value,
    weak: &Weak<Inner>,
    last: bool,
) -> FieldUi {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    root.add_css_class("form-field");
    root.append(&field_head(spec));
    let description = spec
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    if let Some(description) = description {
        let d = content_label(description, &["question-hint"]);
        wrapping(&d);
        root.append(&d);
    }
    let name = spec.label();
    let accessible = if spec.required {
        format!("{name}, required")
    } else {
        name.clone()
    };
    let mut marks: Vec<gtk4::Widget> = Vec::new();
    let focus: gtk4::Widget;
    let limits = describe_limits(&spec.input);
    match &spec.input {
        FieldInput::Text { format, .. } => {
            let entry = entry_field(
                EntryKind {
                    placeholder: match format {
                        Some(TextFormat::Email) => "name@example.com",
                        Some(TextFormat::Uri) => "https://example.com",
                        Some(TextFormat::Date) => "YYYY-MM-DD",
                        Some(TextFormat::DateTime) => "YYYY-MM-DDThh:mm",
                        _ => "",
                    },
                    purpose: match format {
                        Some(TextFormat::Email) => gtk4::InputPurpose::Email,
                        Some(TextFormat::Uri) => gtk4::InputPurpose::Url,
                        _ => gtk4::InputPurpose::FreeForm,
                    },
                    start: initial.as_str().map(str::to_owned),
                    value: |text| {
                        if text.trim().is_empty() {
                            Value::Null
                        } else {
                            Value::String(text.to_owned())
                        }
                    },
                },
                &accessible,
                spec.required,
                (index, last),
                weak,
            );
            root.append(&entry);
            focus = entry.clone().upcast();
            marks.push(entry.upcast());
        }
        FieldInput::Number { integer, .. } => {
            // An entry, not a spin button: a spin button always holds a number, so a field
            // nobody touched would send 0.
            let entry = entry_field(
                EntryKind {
                    placeholder: if *integer { "Whole number" } else { "Number" },
                    purpose: if *integer {
                        gtk4::InputPurpose::Digits
                    } else {
                        gtk4::InputPurpose::Number
                    },
                    start: initial.as_number().map(|n| n.to_string()),
                    value: number_value,
                },
                &accessible,
                spec.required,
                (index, last),
                weak,
            );
            root.append(&entry);
            focus = entry.clone().upcast();
            marks.push(entry.upcast());
        }
        FieldInput::Boolean { .. } => {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            let yes = gtk4::ToggleButton::with_label("Yes");
            let no = gtk4::ToggleButton::with_label("No");
            for (button, answer) in [(&yes, "Yes"), (&no, "No")] {
                button.add_css_class("option-button");
                button.update_property(&[gtk4::accessible::Property::Label(&format!(
                    "{name}: {answer}"
                ))]);
                row.append(button);
            }
            match initial {
                Value::Bool(true) => yes.set_active(true),
                Value::Bool(false) => no.set_active(true),
                _ => {}
            }
            let guard = Rc::new(Cell::new(false));
            let clear = clear_button(&name, &guard, vec![yes.clone(), no.clone()], weak, index);
            clear.set_visible(!spec.required && !initial.is_null());
            for (this, other, value) in [(&yes, &no, true), (&no, &yes, false)] {
                this.connect_toggled({
                    let (weak, guard, other, clear) =
                        (weak.clone(), guard.clone(), other.clone(), clear.clone());
                    let required = spec.required;
                    move |this| {
                        if guard.get() {
                            return;
                        }
                        guard.set(true);
                        let picked = this.is_active();
                        if picked {
                            other.set_active(false);
                        }
                        guard.set(false);
                        clear.set_visible(!required && picked);
                        record(
                            &weak,
                            index,
                            if picked {
                                Value::Bool(value)
                            } else {
                                Value::Null
                            },
                        );
                    }
                });
            }
            row.append(&clear);
            root.append(&row);
            focus = yes.clone().upcast();
            marks.extend([yes.upcast::<gtk4::Widget>(), no.upcast()]);
        }
        FieldInput::Choice {
            options,
            multi: false,
            ..
        } if options.len() > BUTTON_OPTIONS => {
            let placeholder = if spec.required {
                "Choose one…"
            } else {
                "None"
            };
            let mut names: Vec<&str> = vec![placeholder];
            names.extend(options.iter().map(|o| o.label.as_str()));
            let dropdown = gtk4::DropDown::from_strings(&names);
            dropdown.set_hexpand(true);
            dropdown.update_property(&[gtk4::accessible::Property::Label(&accessible)]);
            if let Value::String(value) = initial {
                if let Some(at) = options.iter().position(|o| &o.value == value) {
                    dropdown.set_selected(at as u32 + 1);
                }
            }
            dropdown.connect_selected_notify({
                let (weak, options) = (weak.clone(), options.clone());
                move |dropdown| {
                    let value = (dropdown.selected() as usize)
                        .checked_sub(1)
                        .and_then(|at| options.get(at))
                        .map_or(Value::Null, |o| Value::String(o.value.clone()));
                    record(&weak, index, value);
                }
            });
            root.append(&dropdown);
            focus = dropdown.clone().upcast();
            marks.push(dropdown.upcast());
        }
        FieldInput::Choice {
            options,
            multi: false,
            ..
        } => {
            let flow = gtk4::FlowBox::new();
            flow.set_selection_mode(gtk4::SelectionMode::None);
            flow.set_max_children_per_line(4);
            flow.set_column_spacing(8);
            flow.set_row_spacing(8);
            flow.set_homogeneous(false);
            flow.set_halign(gtk4::Align::Start);
            let guard = Rc::new(Cell::new(false));
            let toggles: Vec<gtk4::ToggleButton> = options
                .iter()
                .map(|o| {
                    let b = gtk4::ToggleButton::new();
                    b.add_css_class("option-button");
                    let text = label(&o.label, &["option-label"]);
                    wrapping(&text);
                    text.set_hexpand(false);
                    text.set_max_width_chars(36);
                    b.set_child(Some(&text));
                    b.update_property(&[gtk4::accessible::Property::Label(&format!(
                        "{name}: {}",
                        o.label
                    ))]);
                    if matches!(initial, Value::String(v) if *v == o.value) {
                        b.set_active(true);
                    }
                    add_button(&flow, &b);
                    b
                })
                .collect();
            let clear = clear_button(&name, &guard, toggles.clone(), weak, index);
            clear.set_visible(!spec.required && !initial.is_null());
            for (at, toggle) in toggles.iter().enumerate() {
                toggle.connect_toggled({
                    let (weak, guard, all, clear, options) = (
                        weak.clone(),
                        guard.clone(),
                        toggles.clone(),
                        clear.clone(),
                        options.clone(),
                    );
                    let required = spec.required;
                    move |this| {
                        if guard.get() {
                            return;
                        }
                        guard.set(true);
                        let picked = this.is_active();
                        if picked {
                            for (i, other) in all.iter().enumerate() {
                                if i != at {
                                    other.set_active(false);
                                }
                            }
                        }
                        guard.set(false);
                        clear.set_visible(!required && picked);
                        let value = if picked {
                            options
                                .get(at)
                                .map_or(Value::Null, |o| Value::String(o.value.clone()))
                        } else {
                            Value::Null
                        };
                        record(&weak, index, value);
                    }
                });
            }
            root.append(&flow);
            root.append(&clear);
            focus = toggles
                .first()
                .map_or_else(|| flow.clone().upcast(), |t| t.clone().upcast());
            marks.extend(toggles.into_iter().map(|t| t.upcast::<gtk4::Widget>()));
        }
        FieldInput::Choice {
            options,
            multi: true,
            min_items,
            max_items,
            ..
        } => {
            let hint = label(&choose_hint(*min_items, *max_items), &["question-hint"]);
            root.append(&hint);
            let column = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            let checks: Vec<gtk4::CheckButton> = options
                .iter()
                .map(|o| {
                    let c = gtk4::CheckButton::with_label(&o.label);
                    c.update_property(&[gtk4::accessible::Property::Label(&format!(
                        "{name}: {}",
                        o.label
                    ))]);
                    if matches!(initial, Value::Array(a) if a.iter().any(|v| v.as_str() == Some(o.value.as_str())))
                    {
                        c.set_active(true);
                    }
                    column.append(&c);
                    c
                })
                .collect();
            for check in &checks {
                check.connect_toggled({
                    let (weak, checks, options) = (weak.clone(), checks.clone(), options.clone());
                    move |_| {
                        let picked: Vec<Value> = checks
                            .iter()
                            .zip(&options)
                            .filter(|(c, _)| c.is_active())
                            .map(|(_, o)| Value::String(o.value.clone()))
                            .collect();
                        record(
                            &weak,
                            index,
                            if picked.is_empty() {
                                Value::Null
                            } else {
                                Value::Array(picked)
                            },
                        );
                    }
                });
            }
            root.append(&column);
            focus = checks
                .first()
                .map_or_else(|| column.clone().upcast(), |c| c.clone().upcast());
            marks.extend(checks.into_iter().map(|c| c.upcast::<gtk4::Widget>()));
        }
        FieldInput::Unknown => {
            let note = label(
                "agent-terminal cannot show this kind of field. Decline, or answer it in the tool's own interface.",
                &["form-note"],
            );
            wrapping(&note);
            root.append(&note);
            focus = root.clone().upcast();
        }
    }
    if let Some(limits) = limits {
        root.append(&label(&limits, &["question-hint"]));
    }
    let error = label("", &["form-error"]);
    wrapping(&error);
    error.set_visible(false);
    error.set_selectable(false);
    error.update_property(&[gtk4::accessible::Property::Label("Problem with this field")]);
    root.append(&error);
    wire(&root, index, weak);
    FieldUi {
        root,
        error,
        focus,
        marks,
        described: description.unwrap_or_default().to_owned(),
        visited: Cell::new(false),
    }
}

/// "Clear" for an optional yes/no or single choice: puts it back to "not answered".
fn clear_button(
    field: &str,
    guard: &Rc<Cell<bool>>,
    toggles: Vec<gtk4::ToggleButton>,
    weak: &Weak<Inner>,
    index: usize,
) -> gtk4::Button {
    let clear = gtk4::Button::with_label("Clear");
    clear.add_css_class("flat");
    clear.set_tooltip_text(Some("Leave this field unanswered"));
    clear.update_property(&[gtk4::accessible::Property::Label(&format!("Clear {field}"))]);
    clear.connect_clicked({
        let (guard, weak) = (guard.clone(), weak.clone());
        move |clear| {
            guard.set(true);
            for toggle in &toggles {
                toggle.set_active(false);
            }
            guard.set(false);
            clear.set_visible(false);
            record(&weak, index, Value::Null);
        }
    });
    clear
}

#[cfg(test)]
impl FormCardWidget {
    /// The status line (the reason Submit is off, or the outcome).
    pub(super) fn status(&self) -> String {
        self.inner.status.text().to_string()
    }

    pub(super) fn buttons(&self) -> Vec<String> {
        if !self.inner.buttons.is_visible() {
            return Vec::new();
        }
        super::cards::row_buttons(&self.inner.buttons)
            .iter()
            .filter(|w| w.is_visible())
            .filter_map(|w| w.downcast_ref::<gtk4::Button>().and_then(|b| b.label()))
            .map(|l| l.to_string())
            .collect()
    }

    pub(super) fn button(&self, label: &str) -> Option<gtk4::Button> {
        super::cards::row_buttons(&self.inner.buttons)
            .iter()
            .filter_map(|w| w.downcast_ref::<gtk4::Button>().cloned())
            .find(|b| b.label().as_deref() == Some(label))
    }

    /// The text entry of field `index`.
    pub(super) fn entry(&self, index: usize) -> Option<gtk4::Entry> {
        self.inner
            .fields
            .get(index)?
            .marks
            .first()?
            .clone()
            .downcast::<gtk4::Entry>()
            .ok()
    }

    /// The toggle buttons or checks of field `index`.
    pub(super) fn marks(&self, index: usize) -> Vec<gtk4::Widget> {
        self.inner
            .fields
            .get(index)
            .map_or_else(Vec::new, |f| f.marks.clone())
    }

    /// The error under field `index`, when shown.
    pub(super) fn error(&self, index: usize) -> Option<String> {
        let ui = self.inner.fields.get(index)?;
        (ui.error.is_visible() && ui.error.has_css_class("form-error"))
            .then(|| ui.error.text().to_string())
    }

    pub(super) fn field_note(&self, index: usize) -> Option<String> {
        let ui = self.inner.fields.get(index)?;
        (ui.error.is_visible() && ui.error.has_css_class("form-note"))
            .then(|| ui.error.text().to_string())
    }

    pub(super) fn submit_enabled(&self) -> Option<bool> {
        self.inner.submit.as_ref().map(|b| b.is_sensitive())
    }

    pub(super) fn is_suggested(&self, label: &str) -> bool {
        self.button(label)
            .is_some_and(|b| b.has_css_class("suggested-action"))
    }

    /// The first label of the card with CSS class `class` (visible or not).
    fn label_with(&self, class: &str) -> Option<gtk4::Label> {
        crate::testutil::descendants(self.inner.root.upcast_ref())
            .into_iter()
            .find(|w| w.has_css_class(class))
            .and_then(|w| w.downcast::<gtk4::Label>().ok())
    }

    pub(super) fn host_line(&self) -> String {
        self.label_with("form-host")
            .map(|l| l.text().to_string())
            .unwrap_or_default()
    }

    pub(super) fn concern(&self) -> Option<String> {
        self.label_with("approval-warning")
            .filter(|l| l.is_visible())
            .map(|l| l.text().to_string())
    }

    pub(super) fn link_text(&self) -> String {
        self.label_with("mono-text")
            .map(|l| l.text().to_string())
            .unwrap_or_default()
    }

    pub(super) fn refusal(&self) -> Option<String> {
        self.inner
            .refusal
            .is_visible()
            .then(|| self.inner.refusal.text().to_string())
    }

    /// The app's own warning about secrets, when the form has a text field.
    pub(super) fn secret_note(&self) -> Option<String> {
        self.label_with("form-secret-note")
            .map(|l| l.text().to_string())
    }

    pub(super) fn opened_note(&self) -> Option<String> {
        self.inner
            .opened
            .is_visible()
            .then(|| self.inner.opened.text().to_string())
    }

    pub(super) fn failure_note(&self) -> Option<String> {
        self.inner
            .open_failed
            .is_visible()
            .then(|| self.inner.open_failed.text().to_string())
    }

    pub(super) fn body_visible(&self) -> bool {
        self.inner.body.is_visible()
    }

    pub(super) fn sent_to(&self) -> Option<String> {
        self.inner
            .sent_to
            .is_visible()
            .then(|| self.inner.sent_to.text().to_string())
    }
}

#[cfg(test)]
impl FormCardWidget {
    /// The user has been through field `index` (as focus leaving it does).
    pub(super) fn visit(&self, index: usize) {
        if let Some(field) = self.inner.fields.get(index) {
            field.visited.set(true);
        }
        self.inner.refresh();
    }

    /// What the card would send right now, or `None` when Submit is off.
    pub(super) fn answer(&self) -> Option<serde_json::Map<String, Value>> {
        self.inner.draft.borrow().check().ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_core::form::ChoiceOption;
    use serde_json::json;

    type Events = Rc<RefCell<Vec<RowEvent>>>;

    fn card_of(form: Form) -> FormCard {
        FormCard {
            request: "f".into(),
            server: "github".into(),
            message: "Which repo should I use?".into(),
            form,
            state: FormState::Pending,
            refused: false,
        }
    }

    fn widget(form: Form) -> (FormCardWidget, Events) {
        let events: Events = Rc::default();
        let received = events.clone();
        let sink: RowSink = Rc::new(move |e| received.borrow_mut().push(e));
        (FormCardWidget::new(&card_of(form), &sink), events)
    }

    fn field(name: &str, required: bool, input: FieldInput) -> FormField {
        FormField {
            name: name.into(),
            title: None,
            description: None,
            required,
            input,
        }
    }

    fn text(default: Option<&str>) -> FieldInput {
        FieldInput::Text {
            format: None,
            min_length: None,
            max_length: None,
            default: default.map(str::to_owned),
        }
    }

    fn options(n: usize) -> Vec<ChoiceOption> {
        (0..n)
            .map(|i| ChoiceOption {
                value: format!("v{i}"),
                label: format!("Option {i}"),
            })
            .collect()
    }

    fn choice(n: usize, multi: bool, min: Option<u64>, max: Option<u64>) -> FieldInput {
        FieldInput::Choice {
            options: options(n),
            multi,
            min_items: min,
            max_items: max,
            default: Vec::new(),
        }
    }

    fn labels_in(widget: &impl IsA<gtk4::Widget>) -> Vec<String> {
        crate::testutil::descendants(widget.upcast_ref())
            .iter()
            .filter_map(|w| w.downcast_ref::<gtk4::Label>())
            .map(|l| l.text().to_string())
            .collect()
    }

    fn last_form(events: &Events) -> (FormAction, Option<Value>) {
        match events.borrow().last() {
            Some(RowEvent::Form {
                action, content, ..
            }) => (*action, content.clone()),
            _ => panic!("no form event"),
        }
    }

    /// Entry fields, toggles, choices and how the card answers for them.
    pub(crate) fn form_and_link_cards() {
        a_form_card_frames_the_servers_words_and_says_why_submit_is_off();
        only_what_the_user_touched_or_was_given_is_sent();
        enter_submits_only_from_the_last_field_and_only_when_valid();
        fields_that_cannot_be_filled_in_never_pass();
        a_closed_form_card_shows_its_outcome();
        a_link_card_steers_open_then_done();
        a_link_that_cannot_be_opened_never_reaches_the_desktop();
    }

    fn a_form_card_frames_the_servers_words_and_says_why_submit_is_off() {
        let (card, events) = widget(Form::Fields {
            fields: vec![
                field("repo_name", true, text(None)),
                field(
                    "depth",
                    false,
                    FieldInput::Number {
                        integer: true,
                        minimum: Some(1.0),
                        maximum: Some(9.0),
                        default: None,
                    },
                ),
                field("flavour", false, choice(8, false, None, None)),
                field("tags", false, choice(4, true, Some(1), Some(3))),
            ],
        });
        let labels = labels_in(card.root());
        // The server's name is a chip and its words are a quote: never the app's voice.
        assert!(
            labels.contains(&"github tool server".to_owned()),
            "{labels:?}"
        );
        assert!(labels.contains(&"Which repo should I use?".to_owned()));
        assert!(labels.contains(&"A tool server wants information".to_owned()));
        assert!(
            labels.contains(&"Repo name".to_owned()),
            "a key reads as words"
        );
        assert_eq!(
            labels.iter().filter(|l| *l == "Required").count(),
            1,
            "only the required field says so"
        );
        assert!(labels.contains(&"Choose 1 to 3".to_owned()));
        assert!(labels.contains(&"Whole number, 1 to 9".to_owned()));
        let quote = crate::testutil::descendants(card.root().upcast_ref())
            .into_iter()
            .find(|w| w.has_css_class("md-quote"))
            .expect("the message is quoted");
        assert!(quote.has_css_class("form-message"));

        // Nothing is answered, and Submit says why it is off, in one line.
        assert_eq!(card.buttons(), ["Submit", "Decline"]);
        assert_eq!(card.submit_enabled(), Some(false));
        assert_eq!(card.status(), "Fill in Repo name");
        assert_eq!(card.sent_to().as_deref(), Some("Sent to github."));
        assert!(
            card.error(0).is_none(),
            "no scolding before the user has been there"
        );
        assert_eq!(
            card.entry(1).map(|e| e.text().to_string()),
            Some(String::new()),
            "no 0"
        );
        assert!(card.answer().is_none());
        // No focus is taken when the card appears.
        assert!(card.root().focus_child().is_none() && !card.root().is_focus());
        // Escape means nothing on this card: none of its own containers handles keys (the
        // controls inside bring GTK's own, for arrows in a list and the like).
        for widget in crate::testutil::descendants(card.root().upcast_ref())
            .into_iter()
            .chain([card.root().clone().upcast::<gtk4::Widget>()])
            .filter(|w| w.is::<gtk4::Box>())
        {
            let controllers = widget.observe_controllers();
            for i in 0..controllers.n_items() {
                let controller = controllers.item(i).expect("controller");
                assert!(
                    !controller.is::<gtk4::EventControllerKey>(),
                    "{widget:?} handles keys"
                );
            }
        }

        // The error shows when the user leaves the field, and goes when it is fixed.
        card.visit(0);
        assert_eq!(card.error(0).as_deref(), Some("Repo name is required."));
        let entry = card.entry(0).expect("entry");
        entry.set_text("   ");
        assert!(card.error(0).is_some(), "blanks are not an answer");
        entry.set_text("octo");
        assert!(card.error(0).is_none());
        assert_eq!(card.submit_enabled(), Some(true));
        assert_eq!(card.status(), "");

        // A bad number is refused with a reason, once the user has been there.
        let depth = card.entry(1).expect("number entry");
        depth.set_text("0");
        assert_eq!(card.status(), "Fix Depth");
        card.visit(1);
        assert_eq!(card.error(1).as_deref(), Some("Enter 1 or more."));
        depth.set_text("abc");
        assert_eq!(card.error(1).as_deref(), Some("Enter a number."));
        depth.set_text("4");
        assert!(card.error(1).is_none());

        // Decline sends no content, and there is no second way to say no.
        card.button("Decline").expect("decline").emit_clicked();
        assert_eq!(last_form(&events), (FormAction::Decline, None));
        assert!(card.button("Dismiss").is_none() && card.button("Stop turn").is_none());
        // Said in the app's own words, because the form has a text field.
        assert_eq!(
            card.secret_note().as_deref(),
            Some("agent-terminal never needs a password, key or code here. Don't paste one.")
        );
    }

    fn only_what_the_user_touched_or_was_given_is_sent() {
        let (card, events) = widget(Form::Fields {
            fields: vec![
                field("name", true, text(Some("octo"))),
                field("verbose", false, FieldInput::Boolean { default: None }),
                field(
                    "colour",
                    false,
                    FieldInput::Boolean {
                        default: Some(false),
                    },
                ),
                field("kind", false, choice(3, false, None, None)),
                field("flavour", false, choice(8, false, None, None)),
                field("tags", false, choice(3, true, None, None)),
            ],
        });
        // Defaults count as given; nothing else is sent.
        assert_eq!(
            card.answer().map(Value::Object),
            Some(json!({"name": "octo", "colour": false}))
        );
        assert_eq!(card.submit_enabled(), Some(true));

        // Yes / No are exclusive, neither is picked first, and choosing again clears.
        let toggles = |i: usize| -> Vec<gtk4::ToggleButton> {
            card.marks(i)
                .into_iter()
                .filter_map(|w| w.downcast::<gtk4::ToggleButton>().ok())
                .collect()
        };
        let verbose = toggles(1);
        assert!(verbose.iter().all(|t| !t.is_active()), "neither is picked");
        verbose[0].set_active(true);
        assert!(verbose[0].is_active() && !verbose[1].is_active());
        verbose[1].set_active(true);
        assert!(!verbose[0].is_active() && verbose[1].is_active());
        assert_eq!(
            card.answer().map(Value::Object),
            Some(json!({"name": "octo", "verbose": false, "colour": false}))
        );
        verbose[1].set_active(false);
        assert_eq!(
            card.answer().map(Value::Object),
            Some(json!({"name": "octo", "colour": false})),
            "un-picking puts it back to unanswered"
        );
        // A default of "No" is a pick the user can take back with Clear.
        let colour = toggles(2);
        assert!(colour[1].is_active());

        // A few options are buttons, one at a time; many are a dropdown with no pick.
        let kinds = toggles(3);
        assert_eq!(kinds.len(), 3);
        kinds[1].set_active(true);
        kinds[2].set_active(true);
        assert!(!kinds[1].is_active() && kinds[2].is_active());
        assert_eq!(card.answer().expect("valid")["kind"], "v2");
        let flavour = card
            .marks(4)
            .into_iter()
            .next()
            .and_then(|w| w.downcast::<gtk4::DropDown>().ok())
            .expect("more than six options is a dropdown");
        assert_eq!(flavour.selected(), 0, "no pick");
        assert!(card.answer().expect("valid").get("flavour").is_none());
        flavour.set_selected(3);
        assert_eq!(card.answer().expect("valid")["flavour"], "v2");
        flavour.set_selected(0);
        assert!(card.answer().expect("valid").get("flavour").is_none());

        // Several: check buttons, in the options' order.
        let tags: Vec<gtk4::CheckButton> = card
            .marks(5)
            .into_iter()
            .filter_map(|w| w.downcast::<gtk4::CheckButton>().ok())
            .collect();
        tags[2].set_active(true);
        tags[0].set_active(true);
        assert_eq!(card.answer().expect("valid")["tags"], json!(["v0", "v2"]));

        // Submit sends exactly that and nothing more.
        card.button("Submit").expect("submit").emit_clicked();
        let (action, content) = last_form(&events);
        assert_eq!(action, FormAction::Accept);
        assert_eq!(
            content,
            Some(json!({
                "name": "octo", "colour": false, "kind": "v2", "tags": ["v0", "v2"]
            }))
        );
    }

    fn enter_submits_only_from_the_last_field_and_only_when_valid() {
        let (card, events) = widget(Form::Fields {
            fields: vec![
                field("first", false, text(None)),
                field("repo_name", true, text(None)),
            ],
        });
        let first = card.entry(0).expect("first");
        first.set_text("x");
        first.emit_activate();
        assert!(
            events.borrow().is_empty(),
            "Enter elsewhere only activates the field"
        );

        let last = card.entry(1).expect("last");
        last.emit_activate();
        assert!(events.borrow().is_empty(), "not valid: nothing is sent");
        assert_eq!(
            card.error(1).as_deref(),
            Some("Repo name is required."),
            "the attempt shows what is wrong"
        );
        last.set_text("octo");
        last.emit_activate();
        assert_eq!(
            last_form(&events),
            (
                FormAction::Accept,
                Some(json!({"first": "x", "repo_name": "octo"}))
            )
        );
        // A form whose last field is not a text entry has no Enter shortcut at all.
        let (card, events) = widget(Form::Fields {
            fields: vec![
                field("a", false, text(Some("kept"))),
                field("b", false, FieldInput::Boolean { default: None }),
            ],
        });
        card.entry(0).expect("entry").emit_activate();
        assert!(events.borrow().is_empty());
    }

    fn fields_that_cannot_be_filled_in_never_pass() {
        let (card, _) = widget(Form::Fields {
            fields: vec![
                field("legacy", true, FieldInput::Unknown),
                field("extra", false, FieldInput::Unknown),
            ],
        });
        assert_eq!(card.submit_enabled(), Some(false));
        assert_eq!(card.status(), "Cannot fill in Legacy");
        assert_eq!(
            card.field_note(1).as_deref(),
            Some("This optional field was left out.")
        );
        assert!(labels_in(card.root())
            .iter()
            .any(|l| l.contains("cannot show this kind of field")));
        let (card, _) = widget(Form::Fields {
            fields: vec![field("extra", false, FieldInput::Unknown)],
        });
        assert_eq!(card.submit_enabled(), Some(true), "left out, not blocked");
    }

    fn a_closed_form_card_shows_its_outcome() {
        let sink: RowSink = Rc::new(|_| {});
        let form = Form::Fields {
            fields: vec![field("repo_name", true, text(None))],
        };
        let card = FormCardWidget::new(&card_of(form.clone()), &sink);
        let with = |state, form: &Form| FormCard {
            state,
            form: form.clone(),
            ..card_of(form.clone())
        };
        card.update(&with(FormState::Sent, &form));
        assert_eq!(card.status(), "Sending…");
        assert!(card.body_visible() && !card.root().is_focus());
        assert!(!card.inner.buttons.is_sensitive() && !card.inner.body.is_sensitive());
        for (state, outcome) in [
            (FormState::Resolved(FormAction::Accept), "Submitted"),
            (FormState::Resolved(FormAction::Decline), "Declined"),
            (FormState::Resolved(FormAction::Cancel), "Dismissed"),
            (
                FormState::Expired,
                "Expired: the agent that asked has exited",
            ),
        ] {
            card.update(&with(state, &form));
            assert_eq!(card.status(), outcome);
            assert!(!card.body_visible(), "{outcome}: nothing left to fill in");
            assert!(card.buttons().is_empty() && card.sent_to().is_none());
        }
        // A form kind from a newer build is shown closed and says why.
        let unknown = FormCardWidget::new(
            &FormCard {
                state: FormState::Unanswerable,
                ..card_of(Form::Unknown)
            },
            &sink,
        );
        assert!(unknown.status().contains("newer agent-terminal"));
        assert!(unknown.buttons().is_empty() && !unknown.body_visible());
        assert!(!unknown.focus_first(), "a closed card takes no focus");
        // A pending card takes focus only when asked to (the jump), on its first field.
        let (open, _) = widget(form);
        assert!(open.root().focus_child().is_none());
    }

    fn link_card(url: &str) -> (FormCardWidget, Events) {
        widget(Form::Link { url: url.into() })
    }

    fn a_link_card_steers_open_then_done() {
        let (card, events) = link_card("https://github.com/login/device?code=AB-CD");
        assert_eq!(card.host_line(), "Opens github.com");
        assert_eq!(
            card.link_text(),
            "https://github.com/login/device?code=AB-CD"
        );
        assert!(card.concern().is_none());
        assert_eq!(card.buttons(), ["Open in Browser", "Done", "Decline"]);
        assert!(card.is_suggested("Open in Browser") && !card.is_suggested("Done"));
        assert!(
            labels_in(card.root()).contains(&"A tool server wants you to open a link".to_owned())
        );

        let opened: Rc<RefCell<Vec<String>>> = Rc::default();
        let record = opened.clone();
        card.set_launcher(Rc::new(move |url, _, done| {
            record.borrow_mut().push(url.to_owned());
            done(Ok(()));
        }));
        card.button("Open in Browser").expect("open").emit_clicked();
        assert_eq!(
            *opened.borrow(),
            ["https://github.com/login/device?code=AB-CD"]
        );
        assert_eq!(
            card.opened_note().as_deref(),
            Some("Opened in your browser. Choose Done when you have finished.")
        );
        assert!(
            card.is_suggested("Done") && !card.is_suggested("Open in Browser"),
            "after opening, Done is the next step"
        );
        card.button("Done").expect("done").emit_clicked();
        assert_eq!(last_form(&events), (FormAction::Accept, None));

        // A failed launch says so and offers the link to copy; Open stays the next step.
        let (card, _) = link_card("https://example.com/auth");
        card.set_launcher(Rc::new(|_, _, done| done(Err("no browser".into()))));
        card.button("Open in Browser").expect("open").emit_clicked();
        assert!(card
            .failure_note()
            .expect("failure")
            .contains("Copy the link"));
        assert!(card.opened_note().is_none());
        assert!(card.buttons().contains(&"Copy Link".to_owned()));
        assert!(card.is_suggested("Open in Browser"));

        // Something to know before opening is said in words.
        let (card, _) = link_card("http://localhost:3000/cb");
        assert_eq!(card.host_line(), "Opens localhost");
        assert!(card
            .concern()
            .expect("concern")
            .contains("this computer or your local network"));
        let (card, _) = link_card("http://example.com/x");
        assert!(card.concern().expect("concern").contains("not encrypted"));
        let (card, events) = link_card("https://example.com/x");
        card.button("Decline").expect("decline").emit_clicked();
        assert_eq!(last_form(&events), (FormAction::Decline, None));
        let (card, _) = link_card("https://example.com/x");
        card.update(&FormCard {
            state: FormState::Resolved(FormAction::Accept),
            form: Form::Link {
                url: "https://example.com/x".into(),
            },
            ..card_of(Form::Unknown)
        });
        assert_eq!(card.status(), "Done");
    }

    fn a_link_that_cannot_be_opened_never_reaches_the_desktop() {
        // The model never makes such a card answerable; the card refuses on its own too.
        for url in [
            "javascript:alert(1)",
            "https://user:pw@example.com/",
            "https://exa\u{202E}mple.com/",
            "https://example.com\\@evil.test/",
        ] {
            let (card, _) = link_card(url);
            let launched = Rc::new(Cell::new(false));
            let flag = launched.clone();
            card.set_launcher(Rc::new(move |_, _, _| flag.set(true)));
            card.button("Open in Browser").expect("open").emit_clicked();
            assert!(!launched.get(), "{url}");
        }
    }
}
