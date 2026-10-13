//! The thread sidebar's context menu (right-click, the Menu key, long press) and its actions:
//! continue a thread in another agent, switch it in place, rename, archive, delete, open its
//! folder, copy its id; plus the Delete key.
//!
//! The menu is data: [`thread_menu`] builds a model, [`to_gio_menu`] turns it into a `gio::Menu`
//! whose items all activate one window action (`win.thread-menu`) with the encoded
//! [`ThreadAction`] as their target. No closure captures a widget, and a disabled entry points at
//! a disabled action (`win.thread-menu-off`), which GTK draws insensitive.
//!
//! Deleting removes the thread's rows from this app's store and nothing else: an agent's own
//! session files are never touched.

use std::rc::Rc;

use agent_core::adapter::Driver;
use gtk4::gio;

use super::threads::{store_job, NewThread, Sidebar};
use super::*;
use crate::chat::session::build_handoff;
use crate::chat::ChatBackend;
use crate::model_catalog::ModelCatalog;
use crate::window::sidebar_model::{
    driver_label, menu_agents, thread_menu, MenuAgent, MenuEntry, RowKey, ThreadAction,
    ThreadMenuInput,
};

/// The action every enabled menu item activates, with the encoded [`ThreadAction`] as target.
const ACTION: &str = "win.thread-menu";
/// The never-enabled twin that disabled items point at.
const ACTION_OFF: &str = "win.thread-menu-off";

/// A `gio::Menu` for `entries`: sections as sections, submenus as submenus, items as items.
fn to_gio_menu(entries: &[MenuEntry]) -> gio::Menu {
    let menu = gio::Menu::new();
    for entry in entries {
        match entry {
            MenuEntry::Item {
                label,
                action,
                enabled,
            } => {
                let item = gio::MenuItem::new(Some(label), None);
                item.set_action_and_target_value(
                    Some(if *enabled { ACTION } else { ACTION_OFF }),
                    Some(&action.encode().to_variant()),
                );
                menu.append_item(&item);
            }
            // GTK's PopoverMenu draws no icon on a submenu entry (verified in the preview), so
            // agent submenus are plain labels: the brand icons live in the widgets, not here.
            MenuEntry::Submenu { label, entries, .. } => {
                menu.append_submenu(Some(label), &to_gio_menu(entries));
            }
            MenuEntry::Section(entries) => menu.append_section(None, &to_gio_menu(entries)),
        }
    }
    menu
}

