//! Native panels behind the built-in commands: model picker, MCP servers, settings, usage and
//! context. Each is an `adw::Dialog` (libadwaita 1.5) that asks the backend through
//! [`ChatBackend::control`] and fills in when the matching `ControlResult` arrives.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use agent_core::adapter::{Control, Driver};
use agent_core::catalog::{group_for_picker, CatalogModel};
use gtk4::glib;
use serde_json::Value;

use super::cards::{driver_name, label};
use super::header::gauge_text;
use super::model::{format_tokens, Gauge};
use super::payload;
use crate::chat::{ChatBackend, ModelSource, SessionStatus};

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
    /// Where the model picker gets both agents' lists (`None`: ask the backend).
    pub models: Option<Rc<dyn ModelSource>>,
    /// The open picker's refresh hook, called when the source changes. The view connects to the
    /// source once and forwards here, so closed pickers leave nothing behind.
    pub model_listener: ModelListener,
}

pub type ModelListener = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

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

/// The group heading for an agent's models.
fn group_title(driver: Driver) -> &'static str {
    driver.info().long_label
}

/// The fallback list, when there is no [`ModelSource`]: the backend's own `ListModels` answer,
/// all attributed to the current agent.
fn catalog_from_payload(driver: Driver, entries: Vec<payload::ModelEntry>) -> Vec<CatalogModel> {
    entries
        .into_iter()
        .map(|m| CatalogModel {
            driver,
            id: m.id,
            display: m.label,
            description: m.description,
            efforts: Vec::new(),
            default_effort: None,
            via: None,
        })
        .collect()
}

/// Whether `m` is the model the thread is on now (agy rows match at any of their efforts).
fn is_current(m: &CatalogModel, status: &SessionStatus) -> bool {
    m.driver == status.driver && status.model.as_deref().is_some_and(|id| m.is_model(id))
}

/// The effort a row starts on: the thread's own when this is its model, else the model's default.
fn initial_effort(m: &CatalogModel, status: &SessionStatus) -> Option<String> {
    let current = if !is_current(m, status) {
        None
    } else if m.driver == Driver::Agy {
        status.model.as_deref().and_then(|id| m.effort_in(id))
    } else {
        status.effort.as_deref()
    };
    current
        .or_else(|| m.default_effort())
        .filter(|_| !m.efforts.is_empty())
        .map(str::to_owned)
}

/// What choosing a row hands to `switch`: the model value and the effort. agy carries the effort
/// in the id (`<base>-<effort>`). For Claude the effort is separate, and it is sent only when the
/// user picked one on this row or the thread already has one; otherwise the row's default would
/// needlessly restart a session that never had an effort.
fn choice(
    m: &CatalogModel,
    effort: Option<&str>,
    touched: bool,
    status: &SessionStatus,
) -> (String, Option<String>) {
    let effort = effort.filter(|_| !m.efforts.is_empty());
    match m.driver {
        Driver::Agy => (m.model_id_for(effort), effort.map(str::to_owned)),
        Driver::Claude => {
            let send = touched || status.effort.is_some();
            (m.id.clone(), effort.filter(|_| send).map(str::to_owned))
        }
        // The effort rides on every turn, so sending it never restarts anything.
        Driver::Codex => (m.id.clone(), effort.map(str::to_owned)),
    }
}

/// `pick(driver, model value, effort)`.
type Pick = Rc<dyn Fn(Driver, String, Option<String>)>;

