//! Uninstalling: `agent-terminal --uninstall`, and Settings → General → Uninstall.
//!
//! Both ask whether to remove the user's configuration and data too (yes by default), then
//! remove the program the way it was installed. What is removed, and what never is, is
//! `agent_kit::uninstall`'s; this module gathers its inputs (the environment and every agy and
//! Codex profile's home) and talks to the user.
//!
//! The data goes only while the app is stopped: the CLI refuses while an instance runs, and
//! Settings records the request ([`request`]), closes every window (agents stop, the store
//! writer flushes) and [`after_quit`] does the work once `app.run()` has returned.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agent_core::adapter::Driver;
use agent_kit::fsutil::effective_uid;
use agent_kit::uninstall::{self, Elevation, Install, Plan, Report};
use gtk4::{gio, glib};
use tracing::{info, warn};

/// What Settings asked for: `Some(remove_data)` once the user confirmed.
static REQUEST: Mutex<Option<bool>> = Mutex::new(None);

const USAGE: &str = "\
Usage: agent-terminal --uninstall [--yes | --keep-data]

Removes Agent Terminal. Asks whether to remove your configuration and data too (threads,
settings, approval rules, caches, checkpoints in your repositories, and the entries it added to
Antigravity and Codex); the answer defaults to yes.

  -y, --yes    Remove the configuration and data without asking
  --keep-data  Keep the configuration and data, remove only the program

Run it as yourself, not with sudo: it asks for your password when the package needs it.
";

/// Why this process must not uninstall for the user, if it must not: run through sudo or pkexec
/// (HOME is then root's, or the user's folders are not root's to judge), or with a home folder
/// that belongs to someone else.
fn refusal(euid: u32, elevated_by_tool: bool, home_owner: Option<u32>) -> Option<&'static str> {
    if euid == 0 && elevated_by_tool {
        return Some(
            "Run it as yourself, without sudo or pkexec: it asks for your password when the \
             package needs it, and your configuration is in your home folder, not root's.",
        );
    }
    match home_owner {
        Some(owner) if owner != euid => Some(
            "Your home folder belongs to another user. Run it as the user whose Agent Terminal \
             it is.",
        ),
        _ => None,
    }
}

/// The CLI. `args` are the arguments after `--uninstall`.
pub fn cli(args: &[String]) -> glib::ExitCode {
    let mut answer = None;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" if answer != Some(false) => answer = Some(true),
            "--keep-data" if answer != Some(true) => answer = Some(false),
            "--help" | "-h" => {
                print!("{USAGE}");
                return glib::ExitCode::SUCCESS;
            }
            _ => {
                eprint!("{USAGE}");
                return glib::ExitCode::from(2);
            }
        }
    }
    let elevated = ["SUDO_USER", "SUDO_UID", "PKEXEC_UID"]
        .iter()
        .any(|v| std::env::var_os(v).is_some());
    let home_owner = std::env::var_os("HOME")
        .and_then(|h| std::fs::metadata(h).ok())
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m));
    if let Some(reason) = refusal(effective_uid(), elevated, home_owner) {
        eprintln!("{reason}");
        return glib::ExitCode::from(2);
    }
    if app_running() {
        eprintln!(
            "Agent Terminal is running. Quit it first, or use Settings → General → Uninstall."
        );
        return glib::ExitCode::FAILURE;
    }
    println!("Looking at what Agent Terminal put on this computer…");
    let inputs = inputs();
    let plan = uninstall::plan(&inputs);
    print!("{}", summary(&plan, &inputs.home));

    let remove_data = match answer {
        Some(answer) => answer,
        None if std::io::stdin().is_terminal() => {
            // Another copy that stays shares the data: Enter keeps it.
            let shared = plan.shared_with.is_some();
            let question = if shared {
                "Also remove your configuration and data? [y/N] "
            } else {
                "Also remove your configuration and data? [Y/n] "
            };
            match ask(question, !shared) {
                Some(answer) => answer,
                None => {
                    println!("\nNothing was removed.");
                    return glib::ExitCode::FAILURE;
                }
            }
        }
        None => {
            eprintln!(
                "There is no terminal to ask on: pass --yes to remove your configuration and \
                 data too, or --keep-data to keep them."
            );
            return glib::ExitCode::from(2);
        }
    };

    let mut failed = false;
    if remove_data {
        let report = uninstall::apply(&plan, effective_uid(), &inputs.home);
        failed |= print_report("Removed", &report);
    } else {
        println!("Your configuration and data are kept.");
    }
    failed |= !remove_program(&plan.install, &inputs.home, Elevation::Sudo);
    if failed {
        glib::ExitCode::FAILURE
    } else {
        if !matches!(plan.install, Install::Unmanaged { .. }) {
            println!("Agent Terminal is uninstalled.");
        }
        glib::ExitCode::SUCCESS
    }
}