impl AgentTerminalWindow {
    /// Registers the menu's actions on the window. Called once, from the shell's setup.
    pub(super) fn setup_thread_menu_actions(&self) {
        let obj = self.obj();
        let run = gio::SimpleAction::new("thread-menu", Some(&String::static_variant_type()));
        run.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, target| {
                let action = target
                    .and_then(|t| t.get::<String>())
                    .and_then(|s| ThreadAction::decode(&s));
                if let Some(action) = action {
                    obj.imp().run_thread_action(action);
                }
            }
        ));
        obj.add_action(&run);
        let off = gio::SimpleAction::new("thread-menu-off", Some(&String::static_variant_type()));
        off.set_enabled(false);
        obj.add_action(&off);
    }

    /// Right-click, long press, the Menu key (and Shift+F10) open the menu of the row they hit;
    /// Delete asks to delete the selected thread.
    pub(super) fn install_thread_menu_triggers(&self, sidebar: &Rc<Sidebar>) {
        let obj = self.obj();
        let list = sidebar.list.clone();

        let key_of_row = |sidebar: &Rc<Sidebar>, row: &gtk4::ListBoxRow| -> Option<RowKey> {
            usize::try_from(row.index())
                .ok()
                .and_then(|i| sidebar.keys.borrow().get(i).cloned().flatten())
        };

        // Pointer: where the click landed, in the list's coordinates.
        let at_point = {
            let weak_sidebar = Rc::downgrade(sidebar);
            glib::clone!(
                #[weak]
                obj,
                #[weak]
                list,
                move |x: f64, y: f64| {
                    let Some(sidebar) = weak_sidebar.upgrade() else {
                        return;
                    };
                    let Some(row) = list.row_at_y(y as i32) else {
                        return;
                    };
                    let Some(RowKey::Thread(thread)) = key_of_row(&sidebar, &row) else {
                        return;
                    };
                    obj.imp().open_thread_menu(thread, &row, Some((x, y)));
                }
            )
        };
        let at_point = Rc::new(at_point);

        let click = gtk4::GestureClick::new();
        click.set_button(3);
        click.connect_pressed({
            let at_point = at_point.clone();
            move |gesture, _, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                at_point(x, y);
            }
        });
        list.add_controller(click);

        let long_press = gtk4::GestureLongPress::new();
        long_press.set_touch_only(true);
        long_press.connect_pressed({
            let at_point = at_point.clone();
            move |gesture, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                at_point(x, y);
            }
        });
        list.add_controller(long_press);

        // Keyboard: the selected row.
        let keys = gtk4::EventControllerKey::new();
        let weak_sidebar = Rc::downgrade(sidebar);
        keys.connect_key_pressed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            list,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _, state| {
                use gtk4::gdk::Key;
                let menu_key = keyval == Key::Menu
                    || (keyval == Key::F10 && state.contains(gtk4::gdk::ModifierType::SHIFT_MASK));
                let delete = matches!(keyval, Key::Delete | Key::KP_Delete);
                if !menu_key && !delete {
                    return glib::Propagation::Proceed;
                }
                let Some(sidebar) = weak_sidebar.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let Some(row) = list.selected_row() else {
                    return glib::Propagation::Proceed;
                };
                let Some(RowKey::Thread(thread)) = key_of_row(&sidebar, &row) else {
                    return glib::Propagation::Proceed;
                };
                if delete {
                    obj.imp().confirm_delete(&thread);
                } else {
                    obj.imp().open_thread_menu(thread, &row, None);
                }
                glib::Propagation::Stop
            }
        ));
        list.add_controller(keys);
    }

    /// The agents the menu offers: enabled and installed, each with its catalogue models.
    fn menu_agents(&self) -> Vec<MenuAgent> {
        menu_agents(&ModelCatalog::shared().models(), |d| self.agent_usable(d))
    }

    /// Pops the thread's menu up by `row`: at `at` (the pointer, in the list's coordinates), else
    /// below the row. Whether the thread has any message is read off the main thread first.
    ///
    /// The popover hangs off the list, never the row: the sidebar rebuilds its rows on any state
    /// change (focus moving to the popover is one), and a row finalized under an open menu took
    /// the menu with it.
    pub(super) fn open_thread_menu(
        &self,
        thread: String,
        row: &gtk4::ListBoxRow,
        at: Option<(f64, f64)>,
    ) {
        let Some(list) = self.sidebar.borrow().as_ref().map(|s| s.list.clone()) else {
            return;
        };
        // Where to point, fixed now: the row may be gone by the time the menu opens.
        let rect = match at {
            Some((x, y)) => gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1),
            None => match row.compute_bounds(&list) {
                Some(b) => gtk4::gdk::Rectangle::new(
                    b.x() as i32,
                    b.y() as i32,
                    b.width() as i32,
                    b.height() as i32,
                ),
                None => return,
            },
        };
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let has_messages = store_job({
                let thread = thread.clone();
                move |store| store.has_messages(&thread).unwrap_or(false)
            })
            .await
            .unwrap_or(false);
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let Some(summary) = imp.summary_of(&thread) else {
                return; // deleted meanwhile
            };
            let input = ThreadMenuInput {
                thread,
                archived: summary.archived,
                has_messages,
                agents: imp.menu_agents(),
            };
            let menu = to_gio_menu(&thread_menu(&input));
            let popover = gtk4::PopoverMenu::from_model(Some(&menu));
            popover.set_has_arrow(false);
            popover.set_halign(gtk4::Align::Start);
            popover.set_parent(&list);
            popover.set_pointing_to(Some(&rect));
            // Unparented once closed (not inside the signal itself).
            popover.connect_closed(|popover| {
                let popover = popover.clone();
                glib::idle_add_local_once(move || popover.unparent());
            });
            popover.popup();
        });
    }

    /// Carries out a menu choice.
    pub(super) fn run_thread_action(&self, action: ThreadAction) {
        match action {
            ThreadAction::ContinueIn {
                thread,
                driver,
                model,
                effort,
            } => self.continue_thread_in(thread, driver, model, effort),
            ThreadAction::SwitchTo {
                thread,
                driver,
                model,
                effort,
            } => self.switch_thread_to(&thread, driver, model, effort),
            ThreadAction::Rename(thread) => self.prompt_rename(&thread),
            ThreadAction::Archive(thread) => self.set_thread_archived(&thread, true),
            ThreadAction::Unarchive(thread) => self.set_thread_archived(&thread, false),
            ThreadAction::Delete(thread) => self.confirm_delete(&thread),
            ThreadAction::OpenFolder(thread) => self.open_thread_folder(&thread),
            ThreadAction::ReviewEnvironment(thread) => {
                if let Some(summary) = self.summary_of(&thread) {
                    let driver = self.slot_of(&thread).map(|slot| slot.driver()).or_else(|| {
                        summary
                            .driver
                            .as_deref()
                            .and_then(crate::window::sidebar_model::parse_driver)
                    });
                    self.confirm_environment_review(summary.cwd, driver);
                }
            }
            ThreadAction::CopyId(thread) => {
                self.obj().clipboard().set_text(&thread);
                self.show_toast("Copied the thread id");
            }
        }
    }

    /// What the sidebar calls the thread.
    pub(super) fn thread_title_of(&self, thread: &str) -> String {
        let open = self.tabs.borrow().iter().find_map(|t| {
            t.chat
                .as_ref()
                .filter(|c| c.thread == thread && !c.title.is_empty())
                .map(|c| c.title.clone())
        });
        open.or_else(|| {
            self.summary_of(thread)
                .map(|s| s.title)
                .filter(|t| !t.is_empty())
        })
        .unwrap_or_else(|| "New thread".to_owned())
    }

    // ---- continue / switch ----

    /// A NEW thread in the same folder on `driver`/`model`/`effort`, whose first message carries a
    /// budgeted, redacted handoff of this thread (the same path Fork uses). This thread is left
    /// as it is.
    fn continue_thread_in(
        &self,
        thread: String,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(summary) = self.summary_of(&thread) else {
            return;
        };
        let title = self.thread_title_of(&thread);
        // The new thread keeps this one's mode; only a thread with no session yet falls back to
        // the profile's.
        let mode = self
            .slot_of(&thread)
            .and_then(|s| s.get())
            .map(|s| s.wanted_mode());
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let loaded = store_job({
                let thread = thread.clone();
                move |store| store.transcript_messages(&thread)
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let messages = match loaded {
                Some(Ok(messages)) => messages,
                Some(Err(e)) => {
                    present_message(&obj, "Cannot Continue the Thread", &e.to_string());
                    return;
                }
                None => {
                    imp.show_toast("Cannot read the thread");
                    return;
                }
            };
            let handoff = build_handoff(&messages, &format!("thread {thread}"));
            if handoff.carried == 0 {
                imp.show_toast("Nothing to hand over yet: send a message first");
                return;
            }
            imp.create_chat_thread(NewThread {
                dir: Some(summary.cwd),
                model,
                effort,
                title: Some(continued_title(&title, driver)),
                handoff: Some((handoff, format!("“{title}”"))),
                mode,
                ..NewThread::new(driver)
            });
        });
    }

    /// This thread, in place, on `driver`/`model`/`effort` (the header picker's switch). A thread
    /// that is not built yet is opened first and switched as soon as its session exists.
    fn switch_thread_to(
        &self,
        thread: &str,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(_page) = self.open_thread(thread, true) else {
            return;
        };
        if self.slot_of(thread).and_then(|s| s.get()).is_some() {
            // Mid-turn, ask first: the switch stops the turn.
            let id = thread.to_owned();
            self.when_not_busy(thread, move |imp| {
                if let Some(session) = imp.slot_of(&id).and_then(|s| s.get()) {
                    session.switch(driver, model.clone(), effort.clone());
                }
            });
            return;
        }
        if let Some(chat) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|t| t.chat.as_mut())
            .find(|c| c.thread == thread)
        {
            chat.pending_switch = Some((driver, model, effort));
        }
    }

    // ---- rename ----

    fn prompt_rename(&self, thread: &str) {
        let current = self.thread_title_of(thread);
        let entry = gtk4::Entry::builder()
            .text(&current)
            .activates_default(true)
            .build();
        let dialog = adw::AlertDialog::new(Some("Rename Thread"), None);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        let (obj, thread) = (self.obj().downgrade(), thread.to_owned());
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let response = dialog
                .choose_future(Some(window.upcast_ref::<gtk4::Widget>()))
                .await;
            if response != "rename" {
                return;
            }
            let title = entry.text().trim().to_owned();
            if !title.is_empty() {
                window.imp().rename_thread_to(&thread, &title);
            }
        });
    }

    fn rename_thread_to(&self, thread: &str, title: &str) {
        let (id, stored) = (thread.to_owned(), title.to_owned());
        self.write_store("rename the thread", move |s| s.rename_thread(&id, &stored));
        if let Some(chat) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .filter_map(|t| t.chat.as_mut())
            .find(|c| c.thread == thread)
        {
            chat.title = title.to_owned();
        }
        if let Some(page) = self.page_of_thread(thread) {
            page.set_title(title);
            if self.selected_row_key() == Some(RowKey::Thread(thread.to_owned())) {
                if let Some(w) = self.window_title.borrow().as_ref() {
                    w.set_title(title);
                }
            }
        }
        self.refresh_sidebar();
    }

    // ---- archive / delete ----

    /// Hides (or brings back) a thread. Archiving closes its page; the thread stays in the store
    /// and behind the sidebar's "Show archived" toggle.
    fn set_thread_archived(&self, thread: &str, archived: bool) {
        // Archiving ends a running turn like closing does: ask first, and do nothing if declined.
        let key = RowKey::Thread(thread.to_owned());
        if archived && self.row_is_busy(&key) {
            let id = thread.to_owned();
            self.confirm_stop_then(thread, move |imp| imp.archive_now(&id));
            return;
        }
        if archived {
            self.close_row_now(&key);
        }
        self.store_archived(thread, archived);
    }

    fn archive_now(&self, thread: &str) {
        self.close_row_now(&RowKey::Thread(thread.to_owned()));
        self.store_archived(thread, true);
    }

    fn store_archived(&self, thread: &str, archived: bool) {
        let id = thread.to_owned();
        self.write_store("archive the thread", move |s| s.set_archived(&id, archived));
        self.show_toast(if archived {
            "Thread archived (Show archived brings it back into view)"
        } else {
            "Thread restored"
        });
    }

    /// Asks before deleting: this removes the thread's history from Agent Terminal.
    pub(super) fn confirm_delete(&self, thread: &str) {
        let title = self.thread_title_of(thread);
        let dialog = adw::AlertDialog::new(
            Some("Delete This Thread?"),
            Some(&format!(
                "“{title}” and its history in Agent Terminal will be deleted. The agent's own \
                 session files are not touched. This cannot be undone."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let (obj, thread) = (self.obj().downgrade(), thread.to_owned());
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let response = dialog
                .choose_future(Some(window.upcast_ref::<gtk4::Widget>()))
                .await;
            if response == "delete" {
                window.imp().delete_thread_now(&thread);
            }
        });
    }

    /// Closes the thread's page (ending its session) and removes it from the store.
    fn delete_thread_now(&self, thread: &str) {
        self.close_row_now(&RowKey::Thread(thread.to_owned()));
        let id = thread.to_owned();
        self.write_store("delete the thread", move |s| s.delete_thread(&id));
        self.show_toast("Thread deleted");
    }

    // ---- folder ----

    fn open_thread_folder(&self, thread: &str) {
        let Some(summary) = self.summary_of(thread) else {
            return;
        };
        let folder = gio::File::for_path(&summary.cwd);
        if !std::path::Path::new(&summary.cwd).is_dir() {
            self.show_toast("That folder no longer exists");
            return;
        }
        gtk4::FileLauncher::new(Some(&folder)).launch(
            Some(self.obj().upcast_ref::<gtk4::Window>()),
            None::<&gio::Cancellable>,
            |result| {
                if let Err(e) = result {
                    warn!("Cannot open the folder: {e}");
                }
            },
        );
    }
}

