//! Settings → General → Uninstall: what an uninstall removes, the "Remove Configuration and
//! Data" choice (on by default; off when another installed copy shares the data), and the
//! confirmation. The work itself waits until every window
//! has closed (`crate::uninstall::after_quit`), so no agent or store write outlives the data.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4::glib;

use super::AgentTerminalWindow;

impl AgentTerminalWindow {
    /// The Uninstall group for Settings → General.
    pub(super) fn uninstall_group(&self) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().title("Uninstall").build();
        let button = gtk4::Button::builder()
            .label("Uninstall…")
            .valign(gtk4::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        let row = adw::ActionRow::builder()
            .title("Uninstall Agent Terminal")
            .subtitle(
                "Remove the program, and choose whether to remove your configuration and data too",
            )
            .build();
        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));
        let obj = self.obj();
        button.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |button| obj.imp().review_uninstall(button)
        ));
        group.add(&row);
        group
    }

    /// Looks at what an uninstall would remove (off the main thread: it sources profile env
    /// files and asks git and the package manager), then asks.
    fn review_uninstall(&self, button: &gtk4::Button) {
        button.set_sensitive(false);
        let weak = self.obj().downgrade();
        let weak_button = button.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let found = gtk4::gio::spawn_blocking(|| {
                let inputs = crate::uninstall::inputs();
                let plan = agent_kit::uninstall::plan(&inputs);
                (
                    crate::uninstall::summary(&plan, &inputs.home),
                    plan.shared_with.is_some(),
                )
            })
            .await;
            let Some(button) = weak_button.upgrade() else {
                return;
            };
            button.set_sensitive(true);
            let (Some(obj), Ok((summary, shared))) = (weak.upgrade(), found) else {
                return;
            };
            obj.imp()
                .confirm_uninstall(summary.trim(), !shared, &button);
        });
    }

    /// `remove_data`: the switch's starting state, off when another installed copy that stays
    /// uses the same data.
    fn confirm_uninstall(&self, summary: &str, remove_data: bool, anchor: &gtk4::Button) {
        let dialog = adw::AlertDialog::new(
            Some("Uninstall Agent Terminal?"),
            Some(
                "Every window closes and running threads stop. The removal finishes after the \
                 app has quit; a package asks for your password.",
            ),
        );
        let data = adw::SwitchRow::builder()
            .title("Remove Configuration and Data")
            .subtitle(
                "Threads, settings, approval rules, caches, checkpoints in your repositories, \
                 and the entries Agent Terminal added to Antigravity and Codex",
            )
            .active(remove_data)
            .build();
        let rows = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        rows.append(&data);
        let details = gtk4::Label::builder()
            .label(summary)
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["dim-label", "caption"])
            .build();
        let scroller = gtk4::ScrolledWindow::builder()
            .child(&details)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(220)
            .build();
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        content.append(&rows);
        content.append(&scroller);
        dialog.set_extra_child(Some(&content));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("uninstall", "Uninstall");
        dialog.set_response_appearance("uninstall", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let obj = self.obj();
        dialog.connect_response(
            Some("uninstall"),
            glib::clone!(
                #[weak]
                obj,
                #[weak]
                data,
                move |_, _| {
                    crate::uninstall::request(data.is_active());
                    tracing::info!(
                        remove_data = data.is_active(),
                        "uninstall: confirmed in Settings, closing every window"
                    );
                    // The dialog above already said running threads stop: no second question
                    // that could leave the request waiting for an unrelated quit.
                    obj.imp().for_each_window(|window| {
                        window.close_confirmed.set(true);
                        window.obj().close();
                    });
                }
            ),
        );
        dialog.present(Some(anchor));
    }
}
