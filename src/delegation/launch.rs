//! Lane D: per-generation launch bindings and Claude overlay files (§5.5, contract §5).
//!
//! The token reaches only the places the contract's exposure matrix lists: the Claude overlay's
//! server `env`, agy's process env, and Codex's `OpenSession.delegation`. Never argv.

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use agent_core::adapter::Driver;
use agent_core::delegation::launch::{
    claude_child_args, claude_parent_args, overlay_file_name, overlay_sweep_candidate,
};
use agent_core::delegation::{CapabilityToken, DelegationBinding};

use super::rundir;
use super::seam::{BindingRequest, LaunchBinding, PreparedBinding, Role, RuntimeDir};

/// 32 bytes from the kernel's CSPRNG (a std `File` read; no new crate).
fn random_bytes() -> Result<[u8; 32], String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(bytes)
}

fn utf8(path: &Path, what: &str) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("the {what} path is not valid UTF-8"))
}

/// Mints the token and, for Claude, writes the overlay (`O_EXCL|O_NOFOLLOW`, `0600`). Blocking:
/// run in `gio::spawn_blocking`.
pub(crate) fn prepare_binding(req: &BindingRequest) -> Result<PreparedBinding, String> {
    prepare_with(req, random_bytes()?)
}

/// [`prepare_binding`] with the random bytes given (tests pin them to find the token).
fn prepare_with(req: &BindingRequest, random: [u8; 32]) -> Result<PreparedBinding, String> {
    if !req.bridge_command.is_absolute() || !req.endpoint.is_absolute() {
        return Err("the bridge command and the endpoint must be absolute paths".to_owned());
    }
    let token = CapabilityToken::from_random(random);
    let binding = DelegationBinding {
        command: utf8(&req.bridge_command, "bridge command")?,
        endpoint: utf8(&req.endpoint, "endpoint")?,
        instance: req.instance.clone(),
        token: token.clone(),
    };
    let mut launch = LaunchBinding::default();
    let mut overlay = None;
    match req.driver {
        Driver::Claude => {
            let argv_for = |path: &str| -> Result<Vec<String>, String> {
                match req.role {
                    Role::Root => Ok(claude_parent_args(path)),
                    Role::Child => req
                        .task_mode
                        .map(|mode| claude_child_args(mode, path))
                        .ok_or_else(|| "a Claude child binding needs its task mode".to_owned()),
                }
            };
            let name = overlay_file_name(req.app_pid, &req.worker, req.generation);
            let path = req.rundir.path().join(name);
            let path_str = utf8(&path, "overlay")?;
            // Built before the file exists, so a refusal leaves nothing behind.
            let argv = argv_for(&path_str)?;
            let body = serde_json::to_vec(&binding.claude_overlay(req.app_pid))
                .map_err(|e| format!("cannot encode the overlay: {e}"))?;
            write_overlay(&req.rundir, &path, &body, rundir::effective_uid())?;
            launch.argv_tail = argv;
            overlay = Some(path);
        }
        Driver::Agy => {
            launch.env = binding
                .env(req.app_pid)
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect();
        }
        Driver::Codex => {
            launch.open = Some(binding);
        }
    }
    Ok(PreparedBinding {
        worker: req.worker.clone(),
        generation: req.generation,
        token,
        launch,
        overlay,
    })
}

/// Creates `path` (inside `dir`) with `O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC`, mode `0600`,
/// proves its owner and mode by `fstat` before writing, then writes and syncs. The directory is
/// re-proven first. Any failure after the create unlinks the file.
fn write_overlay(dir: &RuntimeDir, path: &Path, body: &[u8], uid: u32) -> Result<(), String> {
    rundir::check(dir.path(), uid)?;
    if path.parent() != Some(dir.path()) {
        return Err("the overlay is not directly in the runtime directory".to_owned());
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("cannot create the overlay {}: {e}", path.display()))?;
    let written = (|| -> Result<(), String> {
        let meta = file
            .metadata()
            .map_err(|e| format!("cannot fstat the overlay: {e}"))?;
        if meta.uid() != uid || !meta.is_file() || meta.mode() & 0o7777 != 0o600 {
            return Err("the overlay's owner or mode is not as created".to_owned());
        }
        file.write_all(body)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("cannot write the overlay: {e}"))
    })();
    if written.is_err() {
        drop(file);
        unlink_overlay(path);
    }
    written
}