/// `<source title> (→ <Agent>)`: the title of a thread continued in another agent.
fn continued_title(source: &str, driver: Driver) -> String {
    format!("{source} (→ {})", driver_label(driver))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::window::sidebar_model::MenuAgent;

    #[test]
    fn a_continued_thread_is_titled_after_its_source_and_the_agent() {
        assert_eq!(
            continued_title("Fix the build", Driver::Codex),
            "Fix the build (→ Codex)"
        );
        assert_eq!(
            continued_title("New thread", Driver::Agy),
            "New thread (→ Antigravity)"
        );
    }

    /// GTK checks, run from the window test (GTK belongs to the one thread that initialised it).
    pub(in crate::window) fn gtk_checks() {
        let input = ThreadMenuInput {
            thread: "t".into(),
            archived: false,
            has_messages: false,
            agents: vec![MenuAgent {
                driver: Driver::Claude,
                models: Vec::new(),
            }],
        };
        let menu = to_gio_menu(&thread_menu(&input));
        // Three sections, each a submenu-free group of items or submenus.
        let model: &gio::MenuModel = menu.upcast_ref();
        assert_eq!(model.n_items(), 3);
        let section = model
            .item_link(0, "section")
            .expect("the first section holds Continue in and Switch");
        assert_eq!(
            gio_menu_labels_of(&section),
            ["Continue in a new thread", "Switch this thread to"]
        );
        let housekeeping = model.item_link(1, "section").expect("second section");
        assert_eq!(
            gio_menu_labels_of(&housekeeping),
            ["Rename…", "Archive", "Delete…"]
        );
        // With no messages every "Continue in" leaf points at the disabled action; Switch does not.
        let continue_in = section.item_link(0, "submenu").expect("submenu");
        let claude = continue_in.item_link(0, "submenu").expect("claude");
        let action = claude
            .item_attribute_value(0, "action", None)
            .and_then(|v| v.get::<String>());
        assert_eq!(action.as_deref(), Some(ACTION_OFF));
        let switch = section.item_link(1, "submenu").expect("submenu");
        let claude = switch.item_link(0, "submenu").expect("claude");
        let action = claude
            .item_attribute_value(0, "action", None)
            .and_then(|v| v.get::<String>());
        assert_eq!(action.as_deref(), Some(ACTION));
        // The target decodes back to the action.
        let target = claude
            .item_attribute_value(0, "target", None)
            .and_then(|v| v.get::<String>())
            .and_then(|s| ThreadAction::decode(&s));
        assert!(matches!(
            target,
            Some(ThreadAction::SwitchTo {
                driver: Driver::Claude,
                model: None,
                effort: None,
                ..
            })
        ));
    }

    /// Each menu choice, carried out on a real window and store, does what its label says and
    /// nothing more. Run from the window test with the shell up (GTK belongs to one thread).
    pub(in crate::window) fn actions_carry_out_each_choice(
        window: &crate::window::AgentTerminalWindow,
    ) {
        use crate::testutil::pump_until;
        use crate::window::imp::threads::app_store;
        let imp = window.imp();
        let ctx = glib::MainContext::default();
        let settle = |cond: &dyn Fn() -> bool| assert!(pump_until(&ctx, 5, cond), "timed out");
        let chat_of = |thread: &str| {
            imp.tabs
                .borrow()
                .iter()
                .filter_map(|t| t.chat.as_ref())
                .find(|c| c.thread == thread)
                .map(|c| (c.title.clone(), c.driver, c.pending_switch.clone()))
        };
        let dir = tempfile::tempdir().expect("tmp");
        let folder = dir.path().to_string_lossy().into_owned();
        let open_thread = || {
            imp.new_chat_thread(Some(Driver::Claude), Some(folder.clone()), None);
            let thread = imp
                .tabs
                .borrow()
                .last()
                .and_then(|t| t.chat.as_ref().map(|c| c.thread.clone()))
                .expect("a thread page");
            imp.reload_summaries();
            thread
        };
        let thread = open_thread();

        // A thread nobody has named is "New thread" everywhere; an unknown one too.
        assert_eq!(imp.thread_title_of(&thread), "New thread");
        assert_eq!(imp.thread_title_of("no-such-thread"), "New thread");

        // Rename: the store, the open page, its tab title and the sidebar row all follow.
        imp.rename_thread_to(&thread, "Fix the build");
        settle(&|| {
            app_store()
                .thread_summary(&thread)
                .ok()
                .flatten()
                .is_some_and(|s| s.title == "Fix the build")
        });
        assert_eq!(
            chat_of(&thread).map(|c| c.0).as_deref(),
            Some("Fix the build")
        );
        assert_eq!(
            imp.page_of_thread(&thread)
                .map(|p| p.title().to_string())
                .as_deref(),
            Some("Fix the build")
        );
        settle(&|| {
            imp.sidebar_rows()
                .iter()
                .any(|r| r.key == RowKey::Thread(thread.clone()) && r.title == "Fix the build")
        });
        // While the page is open its own title wins over a stored one changed elsewhere.
        app_store()
            .rename_thread(&thread, "Changed elsewhere")
            .expect("rename");
        assert_eq!(imp.thread_title_of(&thread), "Fix the build");

        // Copy id: exactly the id reaches the clipboard.
        imp.run_thread_action(ThreadAction::CopyId(thread.clone()));
        let copied = Rc::new(std::cell::RefCell::new(None));
        let sink = copied.clone();
        let clipboard = window.clipboard();
        ctx.spawn_local(async move {
            *sink.borrow_mut() = Some(clipboard.read_text_future().await.ok().flatten());
        });
        settle(&|| copied.borrow().is_some());
        assert_eq!(
            copied
                .borrow()
                .clone()
                .flatten()
                .map(|t| t.to_string())
                .as_deref(),
            Some(thread.as_str())
        );

        // Continue in, before anything was said: there is nothing to hand over, so no thread
        // is made.
        let pages = imp.tabs.borrow().len();
        imp.run_thread_action(ThreadAction::ContinueIn {
            thread: thread.clone(),
            driver: Driver::Codex,
            model: None,
            effort: None,
        });
        for _ in 0..50 {
            ctx.iteration(false);
        }
        assert_eq!(imp.tabs.borrow().len(), pages, "no handoff, no new thread");

        // With history it opens a NEW Codex thread in the same folder, titled after its
        // source, and leaves the source alone.
        app_store()
            .append_user_message(&thread, None, "make the build pass")
            .expect("message");
        imp.run_thread_action(ThreadAction::ContinueIn {
            thread: thread.clone(),
            driver: Driver::Codex,
            model: None,
            effort: None,
        });
        settle(&|| imp.tabs.borrow().len() == pages + 1);
        {
            let tabs = imp.tabs.borrow();
            let new = tabs.last().expect("the new page");
            let chat = new.chat.as_ref().expect("a thread page");
            assert_ne!(chat.thread, thread);
            assert_eq!(chat.driver, Driver::Codex);
            assert_eq!(new.dir, folder);
            assert_eq!(chat.title, "Fix the build (→ Codex)");
        }
        assert_eq!(
            chat_of(&thread).map(|c| c.1),
            Some(Driver::Claude),
            "the source is unchanged"
        );

        // The menu itself reflects the thread: with history, Continue in is offered enabled.
        // Folders start collapsed: open the thread's by clicking its header, as a user would.
        let sidebar = imp.sidebar.borrow().clone().expect("sidebar");
        let row_index = || {
            sidebar
                .keys
                .borrow()
                .iter()
                .position(|k| k.as_ref() == Some(&RowKey::Thread(thread.clone())))
        };
        imp.refresh_sidebar();
        if row_index().is_none() {
            let header = crate::testutil::descendants(sidebar.list.upcast_ref())
                .into_iter()
                .find(|w| w.is::<gtk4::ListBoxRow>() && w.widget_name() == folder.as_str())
                .expect("the folder's header");
            let toggle = header
                .first_child()
                .and_then(|line| line.first_child())
                .and_then(|w| w.downcast::<gtk4::Button>().ok())
                .expect("the folder toggle");
            toggle.emit_clicked();
        }
        let index = row_index().expect("the thread's row, once its folder is open");
        let row = sidebar.list.row_at_index(index as i32).expect("row");
        // The menu offers only agents detected as ready; here Claude is, the others are not.
        let availability = crate::availability::AgentAvailability::shared();
        let before = availability.all();
        availability.set(
            Driver::Claude,
            crate::availability::Availability::Ready("/nonexistent/claude".into()),
        );
        imp.open_thread_menu(thread.clone(), &row, Some((4.0, 4.0)));
        let list = imp
            .sidebar
            .borrow()
            .as_ref()
            .map(|s| s.list.clone())
            .expect("list");
        // The thread menu hangs off the list itself (rows' own popovers sit deeper).
        let popover = || {
            let mut child = list.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                if let Ok(menu) = widget.downcast::<gtk4::PopoverMenu>() {
                    return Some(menu);
                }
            }
            None
        };
        settle(&|| popover().is_some());
        // GIO does not bounds-check item indices: count before indexing.
        let link = |model: &gio::MenuModel, index: i32, kind: &str| {
            assert!(
                index < model.n_items(),
                "{kind} {index} of {}",
                model.n_items()
            );
            model.item_link(index, kind).expect(kind)
        };
        let model = popover().and_then(|p| p.menu_model()).expect("menu model");
        assert_eq!(model.n_items(), 3, "the thread menu's three sections");
        let section = link(&model, 0, "section");
        let continue_in = link(&section, 0, "submenu");
        assert_eq!(continue_in.n_items(), 1, "only the ready agent is offered");
        let first_agent = link(&continue_in, 0, "submenu");
        assert!(first_agent.n_items() > 0);
        for (driver, state) in before {
            availability.set(driver, state);
        }
        assert_eq!(
            first_agent
                .item_attribute_value(0, "action", None)
                .and_then(|v| v.get::<String>())
                .as_deref(),
            Some(ACTION),
            "a thread with history can be continued"
        );
        if let Some(p) = popover() {
            p.popdown();
        }

        // Archive: the page closes and the store marks it; Unarchive brings it back.
        imp.run_thread_action(ThreadAction::Archive(thread.clone()));
        assert!(chat_of(&thread).is_none(), "archiving closes the page");
        settle(&|| {
            app_store()
                .thread_summary(&thread)
                .ok()
                .flatten()
                .is_some_and(|s| s.archived)
        });
        imp.run_thread_action(ThreadAction::Unarchive(thread.clone()));
        settle(&|| {
            app_store()
                .thread_summary(&thread)
                .ok()
                .flatten()
                .is_some_and(|s| !s.archived)
        });
        // Closed, its title comes from the store.
        imp.reload_summaries();
        assert_eq!(imp.thread_title_of(&thread), "Changed elsewhere");

        // Switch, for a thread that is not open: it is opened, and the switch waits for its
        // session.
        imp.run_thread_action(ThreadAction::SwitchTo {
            thread: thread.clone(),
            driver: Driver::Codex,
            model: Some("gpt-5".into()),
            effort: None,
        });
        let switched = chat_of(&thread).expect("switching opens the thread");
        assert!(
            imp.slot_of(&thread).and_then(|s| s.get()).is_none(),
            "a reopened thread has no session until it is shown"
        );
        assert_eq!(
            switched.2,
            Some((Driver::Codex, Some("gpt-5".into()), None)),
            "the switch waits for the session"
        );

        // Delete: the page closes and the thread leaves the store, its history with it.
        imp.delete_thread_now(&thread);
        assert!(chat_of(&thread).is_none());
        settle(&|| app_store().thread_summary(&thread).ok().flatten().is_none());
        assert!(!app_store().has_messages(&thread).expect("read"));
        // Actions on a thread that is gone do nothing (and do not panic).
        imp.run_thread_action(ThreadAction::OpenFolder(thread.clone()));
        imp.run_thread_action(ThreadAction::ReviewEnvironment(thread.clone()));
        imp.run_thread_action(ThreadAction::ContinueIn {
            thread: thread.clone(),
            driver: Driver::Agy,
            model: None,
            effort: None,
        });
        assert!(chat_of(&thread).is_none());
    }

    fn gio_menu_labels_of(model: &gio::MenuModel) -> Vec<String> {
        (0..model.n_items())
            .filter_map(|i| {
                model
                    .item_attribute_value(i, "label", None)
                    .and_then(|v| v.get::<String>())
            })
            .collect()
    }
}