/// The grouped, filtered rows for `query`. Every row has the same UX for both agents: the name
/// and, when the model offers efforts, an effort dropdown; choosing the row or changing its
/// effort calls `pick`.
fn model_list(
    models: &[CatalogModel],
    query: &str,
    status: &SessionStatus,
    pick: &Pick,
) -> gtk4::Widget {
    let groups = group_for_picker(models, query);
    if groups.is_empty() {
        let page = adw::StatusPage::new();
        page.add_css_class("compact");
        page.set_icon_name(Some("system-search-symbolic"));
        page.set_title(if models.is_empty() {
            "No models yet"
        } else {
            "No models match"
        });
        page.set_description(Some(if models.is_empty() {
            "The model lists are fetched in the background and appear here when they arrive."
        } else {
            "Try a different search."
        }));
        return page.upcast();
    }
    let status = Rc::new(status.clone()); // shared by every row's handlers
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    for (driver, rows) in groups {
        let heading = label(&group_title(driver).to_uppercase(), &["model-group"]);
        heading.set_halign(gtk4::Align::Start);
        heading.set_margin_top(6);
        heading.set_margin_start(6);
        column.append(&heading);
        let list = boxed_list();
        for m in rows {
            let row = adw::ActionRow::new();
            row.set_use_markup(false);
            row.set_title(&m.display);
            let sub = match &m.description {
                Some(desc) => format!("{} · {desc}", m.id),
                None => m.id.clone(),
            };
            row.set_subtitle(&sub);
            row.set_activatable(true);
            if let Some(via) = &m.via {
                let tag = label(&format!("via {via}"), &["model-via"]);
                tag.set_valign(gtk4::Align::Center);
                row.add_suffix(&tag);
            }
            // The effort shown on the row; `touched` once the user has picked one here.
            let effort: Rc<RefCell<Option<String>>> =
                Rc::new(RefCell::new(initial_effort(m, &status)));
            let touched = Rc::new(Cell::new(false));
            if !m.efforts.is_empty() {
                let efforts: Vec<&str> = m.efforts.iter().map(String::as_str).collect();
                let dropdown = gtk4::DropDown::from_strings(&efforts);
                dropdown.set_valign(gtk4::Align::Center);
                dropdown.set_tooltip_text(Some("Effort"));
                let at = effort
                    .borrow()
                    .as_deref()
                    .and_then(|e| m.efforts.iter().position(|x| x == e));
                dropdown.set_selected(at.and_then(|i| u32::try_from(i).ok()).unwrap_or(0));
                // Changing the effort switches straight away, like choosing the row.
                dropdown.connect_selected_notify({
                    let (m, effort, touched, pick, status) = (
                        m.clone(),
                        effort.clone(),
                        touched.clone(),
                        pick.clone(),
                        status.clone(),
                    );
                    move |dd| {
                        let chosen = m.efforts.get(dd.selected() as usize).cloned();
                        if *effort.borrow() == chosen {
                            return;
                        }
                        *effort.borrow_mut() = chosen;
                        touched.set(true);
                        let (id, e) = choice(&m, effort.borrow().as_deref(), true, &status);
                        pick(m.driver, id, e);
                    }
                });
                row.add_suffix(&dropdown);
            }
            if is_current(m, &status) {
                row.add_css_class("current-model");
                let check = gtk4::Image::from_icon_name("object-select-symbolic");
                check.add_css_class("accent");
                row.add_suffix(&check);
            }
            let (pick, m, status) = (pick.clone(), m.clone(), status.clone());
            row.connect_activated(move |_| {
                let (id, e) = choice(&m, effort.borrow().as_deref(), touched.get(), &status);
                pick(m.driver, id, e);
            });
            list.append(&row);
        }
        column.append(&list);
    }
    column.upcast()
}

