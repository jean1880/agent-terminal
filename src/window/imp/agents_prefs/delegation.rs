//! Managed registration is previewed on a worker and applied only by its explicit dialog.

use super::*;
use agent_kit::delegation_config::{self, Action};
use std::path::PathBuf;

impl AgentTerminalWindow {
    /// The delegation group, and its Antigravity registration row (which the agent groups'
    /// "Available for delegation" switches report to).
    pub(super) fn delegation_group(
        &self,
        dialog: &adw::PreferencesDialog,
    ) -> (adw::PreferencesGroup, glib::WeakRef<adw::ActionRow>) {
        let group = adw::PreferencesGroup::builder()
            .title("Cross-agent delegation")
            .description(agent_core::delegation::tools::DELEGATION_SCOPE_TEXT)
            .build();
        let runtime = adw::ActionRow::builder().title("Application binding")
            .subtitle(if crate::delegation::supervisor().is_some() {
                "Supervisor available. Each enabled thread receives its own binding when opened."
            } else { "Could not verify the supervisor. Delegation is unavailable until startup completes." })
            .build();
        group.add(&runtime);
        let row = adw::ActionRow::builder().title("Antigravity registration")
            .subtitle("Checking… Claude and Codex use temporary launch bindings; Antigravity needs a managed registration, set up when it is made available for delegation.")
            .subtitle_lines(0).build();
        group.add(&row);
        self.check_delegation_registration(row.downgrade());
        let registration_row = row.downgrade();
        for (title, action) in [
            ("Configure / Repair", Some(Action::Configure)),
            ("Remove", Some(Action::Remove)),
            ("Test", None),
        ] {
            let button = gtk4::Button::with_label(title);
            let obj = self.obj();
            let weak_dialog = dialog.downgrade();
            let weak_row = row.downgrade();
            button.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    obj.imp().preview_delegation_registration(
                        action,
                        weak_dialog.clone(),
                        weak_row.clone(),
                    );
                }
            ));
            let action_row = adw::ActionRow::builder().title(title).build();
            action_row.add_suffix(&button);
            action_row.set_activatable_widget(Some(&button));
            group.add(&action_row);
        }
        let undo = gtk4::Button::with_label("Undo last change");
        let obj = self.obj();
        let weak_row = row.downgrade();
        undo.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| {
                let profile = obj.imp().agent_profile_now(Driver::Agy);
                let clear = obj.imp().config.borrow().clear_env.clone();
                let obj = obj.downgrade();
                let weak_row = weak_row.clone();
                glib::MainContext::default().spawn_local(async move {
                    let result = match registration_context(profile, clear).await {
                        Ok((home, state, _, _)) => gtk4::gio::spawn_blocking(move || {
                            delegation_config::undo(&home, &state)
                        })
                        .await
                        .unwrap_or_else(|_| Err("Registration worker failed".into())),
                        Err(error) => Err(error),
                    };
                    let text = match result {
                        Ok(()) => "Registration restored. Reopen enabled threads to use it.".into(),
                        Err(error) => error,
                    };
                    if let Some(row) = weak_row.upgrade() {
                        row.set_subtitle(&text);
                    }
                    if let Some(obj) = obj.upgrade() {
                        obj.imp().show_toast(&text);
                    }
                });
            }
        ));
        let undo_row = adw::ActionRow::builder()
            .title("Restore previous registration")
            .subtitle("Refuses to overwrite settings edited after the last change.")
            .build();
        undo_row.add_suffix(&undo);
        undo_row.set_activatable_widget(Some(&undo));
        group.add(&undo_row);
        (group, registration_row)
    }

    fn preview_delegation_registration(
        &self,
        action: Option<Action>,
        dialog: glib::WeakRef<adw::PreferencesDialog>,
        row: glib::WeakRef<adw::ActionRow>,
    ) {
        let Some(action) = action else {
            self.test_delegation_registration(row);
            return;
        };
        self.review_delegation_registration(action, dialog, row, |_| {});
    }

    /// The Test button: the strict registration check plus the headless bridge handshake.
    fn test_delegation_registration(&self, row: glib::WeakRef<adw::ActionRow>) {
        let profile = self.agent_profile_now(Driver::Agy);
        let profile_name = profile.name.clone();
        let clear = self.config.borrow().clear_env.clone();
        if let Some(row) = row.upgrade() {
            row.set_subtitle("Checking the selected profile's registration…");
        }
        glib::MainContext::default().spawn_local(async move {
            let result = match registration_context(profile, clear).await {
                Ok((home, state, command, launch)) => gtk4::gio::spawn_blocking(move || {
                    let preview=delegation_config::preview(&home, &state, &command, Action::Configure);
                    let diagnostic=delegation_config::bridge_diagnostic(&command,&launch.env,&launch.unset);
                    (preview,diagnostic)
                }).await.map_err(|_| "Registration worker failed".to_owned()),
                Err(error) => Err(error),
            };
            let text=match result {
                Ok((preview,diagnostic))=>{
                    let registration=registration_status(&preview);
                    let shim=match diagnostic {Ok(())=>"Headless bridge handshake verified in this profile's environment; unbound state expected.".to_owned(),Err(error)=>format!("Headless bridge diagnostic failed: {error}")};
                    format!("Profile: {profile_name}\n{registration}\n{shim}\nLive authenticated thread binding is not tested by this check; open an enabled thread to establish it.")
                }
                Err(error)=>format!("Could not verify this profile's environment: {error}"),
            };
            if let Some(row) = row.upgrade() { row.set_subtitle(&text); }
        });
    }

    /// Fills the registration row with the strict check's verdict, without the bridge
    /// handshake (that starts a process; the Test button runs it).
    pub(super) fn check_delegation_registration(&self, row: glib::WeakRef<adw::ActionRow>) {
        let profile = self.agent_profile_now(Driver::Agy);
        let clear = self.config.borrow().clear_env.clone();
        glib::MainContext::default().spawn_local(async move {
            let text = match registration_context(profile, clear).await {
                Ok((home, state, command, _)) => gtk4::gio::spawn_blocking(move || {
                    registration_status(&delegation_config::preview(
                        &home,
                        &state,
                        &command,
                        Action::Configure,
                    ))
                })
                .await
                .unwrap_or_else(|_| "Registration worker failed".to_owned()),
                Err(error) => error,
            };
            if let Some(row) = row.upgrade() {
                row.set_subtitle(&text);
            }
        });
    }

    /// Previews `action` on the Antigravity profile's registration, listing any permission
    /// repairs, and applies it only from its review dialog. `done` gets the outcome once:
    /// `Ready` when nothing needed changing or the change was applied.
    pub(super) fn review_delegation_registration(
        &self,
        action: Action,
        dialog: glib::WeakRef<adw::PreferencesDialog>,
        row: glib::WeakRef<adw::ActionRow>,
        done: impl FnOnce(Review) + 'static,
    ) {
        let profile = self.agent_profile_now(Driver::Agy);
        let profile_name = profile.name.clone();
        let clear = self.config.borrow().clear_env.clone();
        let weak = self.obj().downgrade();
        if let Some(row) = row.upgrade() {
            row.set_subtitle("Checking the selected profile's registration…");
        }
        glib::MainContext::default().spawn_local(async move {
            let result = match registration_context(profile, clear).await {
                Ok((home, state, command, _)) => gtk4::gio::spawn_blocking(move || {
                    delegation_config::preview_repairing(&home, &state, &command, action)
                })
                .await
                .unwrap_or_else(|_| Err("Registration worker failed".to_owned())),
                Err(error) => Err(error),
            };
            let finish = |row: &glib::WeakRef<adw::ActionRow>, text: &str| {
                if let Some(row) = row.upgrade() {
                    row.set_subtitle(text);
                }
            };
            let plan = match result {
                Ok(plan) if !plan.changed() => {
                    let text = match action {
                        Action::Configure => registration_status(&Ok(plan)),
                        Action::Remove => "No managed registration to remove.".to_owned(),
                    };
                    finish(&row, &text);
                    done(Review::Ready);
                    return;
                }
                Ok(plan) => plan,
                Err(error) => {
                    let text = format!("Could not verify: {error}");
                    finish(&row, &text);
                    done(Review::Failed(text));
                    return;
                }
            };
            let (Some(obj), Some(parent)) = (weak.upgrade(), dialog.upgrade()) else {
                done(Review::Cancelled);
                return;
            };
            let summary = format!("Profile: {profile_name}\n{}", plan.summary());
            let confirm =
                adw::AlertDialog::new(Some("Review Delegation Registration"), Some(&summary));
            confirm.add_responses(&[("cancel", "Cancel"), ("apply", "Apply")]);
            confirm.set_default_response(Some("cancel"));
            confirm.set_close_response("cancel");
            confirm.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
            if confirm.choose_future(Some(&parent)).await != "apply" {
                finish(&row, "Registration left unchanged.");
                done(Review::Cancelled);
                return;
            }
            let result = gtk4::gio::spawn_blocking(move || delegation_config::apply(plan))
                .await
                .unwrap_or_else(|_| Err("Registration worker failed".into()));
            let (text, review) = match result {
                Ok(()) => (
                    "Registration saved. Reopen enabled threads to use the new configuration."
                        .to_owned(),
                    Review::Ready,
                ),
                Err(error) => (error.clone(), Review::Failed(error)),
            };
            finish(&row, &text);
            obj.imp().show_toast(&text);
            done(review);
        });
    }
}