/// Reads a yes/no answer; Enter alone is `default`. `None` on end of input.
fn ask(question: &str, default: bool) -> Option<bool> {
    let stdin = std::io::stdin();
    loop {
        print!("{question}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).ok()? == 0 {
            return None;
        }
        if let Some(answer) = parse_answer(&line, default) {
            return Some(answer);
        }
    }
}

/// `default` for Enter alone; `Some(true)` for y or yes, `Some(false)` for n or no (any case);
/// `None` otherwise.
fn parse_answer(line: &str, default: bool) -> Option<bool> {
    match line.trim().to_lowercase().as_str() {
        "" => Some(default),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// What the plan removes, for the terminal and for the Settings dialog.
pub fn summary(plan: &Plan, home: &Path) -> String {
    let mut out = format!("\nProgram: {}\n", plan.install.describe(home));
    if plan.items.is_empty() {
        out.push_str("Configuration and data: none found.\n");
    } else {
        out.push_str(&format!(
            "Configuration and data ({}):\n",
            uninstall::human_size(plan.bytes())
        ));
        for item in &plan.items {
            out.push_str(&format!("  • {}\n", item.describe(home)));
        }
    }
    for note in &plan.notes {
        out.push_str(&format!("Note: {note}\n"));
    }
    out.push('\n');
    out
}

/// Prints what was done; `true` when anything failed.
fn print_report(heading: &str, report: &Report) -> bool {
    for done in &report.done {
        println!("{heading}: {done}");
    }
    for failed in &report.failed {
        eprintln!("Could not remove {failed}");
    }
    !report.failed.is_empty()
}

/// Removes the program; `false` when that failed. A package goes through its manager, with
/// `elevation` unless already root.
fn remove_program(install: &Install, home: &Path, elevation: Elevation) -> bool {
    match install {
        Install::Local { files } => !print_report("Removed", &uninstall::remove_local(files, home)),
        Install::Package { .. } => {
            let elevation = if effective_uid() == 0 {
                Elevation::None
            } else {
                elevation
            };
            let Some(argv) = install.removal_command(elevation) else {
                return false;
            };
            println!("Running: {}", argv.join(" "));
            info!(command = %argv.join(" "), "uninstall: removing the package");
            match std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .status()
            {
                Ok(status) if status.success() => true,
                Ok(status) => {
                    warn!(%status, "uninstall: the package manager did not remove the package");
                    eprintln!(
                        "The package was not removed ({status}). Run the command above yourself."
                    );
                    false
                }
                Err(e) => {
                    warn!("uninstall: could not run {}: {e}", argv[0]);
                    eprintln!(
                        "Could not run {}: {e}. Run the command above yourself.",
                        argv[0]
                    );
                    false
                }
            }
        }
        Install::Unmanaged { .. } => {
            println!("{}", install.describe(home));
            true
        }
    }
}

/// Whether an instance of the app runs for this user: one owns its D-Bus name on this session
/// bus, or (seen from another login, with another bus or none) a process of this user's is the
/// app.
pub fn app_running() -> bool {
    if uninstall::other_instances(effective_uid()) > 0 {
        return true;
    }
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return false;
    };
    bus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&glib::Variant::tuple_from_iter([glib::Variant::from(
            crate::APP_ID,
        )])),
        None,
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|reply| reply.get::<(bool,)>())
    .is_some_and(|(owned,)| owned)
}

