//! Native panels behind the built-in commands: model picker, MCP servers, settings, usage and
//! context. Each is an `adw::Dialog` (libadwaita 1.5) that asks the backend through
//! [`ChatBackend::control`] and fills in when the matching `ControlResult` arrives.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use agent_core::adapter::{Control, Driver};
use gtk4::glib;
use serde_json::Value;

use super::cards::{driver_name, label};
use super::header::gauge_text;
use super::model::{format_tokens, Gauge};
use super::payload;
use crate::chat::ChatBackend;

type Reply = Box<dyn FnOnce(Result<Value, String>)>;

/// Routes `ControlResult`s to whoever asked, by request id.
#[derive(Default)]
pub struct Requests {
    pending: RefCell<HashMap<String, Reply>>,
    /// Replies that arrived before their callback was registered (a backend answering
    /// synchronously from inside `control`).
    early: RefCell<HashMap<String, Result<Value, String>>>,
}

impl Requests {
    pub fn ask(
        &self,
        backend: &dyn ChatBackend,
        control: Control,
        reply: impl FnOnce(Result<Value, String>) + 'static,
    ) -> String {
        let id = backend.control(control);
        let early = self.early.borrow_mut().remove(&id);
        match early {
            Some(result) => reply(result),
            None => {
                self.pending
                    .borrow_mut()
                    .insert(id.clone(), Box::new(reply));
            }
        }
        id
    }

    /// Returns whether the reply had an owner here.
    pub fn resolve(&self, id: &str, result: Result<Value, String>) -> bool {
        let reply = self.pending.borrow_mut().remove(id);
        match reply {
            Some(f) => {
                f(result);
                true
            }
            None => {
                // Keep a bounded number of unclaimed replies for the synchronous case.
                let mut early = self.early.borrow_mut();
                if early.len() > 32 {
                    early.clear();
                }
                early.insert(id.to_owned(), result);
                false
            }
        }
    }
}

/// What panels need from the view.
#[derive(Clone)]
pub struct PanelCtx {
    pub backend: Rc<dyn ChatBackend>,
    pub requests: Rc<Requests>,
    pub parent: gtk4::Widget,
}

fn dialog(title: &str, width: i32, height: i32) -> (adw::Dialog, gtk4::Stack) {
    let d = adw::Dialog::new();
    d.set_title(title);
    d.set_content_width(width);
    d.set_content_height(height);
    d.add_css_class("chat-panel");
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let stack = gtk4::Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
    let spinner = gtk4::Spinner::new();
    spinner.set_spinning(true);
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk4::Align::Center);
    spinner.set_valign(gtk4::Align::Center);
    stack.add_named(&spinner, Some("loading"));
    toolbar.set_content(Some(&stack));
    d.set_child(Some(&toolbar));
    (d, stack)
}

fn show_error(stack: &gtk4::Stack, message: &str) {
    let page = adw::StatusPage::new();
    page.set_icon_name(Some("dialog-error-symbolic"));
    page.set_title("Could not load this panel");
    page.set_description(Some(&glib::markup_escape_text(message)));
    replace_page(stack, "error", &page);
}

fn replace_page(stack: &gtk4::Stack, name: &str, widget: &impl IsA<gtk4::Widget>) {
    if let Some(old) = stack.child_by_name(name) {
        stack.remove(&old);
    }
    stack.add_named(widget, Some(name));
    stack.set_visible_child_name(name);
}

fn scrolled(child: &impl IsA<gtk4::Widget>) -> gtk4::ScrolledWindow {
    let s = gtk4::ScrolledWindow::new();
    s.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    s.set_vexpand(true);
    s.set_child(Some(child));
    s
}

fn boxed_list() -> gtk4::ListBox {
    let l = gtk4::ListBox::new();
    l.set_selection_mode(gtk4::SelectionMode::None);
    l.add_css_class("boxed-list");
    l
}

fn padded(child: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    b.set_margin_top(12);
    b.set_margin_bottom(18);
    b.set_margin_start(18);
    b.set_margin_end(18);
    b.append(child);
    b
}

// ---------------------------------------------------------------------------------------------
// Model picker
// ---------------------------------------------------------------------------------------------