/// How a registration review ended.
pub(super) enum Review {
    /// Nothing needed changing, or the reviewed change was applied.
    Ready,
    /// The user declined the change (or the dialog went away).
    Cancelled,
    /// The registration could not be checked or the change failed; the message says why.
    Failed(String),
}

fn registration_status(preview: &Result<delegation_config::Preview, String>) -> String {
    match preview {
        Ok(plan) if !plan.changed() => "Registration and exact allow rules verified.".to_owned(),
        Ok(_) => {
            "Registration or exact allow rules need setting up: Configure / Repair.".to_owned()
        }
        Err(error) => format!("Registration could not be verified: {error}"),
    }
}

async fn registration_context(
    profile: Profile,
    clear: Vec<String>,
) -> Result<(PathBuf, PathBuf, PathBuf, crate::agent_proc::AgentEnv), String> {
    let file = profile.env_file.clone();
    let env =
        gtk4::gio::spawn_blocking(move || match file.filter(|path| !path.trim().is_empty()) {
            Some(path) => crate::utils::load_env_file_checked(&path),
            None => Ok(Vec::new()),
        })
        .await
        .map_err(|_| "Could not resolve this profile's environment".to_owned())??;
    let launch = crate::agent_proc::AgentEnv {
        env,
        unset: super::super::threads::unset_list(&clear),
    };
    if agent_core::delegation::agy_guard::argv_redirect(&profile.argv()).is_some()
        || agent_core::delegation::agy_guard::env_redirect(&crate::agent_home::child_env(&launch))
            .is_some()
    {
        return Err("The selected profile redirects Antigravity configuration; managed registration is unavailable.".into());
    }
    let home = crate::agent_home::effective_home(&launch)
        .ok_or("The selected profile has no verified home")?;
    let state = home.join(".config/agent-terminal/delegation");
    let command = std::env::current_exe()
        .map_err(|_| "Cannot locate the installed Agent Terminal executable")?;
    Ok((home, state, command, launch))
}
