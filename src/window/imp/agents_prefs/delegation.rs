//! Managed registration is previewed on a worker and applied only by its explicit dialog.

use super::*;
use agent_kit::delegation_config::{self, Action};
use std::path::PathBuf;

impl AgentTerminalWindow {
    pub(super) fn delegation_group(
        &self,
        dialog: &adw::PreferencesDialog,
    ) -> adw::PreferencesGroup {
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
            .subtitle("Not checked. Claude and Codex use temporary launch bindings; Antigravity needs a managed registration.")
            .subtitle_lines(0).build();
        group.add(&row);
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
        group
    }

    fn preview_delegation_registration(
        &self,
        action: Option<Action>,
        dialog: glib::WeakRef<adw::PreferencesDialog>,
        row: glib::WeakRef<adw::ActionRow>,
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
                Ok((home, state, command, launch)) => gtk4::gio::spawn_blocking(move || {
                    let preview=delegation_config::preview(&home, &state, &command, action.unwrap_or(Action::Configure));
                    let diagnostic=action.is_none().then(||delegation_config::bridge_diagnostic(&command,&launch.env,&launch.unset));
                    (preview,diagnostic)
                }).await.map_err(|_| "Registration worker failed".to_owned()),
                Err(error) => Err(error),
            };
            if action.is_none() {
                let text=match result {
                    Ok((preview,diagnostic))=>{
                        let registration=match preview {Ok(plan) if !plan.changed()=>"Registration and exact allow rules verified.".to_owned(),Ok(_)=>"Registration or exact allow rules need repair.".to_owned(),Err(error)=>format!("Registration could not be verified: {error}")};
                        let shim=match diagnostic {Some(Ok(()))=>"Headless bridge handshake verified in this profile's environment; unbound state expected.".to_owned(),Some(Err(error))=>format!("Headless bridge diagnostic failed: {error}"),None=>"Headless bridge was not tested.".to_owned()};
                        format!("Profile: {profile_name}\n{registration}\n{shim}\nLive authenticated thread binding is not tested by this check; open an enabled thread to establish it.")
                    }
                    Err(error)=>format!("Could not verify this profile's environment: {error}"),
                };
                if let Some(row) = row.upgrade() { row.set_subtitle(&text); }
                return;
            }
            let plan=match result {Ok((Ok(plan),_))=>plan,Ok((Err(error),_))|Err(error)=>{if let Some(row)=row.upgrade(){row.set_subtitle(&format!("Could not verify: {error}"));}return;}};
            let (Some(obj), Some(parent)) = (weak.upgrade(), dialog.upgrade()) else { return; };
            let summary=format!("Profile: {profile_name}\n{}",plan.summary());
            let confirm = adw::AlertDialog::new(Some("Review Delegation Registration"), Some(&summary));
            confirm.add_responses(&[("cancel", "Cancel"), ("apply", "Apply")]);
            confirm.set_default_response(Some("cancel"));
            confirm.set_close_response("cancel");
            confirm.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
            let plan = RefCell::new(Some(plan));
            let obj = obj.downgrade();
            confirm.connect_response(None, move |_, response| {
                if response != "apply" { return; }
                let Some(plan) = plan.borrow_mut().take() else { return; };
                let weak = obj.clone();
                let row = row.clone();
                glib::MainContext::default().spawn_local(async move {
                    let result = gtk4::gio::spawn_blocking(move || delegation_config::apply(plan)).await
                        .unwrap_or_else(|_| Err("Registration worker failed".into()));
                    let text = match result {
                        Ok(()) => "Registration saved. Reopen enabled threads to use the new configuration.".into(),
                        Err(error) => error,
                    };
                    if let Some(row) = row.upgrade() { row.set_subtitle(&text); }
                    if let Some(obj) = weak.upgrade() { obj.imp().show_toast(&text); }
                });
            });
            confirm.present(Some(&parent));
        });
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