pub fn model_picker(ctx: &PanelCtx) {
    let status = ctx.backend.status();
    let (d, stack) = dialog("Model", 480, 560);

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    let switch = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    switch.add_css_class("linked");
    switch.add_css_class("agent-switch");
    switch.set_halign(gtk4::Align::Center);
    switch.set_margin_top(12);
    let claude = gtk4::ToggleButton::with_label(driver_name(Driver::Claude));
    let agy = gtk4::ToggleButton::with_label(driver_name(Driver::Agy));
    agy.set_group(Some(&claude));
    switch.append(&claude);
    switch.append(&agy);
    outer.append(&switch);
    // The stack moves from the toolbar into `outer`, under the agent switch.
    if let Some(toolbar) = d.child().and_downcast::<adw::ToolbarView>() {
        toolbar.set_content(None::<&gtk4::Widget>);
        outer.append(&stack);
        toolbar.set_content(Some(&outer));
    }
    stack.set_vexpand(true);

    // Page for the other agent: a handoff, not a list we can query from here.
    let handoff_page = |target: Driver| {
        let page = adw::StatusPage::new();
        page.add_css_class("compact");
        page.set_icon_name(Some("system-switch-user-symbolic"));
        page.set_title(&format!("Continue in {}", driver_name(target)));
        page.set_description(Some(
            "The thread is handed over with a budgeted, redacted summary of the conversation. \
             You can switch back at any time.",
        ));
        let go = gtk4::Button::with_label(&format!("Switch to {}", driver_name(target)));
        go.add_css_class("suggested-action");
        go.add_css_class("pill");
        go.set_halign(gtk4::Align::Center);
        page.set_child(Some(&go));
        (page, go)
    };

    let current = status.driver;
    let other = match current {
        Driver::Claude => Driver::Agy,
        Driver::Agy => Driver::Claude,
    };
    let (other_page, go) = handoff_page(other);
    stack.add_named(&other_page, Some("other"));
    let backend = ctx.backend.clone();
    go.connect_clicked(glib::clone!(
        #[weak]
        d,
        move |_| {
            backend.switch(other, None);
            d.close();
        }
    ));

    match current {
        Driver::Claude => claude.set_active(true),
        Driver::Agy => agy.set_active(true),
    }
    let on_toggle = glib::clone!(
        #[weak]
        stack,
        #[weak]
        claude,
        move |_: &gtk4::ToggleButton| {
            let picked = if claude.is_active() {
                Driver::Claude
            } else {
                Driver::Agy
            };
            if picked == current {
                let name = if stack.child_by_name("models").is_some() {
                    "models"
                } else if stack.child_by_name("error").is_some() {
                    "error"
                } else {
                    "loading"
                };
                stack.set_visible_child_name(name);
            } else {
                stack.set_visible_child_name("other");
            }
        }
    );
    claude.connect_toggled(on_toggle.clone());
    agy.connect_toggled(on_toggle);

    if status.capabilities.model_list {
        let backend = ctx.backend.clone();
        let current_model = status.model.clone();
        ctx.requests.ask(
            ctx.backend.as_ref(),
            Control::ListModels,
            glib::clone!(
                #[weak]
                stack,
                #[weak]
                d,
                #[weak]
                claude,
                move |result| {
                    let models = match result {
                        Ok(v) => payload::models(&v),
                        Err(e) => {
                            show_error(&stack, &e);
                            return;
                        }
                    };
                    if models.is_empty() {
                        show_error(&stack, "The agent returned no models.");
                        return;
                    }
                    let list = boxed_list();
                    for m in models {
                        let row = adw::ActionRow::new();
                        row.set_use_markup(false);
                        row.set_title(&m.label);
                        let sub = match &m.description {
                            Some(desc) => format!("{} · {desc}", m.id),
                            None => m.id.clone(),
                        };
                        row.set_subtitle(&sub);
                        row.set_activatable(true);
                        if current_model.as_deref() == Some(m.id.as_str()) {
                            let check = gtk4::Image::from_icon_name("object-select-symbolic");
                            check.add_css_class("accent");
                            row.add_suffix(&check);
                        }
                        let backend = backend.clone();
                        let id = m.id.clone();
                        row.connect_activated(glib::clone!(
                            #[weak]
                            d,
                            move |_| {
                                backend.switch(current, Some(id.clone()));
                                d.close();
                            }
                        ));
                        list.append(&row);
                    }
                    let page = scrolled(&padded(&list));
                    if let Some(old) = stack.child_by_name("models") {
                        stack.remove(&old);
                    }
                    stack.add_named(&page, Some("models"));
                    let picked = if claude.is_active() {
                        Driver::Claude
                    } else {
                        Driver::Agy
                    };
                    if picked == current {
                        stack.set_visible_child_name("models");
                    }
                }
            ),
        );
    } else {
        show_error(
            &stack,
            &format!("{} cannot list its models.", driver_name(current)),
        );
    }
    d.present(Some(&ctx.parent));
}