/// The plan's inputs from this process's environment and the saved profiles. Blocking: a
/// profile's env file is sourced by a shell.
pub fn inputs() -> uninstall::Inputs {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let home = var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let config = crate::config::TerminalConfig::load();
    let process = crate::codex_home::process_env();

    let mut agy_homes = vec![home.clone()];
    let mut codex_homes = vec![];
    let own_codex = crate::codex_home::resolve(&process).unwrap_or_else(|_| home.join(".codex"));
    codex_homes.push(own_codex);
    for profile in &config.profiles {
        let driver = crate::config::profile_driver(profile);
        if !matches!(driver, Some(Driver::Agy | Driver::Codex)) {
            continue;
        }
        let env_file = match profile.env_file.as_deref().filter(|p| !p.trim().is_empty()) {
            Some(path) => crate::utils::load_env_file_checked(path).unwrap_or_else(|e| {
                warn!("uninstall: profile {}'s env file: {e}", profile.name);
                vec![]
            }),
            None => vec![],
        };
        let child = crate::codex_home::child_env(&crate::codex_home::Inputs {
            inherited: process.clone(),
            shell_path: None,
            env_file,
            clear: config.clear_env.clone(),
        });
        match driver {
            Some(Driver::Agy) => {
                if let Some(h) = crate::agent_home::home_in_child(&child) {
                    if !agy_homes.contains(&h) {
                        agy_homes.push(h);
                    }
                }
            }
            Some(Driver::Codex) => {
                if let Ok(h) = crate::codex_home::resolve(&child) {
                    if !codex_homes.contains(&h) {
                        codex_homes.push(h);
                    }
                }
            }
            _ => {}
        }
    }
    uninstall::Inputs {
        xdg_config: var("XDG_CONFIG_HOME"),
        xdg_state: var("XDG_STATE_HOME"),
        xdg_cache: var("XDG_CACHE_HOME"),
        xdg_data: var("XDG_DATA_HOME"),
        xdg_runtime: var("XDG_RUNTIME_DIR"),
        agy_homes,
        codex_homes,
        exe: std::env::current_exe().ok(),
        uid: effective_uid(),
        home,
    }
}

/// Settings confirmed an uninstall: done by [`after_quit`] once every window has closed.
pub fn request(remove_data: bool) {
    if let Ok(mut pending) = REQUEST.lock() {
        *pending = Some(remove_data);
    }
}

/// Whether an uninstall is waiting for the app to quit. While it is, no window opens (a launch
/// that reaches this instance is refused) and the last window closing quits the app, so the
/// request can never wait for some later, unrelated quit.
pub fn pending() -> bool {
    REQUEST.lock().is_ok_and(|p| p.is_some())
}

/// Runs a confirmed Settings uninstall, after the application loop has ended: the agents are
/// stopped and the store is closed by then. There is no window left, so everything is logged,
/// and the package removal asks for the password through polkit.
pub fn after_quit() {
    let Some(remove_data) = REQUEST.lock().ok().and_then(|mut p| p.take()) else {
        return;
    };
    info!(remove_data, "uninstall: starting");
    let inputs = inputs();
    let plan = uninstall::plan(&inputs);
    if remove_data {
        let report = uninstall::apply(&plan, effective_uid(), &inputs.home);
        for done in &report.done {
            info!("uninstall: removed {done}");
        }
        for failed in &report.failed {
            warn!("uninstall: could not remove {failed}");
        }
    }
    if remove_program(&plan.install, &inputs.home, Elevation::Pkexec) {
        info!("uninstall: done");
    } else {
        warn!("uninstall: the program was not removed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_pkexec_and_a_foreign_home_are_refused() {
        assert!(refusal(1000, false, Some(1000)).is_none());
        assert!(refusal(1000, false, None).is_none());
        // Root itself (a container, a root login) uninstalls its own copy.
        assert!(refusal(0, false, Some(0)).is_none());
        assert!(refusal(0, true, Some(0)).is_some());
        assert!(refusal(0, false, Some(1000)).is_some());
        assert!(refusal(1000, false, Some(0)).is_some());
    }

    #[test]
    fn enter_means_yes_and_only_clear_answers_count() {
        for default in [true, false] {
            assert_eq!(parse_answer("\n", default), Some(default));
            for yes in ["y\n", "Yes\n", "  YES  "] {
                assert_eq!(parse_answer(yes, default), Some(true), "{yes:?}");
            }
            for no in ["n\n", "No\n", "NO"] {
                assert_eq!(parse_answer(no, default), Some(false), "{no:?}");
            }
            for unclear in ["maybe\n", "nope\n", "yy"] {
                assert_eq!(parse_answer(unclear, default), None, "{unclear:?}");
            }
        }
    }

    #[test]
    fn the_summary_lists_the_program_items_and_notes() {
        let home = Path::new("/h");
        let plan = Plan {
            install: Install::Package {
                manager: uninstall::Manager::Apt,
                name: "agent-terminal".into(),
            },
            items: vec![uninstall::Item::Folder {
                path: PathBuf::from("/h/.config/agent-terminal"),
                bytes: 24_000,
            }],
            notes: vec!["hooks.json still has the entry".into()],
            shared_with: None,
        };
        let text = summary(&plan, home);
        assert!(text.contains("The agent-terminal package (apt)"));
        assert!(text.contains("~/.config/agent-terminal (24 KB)"));
        assert!(text.contains("Note: hooks.json"));
        let empty = Plan {
            items: vec![],
            notes: vec![],
            ..plan
        };
        assert!(summary(&empty, home).contains("none found"));
    }
}