/// The model picker: ONE searchable list of every model of both agents, so a thread can jump
/// from an agy Gemini to a Claude model (or back) in a single click. Picking a row of the other
/// agent is a handoff, which the backend's `switch` performs.
pub fn model_picker(ctx: &PanelCtx) {
    let status = ctx.backend.status();
    let (d, stack) = dialog("Model", 520, 640);

    let search = gtk4::SearchEntry::new();
    search.set_placeholder_text(Some("Search models"));
    search.set_margin_top(12);
    search.set_margin_start(18);
    search.set_margin_end(18);
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    outer.append(&search);
    // The stack moves from the toolbar into `outer`, under the search entry.
    if let Some(toolbar) = d.child().and_downcast::<adw::ToolbarView>() {
        toolbar.set_content(None::<&gtk4::Widget>);
        outer.append(&stack);
        toolbar.set_content(Some(&outer));
    }
    stack.set_vexpand(true);

    let models: Rc<RefCell<Vec<CatalogModel>>> = Rc::default();
    let pick: Pick = {
        let backend = ctx.backend.clone();
        let d = d.downgrade();
        Rc::new(move |driver, id, effort| {
            backend.switch(driver, Some(id), effort);
            if let Some(d) = d.upgrade() {
                d.close();
            }
        })
    };
    let render: Rc<dyn Fn()> = {
        let (models, backend) = (models.clone(), ctx.backend.clone());
        let (stack, search) = (stack.downgrade(), search.downgrade());
        Rc::new(move || {
            let (Some(stack), Some(search)) = (stack.upgrade(), search.upgrade()) else {
                return;
            };
            let list = model_list(&models.borrow(), &search.text(), &backend.status(), &pick);
            replace_page(&stack, "models", &scrolled(&padded(&list)));
        })
    };
    search.connect_search_changed({
        let render = render.clone();
        move |_| render()
    });

    match &ctx.models {
        Some(source) => {
            *models.borrow_mut() = source.models();
            render();
            // Keep the list live while the picker is open; the view owns the single connection
            // to the source and forwards to whoever is registered here.
            let live = {
                let (source, models, render) = (source.clone(), models.clone(), render.clone());
                Rc::new(move || {
                    *models.borrow_mut() = source.models();
                    render();
                }) as Rc<dyn Fn()>
            };
            *ctx.model_listener.borrow_mut() = Some(live);
            let slot = ctx.model_listener.clone();
            d.connect_closed(move |_| {
                slot.borrow_mut().take();
            });
        }
        None if status.capabilities.model_list => {
            let driver = status.driver;
            ctx.requests.ask(
                ctx.backend.as_ref(),
                Control::ListModels,
                glib::clone!(
                    #[weak]
                    stack,
                    move |result| match result {
                        Ok(v) => {
                            *models.borrow_mut() =
                                catalog_from_payload(driver, payload::models(&v));
                            render();
                        }
                        Err(e) => show_error(&stack, &e),
                    }
                ),
            );
        }
        None => show_error(
            &stack,
            &format!("{} cannot list its models.", driver_name(status.driver)),
        ),
    }
    d.present(Some(&ctx.parent));
    search.grab_focus();
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

    fn status(driver: Driver, model: Option<&str>) -> SessionStatus {
        SessionStatus {
            driver,
            model: model.map(str::to_owned),
            effort: None,
            mode: agent_core::adapter::Mode::Ask,
            running_turn: false,
            alive: true,
            capabilities: agent_core::caps::Capabilities::claude(),
            commands: Vec::new(),
        }
    }

    #[test]
    fn fallback_catalog_is_attributed_to_the_current_agent() {
        let entries = payload::models(&serde_json::json!({"models": [
            {"value": "opus", "displayName": "Opus", "description": "Big"}]}));
        let m = catalog_from_payload(Driver::Claude, entries);
        assert_eq!(m.len(), 1);
        assert_eq!(
            (m[0].driver, m[0].id.as_str(), m[0].display.as_str()),
            (Driver::Claude, "opus", "Opus")
        );
        assert_eq!(m[0].description.as_deref(), Some("Big"));
    }

    #[test]
    fn current_model_needs_the_same_agent_and_id() {
        let m = CatalogModel {
            driver: Driver::Agy,
            id: "gemini-3.1-pro-high".into(),
            display: "G".into(),
            description: None,
            efforts: vec![],
            default_effort: None,
            via: None,
        };
        assert!(is_current(
            &m,
            &status(Driver::Agy, Some("gemini-3.1-pro-high"))
        ));
        assert!(!is_current(
            &m,
            &status(Driver::Claude, Some("gemini-3.1-pro-high"))
        ));
        assert!(!is_current(&m, &status(Driver::Agy, None)));
        assert_eq!(group_title(Driver::Claude), "Claude");
        assert_eq!(group_title(Driver::Agy), "Antigravity (agy)");
        assert_eq!(group_title(Driver::Codex), "Codex");
    }

    #[test]
    fn a_codex_row_always_sends_its_effort_and_starts_on_the_models_default() {
        let mut gpt = model(Driver::Codex, "gpt-5-codex", &["low", "high"]);
        gpt.default_effort = Some("high".into());
        let st = status(Driver::Claude, Some("opus"));
        assert_eq!(initial_effort(&gpt, &st).as_deref(), Some("high"));
        assert_eq!(
            choice(&gpt, Some("low"), false, &st),
            ("gpt-5-codex".to_owned(), Some("low".to_owned()))
        );
    }

    fn model(driver: Driver, id: &str, efforts: &[&str]) -> CatalogModel {
        CatalogModel {
            driver,
            id: id.into(),
            display: id.into(),
            description: None,
            efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
            default_effort: None,
            via: None,
        }
    }

    #[test]
    fn an_agy_row_matches_the_thread_at_any_effort_and_starts_on_its_effort() {
        let flash = model(Driver::Agy, "gemini-3.8-flash", &["low", "medium", "high"]);
        let on_high = status(Driver::Agy, Some("gemini-3.8-flash-high"));
        assert!(is_current(&flash, &on_high));
        assert_eq!(initial_effort(&flash, &on_high).as_deref(), Some("high"));
        // Another row starts on the model's default (medium).
        let pro = model(Driver::Agy, "gemini-3.1-pro", &["low", "high"]);
        assert!(!is_current(&pro, &on_high));
        assert_eq!(initial_effort(&pro, &on_high).as_deref(), Some("low"));
        let bare = model(Driver::Agy, "plain", &[]);
        assert_eq!(initial_effort(&bare, &on_high), None);
    }

    #[test]
    fn choosing_an_agy_row_composes_the_id_and_keeps_the_effort() {
        let flash = model(Driver::Agy, "gemini-3.8-flash", &["low", "medium", "high"]);
        let st = status(Driver::Agy, Some("x"));
        assert_eq!(
            choice(&flash, Some("low"), false, &st),
            ("gemini-3.8-flash-low".to_owned(), Some("low".to_owned()))
        );
        let bare = model(Driver::Agy, "plain", &[]);
        assert_eq!(choice(&bare, None, false, &st), ("plain".to_owned(), None));
    }

    #[test]
    fn a_claude_row_sends_effort_only_when_it_matters() {
        let sonnet = model(Driver::Claude, "sonnet", &["low", "medium", "high"]);
        let mut st = status(Driver::Claude, Some("opus"));
        // The thread has no effort and the user did not touch the dropdown: model only.
        assert_eq!(
            choice(&sonnet, Some("medium"), false, &st),
            ("sonnet".to_owned(), None)
        );
        // The user picked one: send it.
        assert_eq!(
            choice(&sonnet, Some("high"), true, &st),
            ("sonnet".to_owned(), Some("high".to_owned()))
        );
        // The thread has an effort: rows start on it, and it is sent (unchanged = in session).
        st.effort = Some("high".into());
        assert_eq!(
            initial_effort(&model(Driver::Claude, "opus", &["low", "high"]), &st).as_deref(),
            Some("high")
        );
        assert_eq!(
            choice(&sonnet, Some("high"), false, &st),
            ("sonnet".to_owned(), Some("high".to_owned()))
        );
    }

    #[test]
    fn mcp_status_classes() {
        assert_eq!(status_class("connected"), "ok");
        assert_eq!(status_class("disabled"), "off");
        assert_eq!(status_class("needs-auth"), "pending");
        assert_eq!(status_class("failed"), "bad");
    }
}