/// Removes an overlay after its generation's observed exit. A file already gone is fine.
pub(crate) fn unlink_overlay(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, "could not remove a delegation overlay"),
    }
}

/// Startup: removes every `mcp-<pid>-*.json` whose app pid is not a live agent-terminal. Other
/// names are left alone.
pub(crate) fn sweep_overlays(dir: &RuntimeDir, live_app_pid: &dyn Fn(u32) -> bool) {
    let entries = match std::fs::read_dir(dir.path()) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, "could not list the runtime directory for stale overlays");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(overlay_sweep_candidate) else {
            continue;
        };
        if !live_app_pid(pid) {
            let path: PathBuf = entry.path();
            unlink_overlay(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::delegation::authority::TaskMode;
    use agent_core::delegation::env::ENV_CAP;
    use agent_core::delegation::{InstanceNonce, ProcessGeneration, WorkerId};
    use serde_json::Value;

    fn private_rundir(tmp: &Path) -> RuntimeDir {
        rundir::verified_in(tmp, rundir::effective_uid()).expect("rundir")
    }

    fn request(driver: Driver, role: Role, rundir: RuntimeDir) -> BindingRequest {
        BindingRequest {
            driver,
            role,
            worker: WorkerId::parse("w_abcdefgh").expect("id"),
            generation: ProcessGeneration(2),
            task_mode: Some(TaskMode::Ask),
            app_pid: 4242,
            bridge_command: PathBuf::from("/usr/bin/agent-terminal"),
            endpoint: PathBuf::from("/run/user/1000/agent-terminal/delegation-4242.sock"),
            instance: InstanceNonce::from_bytes([7; 16]),
            rundir,
        }
    }

    fn secret_of(p: &PreparedBinding) -> String {
        p.token.expose_secret().to_owned()
    }

    /// JSON paths at which `needle` appears.
    fn paths_of(v: &Value, needle: &str, at: &str, out: &mut Vec<String>) {
        match v {
            Value::String(s) if s.contains(needle) => out.push(at.to_owned()),
            Value::Object(m) => {
                for (k, c) in m {
                    assert!(!k.contains(needle));
                    paths_of(c, needle, &format!("{at}/{k}"), out);
                }
            }
            Value::Array(a) => {
                for (i, c) in a.iter().enumerate() {
                    paths_of(c, needle, &format!("{at}/{i}"), out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn claude_overlay_is_0600_and_holds_the_token_only_under_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = private_rundir(tmp.path());
        for role in [Role::Root, Role::Child] {
            let mut req = request(Driver::Claude, role, dir.clone());
            if role == Role::Child {
                req.generation = ProcessGeneration(3);
            }
            let p = prepare_binding(&req).expect("prepared");
            let path = p.overlay.clone().expect("overlay");
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some(overlay_file_name(4242, &req.worker, req.generation).as_str())
            );
            let meta = std::fs::symlink_metadata(&path).expect("lstat");
            assert!(meta.is_file());
            assert_eq!(meta.mode() & 0o7777, 0o600);
            let json: Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
            let mut at = Vec::new();
            paths_of(&json, &secret_of(&p), "", &mut at);
            assert_eq!(
                at,
                vec![format!("/mcpServers/agent_terminal/env/{ENV_CAP}")]
            );
            // Never in argv, and no env or open binding for Claude.
            assert!(!p.launch.argv_tail.iter().any(|a| a.contains("atdc_")));
            assert!(p.launch.argv_tail.contains(&"--mcp-config".to_owned()));
            assert_eq!(
                p.launch
                    .argv_tail
                    .contains(&"--strict-mcp-config".to_owned()),
                role == Role::Child
            );
            assert!(p.launch.env.is_empty());
            assert!(p.launch.open.is_none());
            unlink_overlay(&path);
            assert!(!path.exists());
            unlink_overlay(&path); // idempotent
        }
    }

    #[test]
    fn agy_gets_the_env_and_codex_the_open_binding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = private_rundir(tmp.path());
        let agy = prepare_binding(&request(Driver::Agy, Role::Root, dir.clone())).expect("agy");
        assert!(agy.overlay.is_none());
        assert!(agy.launch.argv_tail.is_empty());
        assert!(agy.launch.open.is_none());
        let cap = agy
            .launch
            .env
            .iter()
            .find(|(k, _)| k == ENV_CAP)
            .map(|(_, v)| v.clone());
        assert_eq!(cap, Some(secret_of(&agy)));
        assert!(!format!("{:?}", agy.launch).contains(&secret_of(&agy)));

        let codex = prepare_binding(&request(Driver::Codex, Role::Child, dir)).expect("codex");
        assert!(codex.overlay.is_none());
        assert!(codex.launch.env.is_empty());
        assert!(codex.launch.argv_tail.is_empty());
        assert_eq!(
            codex.launch.open.as_ref().map(|b| &b.token),
            Some(&codex.token)
        );
        // No overlay was written for either.
        assert_eq!(
            std::fs::read_dir(tmp.path().join("agent-terminal"))
                .map(|d| d.count())
                .ok(),
            Some(0)
        );
    }

    #[test]
    fn each_binding_mints_a_fresh_token() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = private_rundir(tmp.path());
        let a = prepare_binding(&request(Driver::Agy, Role::Root, dir.clone())).expect("a");
        let b = prepare_binding(&request(Driver::Agy, Role::Root, dir)).expect("b");
        assert_ne!(secret_of(&a), secret_of(&b));
    }

    #[test]
    fn overlay_refuses_an_existing_file_or_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = private_rundir(tmp.path());
        let req = request(Driver::Claude, Role::Root, dir.clone());
        let path = dir
            .path()
            .join(overlay_file_name(4242, &req.worker, req.generation));
        std::fs::write(&path, b"old").expect("write");
        assert!(prepare_with(&req, [1; 32]).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), b"old");
        std::fs::remove_file(&path).expect("rm");

        let target = tmp.path().join("elsewhere.json");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        assert!(prepare_with(&req, [1; 32]).is_err());
        assert!(!target.exists(), "nothing written through the link");
    }

    #[test]
    fn overlay_0600_excl_refuses_symlinked_rundir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = private_rundir(tmp.path());
        let link = tmp.path().join("linked");
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink");
        let req = request(Driver::Claude, Role::Root, RuntimeDir::from_verified(link));
        let err = prepare_with(&req, [1; 32]).expect_err("refused");
        assert!(err.contains("symlink"), "{err}");
        assert_eq!(
            std::fs::read_dir(real.path()).map(|d| d.count()).ok(),
            Some(0)
        );
    }

    #[test]
    fn a_claude_child_without_a_task_mode_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = private_rundir(tmp.path());
        let mut req = request(Driver::Claude, Role::Child, dir.clone());
        req.task_mode = None;
        assert!(prepare_with(&req, [1; 32]).is_err());
        assert_eq!(
            std::fs::read_dir(dir.path()).map(|d| d.count()).ok(),
            Some(0)
        );
    }

    #[test]
    fn sweep_removes_only_dead_pids_overlays() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = private_rundir(tmp.path());
        let names = [
            "mcp-100-w_abcdefgh-1.json",
            "mcp-200-w_abcdefgh-4.json",
            "mcp.json",
            "delegation-100.sock",
        ];
        for n in names {
            std::fs::write(dir.path().join(n), b"{}").expect("write");
        }
        sweep_overlays(&dir, &|pid| pid == 200);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("list")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "delegation-100.sock".to_owned(),
                "mcp-200-w_abcdefgh-4.json".to_owned(),
                "mcp.json".to_owned()
            ]
        );
    }
}