// ---------------------------------------------------------------------------------------------
// MCP servers
// ---------------------------------------------------------------------------------------------

pub fn mcp_panel(ctx: &PanelCtx) {
    let (d, stack) = dialog("MCP servers", 520, 520);
    load_mcp(ctx, &stack);
    d.present(Some(&ctx.parent));
}

fn load_mcp(ctx: &PanelCtx, stack: &gtk4::Stack) {
    let ctx2 = ctx.clone();
    ctx.requests.ask(
        ctx.backend.as_ref(),
        Control::McpStatus,
        glib::clone!(
            #[weak]
            stack,
            move |result| {
                let servers = match result {
                    Ok(v) => payload::mcp_servers(&v),
                    Err(e) => {
                        show_error(&stack, &e);
                        return;
                    }
                };
                if servers.is_empty() {
                    let page = adw::StatusPage::new();
                    page.set_icon_name(Some("network-server-symbolic"));
                    page.set_title("No MCP servers");
                    page.set_description(Some("This session has no MCP servers configured."));
                    replace_page(&stack, "list", &page);
                    return;
                }
                let list = boxed_list();
                for s in servers {
                    let row = adw::ActionRow::new();
                    row.set_use_markup(false);
                    row.set_title(&s.name);
                    let sub = match &s.detail {
                        Some(detail) => format!("{} · {detail}", s.status),
                        None => s.status.clone(),
                    };
                    row.set_subtitle(&sub);
                    let dot = label(
                        "●",
                        &["mcp-dot", &format!("mcp-{}", status_class(&s.status))],
                    );
                    row.add_prefix(&dot);

                    let reconnect = gtk4::Button::from_icon_name("view-refresh-symbolic");
                    reconnect.add_css_class("flat");
                    reconnect.set_valign(gtk4::Align::Center);
                    reconnect.set_tooltip_text(Some("Reconnect"));
                    reconnect.set_sensitive(s.enabled);
                    let ctx3 = ctx2.clone();
                    let name = s.name.clone();
                    reconnect.connect_clicked(glib::clone!(
                        #[weak]
                        stack,
                        move |b| {
                            b.set_sensitive(false);
                            let ctx4 = ctx3.clone();
                            ctx3.requests.ask(
                                ctx3.backend.as_ref(),
                                Control::McpReconnect {
                                    server: name.clone(),
                                },
                                glib::clone!(
                                    #[weak]
                                    stack,
                                    move |_| load_mcp(&ctx4, &stack)
                                ),
                            );
                        }
                    ));
                    row.add_suffix(&reconnect);

                    let toggle = gtk4::Switch::new();
                    toggle.set_active(s.enabled);
                    toggle.set_valign(gtk4::Align::Center);
                    toggle.set_tooltip_text(Some("Enabled for this session"));
                    let ctx3 = ctx2.clone();
                    let name = s.name.clone();
                    toggle.connect_state_set(glib::clone!(
                        #[weak]
                        stack,
                        #[upgrade_or]
                        glib::Propagation::Proceed,
                        move |sw, state| {
                            sw.set_sensitive(false);
                            let ctx4 = ctx3.clone();
                            ctx3.requests.ask(
                                ctx3.backend.as_ref(),
                                Control::McpToggle {
                                    server: name.clone(),
                                    enabled: state,
                                },
                                glib::clone!(
                                    #[weak]
                                    stack,
                                    move |_| load_mcp(&ctx4, &stack)
                                ),
                            );
                            glib::Propagation::Proceed
                        }
                    ));
                    row.add_suffix(&toggle);
                    list.append(&row);
                }
                replace_page(&stack, "list", &scrolled(&padded(&list)));
            }
        ),
    );
}

fn status_class(status: &str) -> &'static str {
    match status {
        "connected" | "ok" | "running" => "ok",
        "pending" | "connecting" | "needs-auth" => "pending",
        "disabled" => "off",
        _ => "bad",
    }
}

// ---------------------------------------------------------------------------------------------
// Settings / usage / context
// ---------------------------------------------------------------------------------------------

