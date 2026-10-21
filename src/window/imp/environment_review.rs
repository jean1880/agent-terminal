//! Confirmation and target capture for the bundled environment review.

use agent_core::adapter::Driver;

use super::threads::NewThread;
use super::*;
use crate::window::sidebar_model::driver_label;

impl AgentTerminalWindow {
    pub(super) fn review_current_environment(&self) {
        let dir = self.current_dir().unwrap_or_else(|| {
            let home = env::var("HOME").unwrap_or_else(|_| "/".to_owned());
            let requested = self.config.borrow().starting_directory.clone();
            if requested.trim().is_empty() {
                home
            } else {
                crate::utils::expand_tilde_with(&requested, &home)
            }
        });
        let driver = self.current_slot().map(|slot| slot.driver());
        self.confirm_environment_review(dir, driver);
    }

    pub(super) fn confirm_environment_review(&self, dir: String, preferred: Option<Driver>) {
        let Some(driver) = preferred
            .filter(|d| self.agent_usable(*d))
            .or_else(|| self.default_agent())
        else {
            present_message(&self.obj(), "Cannot Review Environment",
                "Wait for agent detection to finish, or enable an installed agent in Settings → Agents.");
            return;
        };
        let dialog = adw::AlertDialog::new(
            Some("Review Multi-Agent Environment?"),
            Some(&format!(
                "Open a new {} session to review this folder:\n\n{}\n\n\
                 The agent will inspect project instructions and relevant agent configuration, \
                 then produce a scored report with skill and MCP recommendations. Review \
                 instructions prohibit changes. Inspected content is sent to the selected \
                 agent under its usual permissions and may use its quota.",
                driver_label(driver),
                dir
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("review", "Start Review")]);
        dialog.set_response_appearance("review", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let response = dialog
                .choose_future(Some(window.upcast_ref::<gtk4::Widget>()))
                .await;
            if response != "review" {
                return;
            }
            let checked = gtk4::gio::spawn_blocking({
                let dir = dir.clone();
                move || crate::environment_review::exact_directory(&dir)
            })
            .await;
            match checked {
                Ok(Ok(_)) => {}
                Ok(Err(message)) => {
                    present_message(&window, "Cannot Review Environment", message);
                    return;
                }
                Err(_) => {
                    present_message(
                        &window,
                        "Cannot Review Environment",
                        "Could not check the target folder.",
                    );
                    return;
                }
            }
            if !window.imp().agent_usable(driver) {
                present_message(
                    &window,
                    "Cannot Review Environment",
                    "The selected agent is no longer available.",
                );
                return;
            }
            // Explicit folder: profile defaults never redirect this review, and a folder gone
            // since is refused rather than replaced (`resolve_new_thread_dir`).
            window.imp().create_chat_thread(NewThread {
                prompt: Some(crate::environment_review::prompt(&dir)),
                dir: Some(dir),
                title: Some("Multi-agent environment review".to_owned()),
                ..NewThread::new(driver)
            });
        });
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::window::imp) fn confirmation_preserves_target(
        window: &super::super::super::AgentTerminalWindow,
    ) {
        let ctx = glib::MainContext::default();
        let imp = window.imp();
        // Libadwaita 1.5 (the minimum, and CI's) ignores an answer given before the dialog's
        // sheet has finished opening, which failed this check on CI only (reproduced in an
        // Ubuntu 24.04 container). `trigger` settles after mapping; animations are off so
        // opening and closing take no animated time.
        let settings = gtk4::Settings::default();
        let animated = settings.as_ref().map(|s| s.is_gtk_enable_animations());
        if let Some(s) = &settings {
            s.set_gtk_enable_animations(false);
        }
        let availability = crate::availability::AgentAvailability::shared();
        assert!(crate::testutil::pump_until(&ctx, 10, || !availability.any_detecting()));
        availability.set(
            Driver::Claude,
            crate::availability::Availability::Ready("/nonexistent/claude".into()),
        );
        let target = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let dir = target.path().to_string_lossy().into_owned();
        // Titled: left blank, it would be discarded once the next thread is shown.
        let source = imp
            .create_chat_thread(NewThread {
                dir: Some(dir.clone()),
                title: Some("source".into()),
                ..NewThread::new(Driver::Claude)
            })
            .unwrap();
        let thread = imp
            .tabs
            .borrow()
            .iter()
            .find(|t| t.page == source)
            .and_then(|t| t.chat.as_ref())
            .unwrap()
            .thread
            .clone();
        imp.create_chat_thread(NewThread {
            dir: Some(other.path().to_string_lossy().into_owned()),
            ..NewThread::new(Driver::Claude)
        });
        let before = imp.tabs.borrow().len();
        // Libadwaita 1.5 opens its embedded sheet on the second mapped frame. Merely
        // appearing in visible_dialog() does not prove it has opened; closing an unmapped
        // sheet can otherwise leave it registered forever in this construction test.
        window.present();
        assert!(crate::testutil::pump_until(&ctx, 5, || window.is_mapped()));
        let trigger = || {
            let target =
                crate::window::sidebar_model::ThreadAction::ReviewEnvironment(thread.clone())
                    .encode();
            window
                .lookup_action("thread-menu")
                .unwrap()
                .activate(Some(&target.to_variant()));
            assert!(crate::testutil::pump_until(&ctx, 5, || window
                .visible_dialog()
                .is_some()));
            let dialog = window
                .visible_dialog()
                .unwrap()
                .downcast::<adw::AlertDialog>()
                .unwrap();
            assert!(crate::testutil::pump_until(&ctx, 5, || dialog.is_mapped()));
            // Let the sheet finish opening (a user never answers within its first frames).
            crate::testutil::settle(&ctx);
            dialog
        };
        let cancel = trigger();
        assert!(cancel.body().contains(&dir));
        // Press the dialog's buttons as the user does. On libadwaita 1.5 a first programmatic
        // `close()` returns true yet leaves an AlertDialog open (verified in an Ubuntu 24.04
        // container), which is what made this check fail on CI only.
        let press = |dialog: &adw::AlertDialog, label: &str| {
            let button = crate::testutil::descendants(dialog.upcast_ref())
                .into_iter()
                .filter_map(|w| w.downcast::<gtk4::Button>().ok())
                .find(|b| b.label().as_deref() == Some(label))
                .unwrap_or_else(|| panic!("the confirmation has a {label:?} button"));
            button.emit_clicked();
        };
        // A press that lands while a busy runner is still opening the sheet is ignored, as a
        // user's would be; press again until the dialog goes (a user would click again too).
        assert!(crate::testutil::pump_until(&ctx, 10, || {
            if window.visible_dialog().is_none() {
                return true;
            }
            press(&cancel, "Cancel");
            crate::testutil::pump_until(&ctx, 1, || window.visible_dialog().is_none())
        }));
        assert_eq!(imp.tabs.borrow().len(), before);
        let review = trigger();
        assert!(crate::testutil::pump_until(&ctx, 10, || {
            if imp.tabs.borrow().len() == before + 1 {
                return true;
            }
            if window.visible_dialog().is_some() {
                press(&review, "Start Review");
            }
            crate::testutil::pump_until(&ctx, 1, || imp.tabs.borrow().len() == before + 1)
        }));
        let tabs = imp.tabs.borrow();
        let added = tabs.last().unwrap();
        assert_eq!(added.dir, dir, "clicked target wins over the selected page");
        assert_eq!(
            added.chat.as_ref().unwrap().title,
            "Multi-agent environment review"
        );
        if let (Some(s), Some(on)) = (&settings, animated) {
            s.set_gtk_enable_animations(on);
        }
    }
}