/// A read-only key/value view of a JSON document, grouped by top-level key.
fn json_page(v: &Value, note: Option<&str>) -> gtk4::Widget {
    let page = adw::PreferencesPage::new();
    if let Some(note) = note {
        let group = adw::PreferencesGroup::new();
        group.set_description(Some(note));
        page.add(&group);
    }
    let mut groups: Vec<(String, adw::PreferencesGroup)> = Vec::new();
    for (path, value) in payload::flatten(v, 240) {
        // Top-level scalars share one "General" group rather than a group each.
        let nested = path.contains(['.', '[']);
        let top = if nested {
            path.split(['.', '[']).next().unwrap_or(&path).to_owned()
        } else {
            "General".to_owned()
        };
        let group = match groups.iter().find(|(k, _)| *k == top) {
            Some((_, g)) => g.clone(),
            None => {
                let g = adw::PreferencesGroup::new();
                g.set_title(&glib::markup_escape_text(&top));
                page.add(&g);
                groups.push((top.clone(), g.clone()));
                g
            }
        };
        let row = adw::ActionRow::new();
        row.set_use_markup(false);
        let rest = if nested {
            path.strip_prefix(&top)
                .unwrap_or(&path)
                .trim_start_matches('.')
        } else {
            &path
        };
        row.set_title(rest);
        row.set_subtitle(&value);
        row.set_subtitle_selectable(true);
        row.add_css_class("json-row");
        group.add(&row);
    }
    if groups.is_empty() {
        let g = adw::PreferencesGroup::new();
        g.set_description(Some("Nothing to show."));
        page.add(&g);
    }
    page.upcast()
}

pub fn settings_panel(ctx: &PanelCtx) {
    let (d, stack) = dialog("Settings", 560, 600);
    ctx.requests.ask(
        ctx.backend.as_ref(),
        Control::GetSettings,
        glib::clone!(
            #[weak]
            stack,
            move |result| match result {
                Ok(v) => replace_page(
                    &stack,
                    "page",
                    &json_page(
                        payload::effective_settings(&v),
                        Some("Effective settings, read-only. Edit them in the agent's own settings files."),
                    ),
                ),
                Err(e) => show_error(&stack, &e),
            }
        ),
    );
    d.present(Some(&ctx.parent));
}

pub fn usage_panel(ctx: &PanelCtx) {
    let (d, stack) = dialog("Usage", 520, 520);
    ctx.requests.ask(
        ctx.backend.as_ref(),
        Control::Usage,
        glib::clone!(
            #[weak]
            stack,
            move |result| match result {
                Ok(v) => replace_page(&stack, "page", &json_page(&v, None)),
                Err(e) => show_error(&stack, &e),
            }
        ),
    );
    d.present(Some(&ctx.parent));
}

pub fn context_panel(ctx: &PanelCtx, fallback: Option<Gauge>) {
    let (d, stack) = dialog("Context window", 480, 520);
    ctx.requests.ask(
        ctx.backend.as_ref(),
        Control::ContextUsage,
        glib::clone!(
            #[weak]
            stack,
            move |result| {
                let v = match result {
                    Ok(v) => v,
                    Err(e) => {
                        show_error(&stack, &e);
                        return;
                    }
                };
                let c = payload::context_breakdown(&v);
                let gauge = match (c.used, fallback) {
                    (Some(used), _) => Some(Gauge {
                        used,
                        max: c.max,
                        auto_compact_at: c.auto_compact_at,
                    }),
                    (None, g) => g,
                };
                let content = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
                if let Some(g) = gauge {
                    let big = label(&gauge_text(&g), &["context-big"]);
                    big.set_halign(gtk4::Align::Center);
                    content.append(&big);
                    if let Some((fill, _)) = super::header::gauge_fraction(&g) {
                        let level = gtk4::LevelBar::new();
                        level.set_value(fill);
                        level.add_css_class("context-level");
                        content.append(&level);
                    }
                    if let Some(at) = g.auto_compact_at {
                        let note = label(
                            &format!("Auto-compacts at {}", format_tokens(at)),
                            &["dim-label"],
                        );
                        note.set_halign(gtk4::Align::Center);
                        content.append(&note);
                    }
                }
                if c.categories.is_empty() {
                    content.append(&json_page(&v, None));
                } else {
                    let list = boxed_list();
                    for (name, tokens) in &c.categories {
                        let row = adw::ActionRow::new();
                        row.set_use_markup(false);
                        row.set_title(name);
                        let t = label(&format_tokens(*tokens), &["mono-text"]);
                        row.add_suffix(&t);
                        list.append(&row);
                    }
                    content.append(&list);
                }
                replace_page(&stack, "page", &scrolled(&padded(&content)));
            }
        ),
    );
    d.present(Some(&ctx.parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_status_classes() {
        assert_eq!(status_class("connected"), "ok");
        assert_eq!(status_class("disabled"), "off");
        assert_eq!(status_class("needs-auth"), "pending");
        assert_eq!(status_class("failed"), "bad");
    }
}
