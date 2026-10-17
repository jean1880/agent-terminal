//! Managed agy registration. Blocking I/O: call on a worker, never from a GTK signal.
//!
//! Preview and apply are separate. The apply compares every preimage again under a private
//! lock; undo compares every postimage. Unrelated JSON values are preserved, and conflicts,
//! symlinks, oversized files and malformed input are refused. The private recovery journal
//! holds original bytes and must never be logged or put in a thread store.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use agent_core::delegation::agy_guard::ALLOW_RULES;
use agent_core::delegation::{BRIDGE_ARG, SERVER_NAME};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_FILE: u64 = 1024 * 1024;
const JOURNAL: &str = "delegation-registration.json";

#[derive(Clone, Copy)]
pub enum Action {
    Configure,
    Remove,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct Edit {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
}

/// Contains private preimages: deliberately has no Debug or public serialisation API.
pub struct Preview {
    home: PathBuf,
    state: PathBuf,
    command: PathBuf,
    action: Action,
    edits: Vec<Edit>,
    added_rules: Vec<String>,
    previous_added_rules: Vec<String>,
    journal_before: Option<String>,
    repairs: Vec<Repair>,
    summary: String,
}

impl Preview {
    /// Safe to show: names and intended edits only, no existing file contents.
    pub fn summary(&self) -> &str {
        &self.summary
    }
    /// Whether applying it would change anything: a file edit or a permission repair.
    pub fn changed(&self) -> bool {
        !self.repairs.is_empty() || self.edits.iter().any(|e| e.before != e.after)
    }
    /// The permission repairs applying it would make first (only from [`preview_repairing`]).
    pub fn repairs(&self) -> &[Repair] {
        &self.repairs
    }
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: u8,
    edits: Vec<Edit>,
    added_rules: Vec<String>,
    #[serde(default)]
    previous_added_rules: Vec<String>,
    #[serde(default = "yes")]
    undo_available: bool,
    #[serde(default = "yes")]
    complete: bool,
}

fn yes() -> bool {
    true
}

fn validate_rules(rules: &[String]) -> Result<(), String> {
    let unique: std::collections::BTreeSet<_> = rules.iter().collect();
    if rules.len() > ALLOW_RULES.len()
        || unique.len() != rules.len()
        || rules
            .iter()
            .any(|rule| !ALLOW_RULES.contains(&rule.as_str()))
    {
        return Err(fail("recovery journal contains unexpected managed rules"));
    }
    Ok(())
}

fn validate_journal(home: &Path, journal: &mut Journal) -> Result<(), String> {
    let expected = [
        home.join(".gemini/config/mcp_config.json"),
        home.join(".gemini/antigravity-cli/settings.json"),
    ];
    if !matches!(journal.version, 1 | 2)
        || journal.edits.len() != 2
        || journal
            .edits
            .iter()
            .zip(&expected)
            .any(|(edit, path)| edit.path != *path)
    {
        return Err(fail("recovery journal has unexpected version or targets"));
    }
    validate_rules(&journal.added_rules)?;
    validate_rules(&journal.previous_added_rules)?;
    for edit in &journal.edits {
        for image in [&edit.before, &edit.after].into_iter().flatten() {
            if image.len() as u64 > MAX_FILE {
                return Err(fail("recovery journal image exceeds its size limit"));
            }
            object(Some(image))?;
        }
    }
    if journal.version == 1 {
        // The unreleased v1 format did not retain ownership before Remove. Only rules both
        // recorded as ours and actually present in its preimage can be restored as ours.
        let before = object(journal.edits[1].before.as_deref())?;
        journal.previous_added_rules = journal
            .added_rules
            .iter()
            .filter(|rule| {
                before
                    .pointer("/permissions/allow")
                    .and_then(Value::as_array)
                    .is_some_and(|rules| {
                        rules
                            .iter()
                            .any(|value| value.as_str() == Some(rule.as_str()))
                    })
            })
            .cloned()
            .collect();
    }
    Ok(())
}

fn fail(message: impl Into<String>) -> String {
    message.into()
}

/// A group- or world-writable directory or file on a registration path that the user owns, and
/// that is neither a symlink nor hard-linked: the one unsafe condition a review may repair.
#[derive(Clone, PartialEq, Eq)]
pub struct Repair {
    path: PathBuf,
    mode: u32,
}

impl Repair {
    /// The path and its permission change, e.g. `/home/u/.gemini: 775 → 755`.
    pub fn describe(&self) -> String {
        format!(
            "{}: {:o} → {:o}",
            self.path.display(),
            self.mode & 0o7777,
            self.mode & 0o7777 & !0o022
        )
    }
}

fn safe_path(home: &Path, path: &Path) -> Result<(), String> {
    check_path(home, path, None)
}

/// Every component from `home` down to `path` must be owned, not a symlink, not hard-linked and
/// not group- or world-writable. With `repairs`, a writable but otherwise safe component is
/// recorded there instead of refused. Messages name the path, never its contents.
fn check_path(
    home: &Path,
    path: &Path,
    mut repairs: Option<&mut Vec<Repair>>,
) -> Result<(), String> {
    if !path.starts_with(home)
        || path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(fail("registration path is outside the selected home"));
    }
    // SAFETY: geteuid has no preconditions.
    let owner = unsafe { libc::geteuid() };
    let mut current = home.to_path_buf();
    let parts = path
        .strip_prefix(home)
        .map_err(|_| fail("invalid registration path"))?
        .components();
    for part in std::iter::once(None).chain(parts.map(Some)) {
        if let Some(part) = part {
            current.push(part);
        }
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && part.is_some() => continue,
            Err(_) => return Err(format!("cannot verify {}", current.display())),
        };
        let named = |why: &str| format!("{} {why}", current.display());
        if meta.file_type().is_symlink() {
            return Err(named("is a symbolic link"));
        }
        if meta.uid() != owner {
            return Err(named("is not owned by you"));
        }
        if part.is_none() && !meta.is_dir() {
            return Err(named("is not a directory"));
        }
        if !meta.is_dir() && !meta.is_file() {
            return Err(named("is not a regular file or directory"));
        }
        if meta.is_file() && meta.nlink() != 1 {
            return Err(named("has other hard links"));
        }
        if meta.mode() & 0o022 != 0 {
            let repair = Repair {
                path: current.clone(),
                mode: meta.mode(),
            };
            match repairs.as_deref_mut() {
                Some(list) => {
                    if !list.contains(&repair) {
                        list.push(repair);
                    }
                }
                None => {
                    return Err(named(
                        "is writable by its group or others; remove that permission (chmod go-w)",
                    ))
                }
            }
        }
    }
    Ok(())
}

/// Removes group and world write from each reviewed path, parents first, through a descriptor
/// opened without following links. A path that is no longer the reviewed, owned file or
/// directory with the reviewed mode is refused, not changed.
fn repair(repairs: &[Repair]) -> Result<(), String> {
    let mut ordered: Vec<_> = repairs.iter().collect();
    ordered.sort_by_key(|r| r.path.components().count());
    // SAFETY: geteuid has no preconditions.
    let owner = unsafe { libc::geteuid() };
    for r in ordered {
        let changed = || {
            format!(
                "{} changed since the preview; preview it again",
                r.path.display()
            )
        };
        let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC;
        if r.mode & libc::S_IFMT == libc::S_IFDIR {
            flags |= libc::O_DIRECTORY;
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(&r.path)
            .map_err(|_| changed())?;
        let meta = file.metadata().map_err(|_| changed())?;
        if meta.uid() != owner || meta.mode() != r.mode || (meta.is_file() && meta.nlink() != 1) {
            return Err(changed());
        }
        file.set_permissions(fs::Permissions::from_mode(r.mode & 0o7777 & !0o022))
            .map_err(|_| format!("cannot change the permissions of {}", r.path.display()))?;
    }
    Ok(())
}

fn read(home: &Path, path: &Path, limit: u64) -> Result<Option<String>, String> {
    read_checked(home, path, limit, None)
}

fn read_checked(
    home: &Path,
    path: &Path,
    limit: u64,
    repairs: Option<&mut Vec<Repair>>,
) -> Result<Option<String>, String> {
    check_path(home, path, repairs)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(fail("cannot read the registration file")),
    };
    if !file
        .metadata()
        .map_err(|_| fail("cannot verify the registration file"))?
        .is_file()
    {
        return Err(fail("registration target is not a regular file"));
    }
    let mut text = String::new();
    file.take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|_| fail("registration file is not readable UTF-8"))?;
    if text.len() as u64 > limit {
        return Err(fail("registration file exceeds its size limit"));
    }
    Ok(Some(text))
}

fn object(text: Option<&str>) -> Result<Value, String> {
    let value: Value = match text {
        Some(text) => serde_json::from_str(text)
            .map_err(|_| fail("registration contains invalid JSON; repair it before continuing"))?,
        None => json!({}),
    };
    if !value.is_object() {
        return Err(fail("registration must be a JSON object"));
    }
    Ok(value)
}

fn encode(value: &Value) -> Result<String, String> {
    let mut text =
        serde_json::to_string_pretty(value).map_err(|_| fail("cannot encode registration"))?;
    text.push('\n');
    if text.len() as u64 > MAX_FILE {
        return Err(fail("updated registration exceeds its size limit"));
    }
    Ok(text)
}

fn subobject<'a>(
    value: &'a mut Value,
    key: &str,
) -> Result<&'a mut serde_json::Map<String, Value>, String> {
    value
        .as_object_mut()
        .ok_or_else(|| fail("registration must be an object"))?
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| fail(format!("{key} must be an object")))
}

/// Plans only the managed server and exact allow rules; no directories or files are written.
/// Any unsafe path refuses the plan.
pub fn preview(
    home: &Path,
    state: &Path,
    command: &Path,
    action: Action,
) -> Result<Preview, String> {
    plan(home, state, command, action, false)
}

/// As [`preview`], but group- or world-writable paths that are otherwise safe are listed as
/// repairs (see [`Preview::repairs`]) instead of refused. [`apply`] makes them first, then
/// plans again strictly and refuses if that plan differs from this one.
pub fn preview_repairing(
    home: &Path,
    state: &Path,
    command: &Path,
    action: Action,
) -> Result<Preview, String> {
    plan(home, state, command, action, true)
}

fn plan(
    home: &Path,
    state: &Path,
    command: &Path,
    action: Action,
    repairing: bool,
) -> Result<Preview, String> {
    if !command.is_absolute() {
        return Err(fail(
            "the installed Agent Terminal command must be absolute",
        ));
    }
    let home = fs::canonicalize(home).map_err(|_| fail("cannot resolve the selected home"))?;
    if !state.starts_with(&home) {
        return Err(fail("recovery directory must be in the selected home"));
    }
    let mcp_path = home.join(".gemini/config/mcp_config.json");
    let settings_path = home.join(".gemini/antigravity-cli/settings.json");
    let mut repairs = Vec::new();
    let mcp_before = read_checked(
        &home,
        &mcp_path,
        MAX_FILE,
        repairing.then_some(&mut repairs),
    )?;
    let settings_before = read_checked(
        &home,
        &settings_path,
        MAX_FILE,
        repairing.then_some(&mut repairs),
    )?;
    let mut mcp = object(mcp_before.as_deref())?;
    let mut settings = object(settings_before.as_deref())?;
    let entry = json!({"command":command, "args":[BRIDGE_ARG]});
    let servers = subobject(&mut mcp, "mcpServers")?;
    if servers
        .get(SERVER_NAME)
        .is_some_and(|existing| existing != &entry)
    {
        return Err(fail(
            "agent_terminal already names a different MCP registration; it was left unchanged",
        ));
    }
    let journal_before = read_checked(
        &home,
        &state.join(JOURNAL),
        16 * MAX_FILE,
        repairing.then_some(&mut repairs),
    )?;
    let previous: Option<Journal> = journal_before
        .as_ref()
        .map(|s| {
            serde_json::from_str(s)
                .map_err(|_| fail("recovery journal is invalid; registration was left unchanged"))
        })
        .transpose()?;
    let previous=previous.map(|mut journal| {validate_journal(&home,&mut journal)?;if !journal.complete{return Err(fail("an interrupted registration change needs Undo last change before another registration change"));}Ok(journal)}).transpose()?;
    let permissions = subobject(&mut settings, "permissions")?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| fail("permissions.allow must be an array"))?;
    if allow.iter().any(|v| !v.is_string()) {
        return Err(fail("permissions.allow contains a non-string rule"));
    }
    let mut added_rules = previous
        .as_ref()
        .map(|j| j.added_rules.clone())
        .unwrap_or_default();
    let previous_added_rules = added_rules.clone();
    let summary = match action {
        Action::Configure => {
            servers.insert(SERVER_NAME.into(), entry);
            for rule in ALLOW_RULES {
                if !allow.iter().any(|v| v.as_str() == Some(rule)) {
                    allow.push(json!(rule));
                    if !added_rules.iter().any(|r| r == rule) {
                        added_rules.push(rule.into());
                    }
                }
            }
            format!("Configure agent_terminal to run {} {}.\nSelected home: {}\nFiles: {} and {}\nAdd the eight exact delegation allow rules.\nPreserve unrelated MCP servers and settings. Save a private recovery journal before applying.", command.display(), BRIDGE_ARG,home.display(),mcp_path.display(),settings_path.display())
        }
        Action::Remove => {
            servers.remove(SERVER_NAME);
            allow.retain(|v| !added_rules.iter().any(|r| v.as_str() == Some(r.as_str())));
            added_rules.clear();
            format!("Selected home: {}\nFiles: {} and {}\nRemove only the managed agent_terminal registration and rules added by Agent Terminal. Preserve other settings and pre-existing allow rules.",home.display(),mcp_path.display(),settings_path.display())
        }
    };
    // A file whose value does not change keeps its exact bytes: formatting alone is no change.
    let after = |before: &Option<String>, value: &Value| -> Result<Option<String>, String> {
        match before {
            None if matches!(action, Action::Remove) => Ok(None),
            Some(text) if object(Some(text))? == *value => Ok(Some(text.clone())),
            _ => encode(value).map(Some),
        }
    };
    let mcp_after = after(&mcp_before, &mcp)?;
    let settings_after = after(&settings_before, &settings)?;
    let summary = if repairs.is_empty() {
        summary
    } else {
        let list: Vec<_> = repairs
            .iter()
            .map(|r| format!("• {}", r.describe()))
            .collect();
        format!(
            "First remove group and world write permission (unsafe for a registration):\n{}\n{summary}",
            list.join("\n")
        )
    };
    Ok(Preview {
        home,
        state: state.to_path_buf(),
        command: command.to_path_buf(),
        action,
        repairs,
        edits: vec![
            Edit {
                path: mcp_path,
                before: mcp_before,
                after: mcp_after,
            },
            Edit {
                path: settings_path,
                before: settings_before,
                after: settings_after,
            },
        ],
        added_rules,
        previous_added_rules,
        journal_before,
        summary,
    })
}

fn private_dirs(home: &Path, dir: &Path) -> Result<(), String> {
    safe_path(home, dir)?;
    if let Some(parent) = dir.parent().filter(|p| p.starts_with(home)) {
        if !parent.exists() {
            private_dirs(home, parent)?;
        }
    }
    if !dir.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(|_| fail("cannot create the registration directory"))?;
    }
    safe_path(home, dir)
}

fn lock(home: &Path, state: &Path) -> Result<File, String> {
    private_dirs(home, state)?;
    if fs::metadata(state)
        .map_err(|_| fail("cannot verify recovery directory"))?
        .mode()
        & 0o777
        != 0o700
    {
        return Err(fail("recovery directory must have mode 0700"));
    }
    let path = state.join("registration.lock");
    safe_path(home, &path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| fail("cannot open the registration lock"))?;
    file.lock().map_err(|_| fail("cannot lock registration"))?;
    Ok(file)
}

fn put(home: &Path, path: &Path, bytes: Option<&str>) -> Result<(), String> {
    safe_path(home, path)?;
    match bytes {
        Some(bytes) => {
            private_dirs(home, path.parent().ok_or_else(|| fail("missing parent"))?)?;
            crate::fsutil::write_private_atomic(path, bytes.as_bytes())
                .map_err(|_| fail("cannot write registration atomically"))
        }
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(fail("cannot remove managed registration file")),
        },
    }
}

fn replace(
    home: &Path,
    path: &Path,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<(), String> {
    let unchanged = || -> std::io::Result<()> {
        match read(home, path, MAX_FILE) {
            Ok(current) if current.as_deref() == before => Ok(()),
            _ => Err(std::io::Error::other(
                "configuration changed before publication",
            )),
        }
    };
    safe_path(home, path)?;
    if let Some(after) = after {
        private_dirs(home, path.parent().ok_or_else(|| fail("missing parent"))?)?;
        // Recheck after writing/syncing the candidate, immediately before its rename.
        crate::fsutil::write_private_atomic_checked(path, after.as_bytes(), |_| unchanged())
            .map_err(|_| {
                fail("configuration changed or could not be written; recovery journal retained")
            })
    } else {
        unchanged().map_err(|_| fail("configuration changed before removal"))?;
        put(home, path, None)
    }
}

/// Applies a reviewed preview. A crash or partial failure leaves the private journal for undo.
/// Its permission repairs come first; the plan is then made again strictly and must match the
/// reviewed one exactly.
pub fn apply(plan: Preview) -> Result<(), String> {
    let plan = if plan.repairs.is_empty() {
        plan
    } else {
        repair(&plan.repairs)?;
        let strict = preview(&plan.home, &plan.state, &plan.command, plan.action)?;
        if strict.edits != plan.edits
            || strict.journal_before != plan.journal_before
            || strict.added_rules != plan.added_rules
        {
            return Err(fail(
                "permissions repaired, but the configuration changed since the preview; preview it again",
            ));
        }
        strict
    };
    let _lock = lock(&plan.home, &plan.state)?;
    if read(&plan.home, &plan.state.join(JOURNAL), 16 * MAX_FILE)? != plan.journal_before {
        return Err(fail(
            "registration recovery state changed since the preview; preview it again",
        ));
    }
    for edit in &plan.edits {
        if read(&plan.home, &edit.path, MAX_FILE)? != edit.before {
            return Err(fail(
                "configuration changed since the preview; preview it again",
            ));
        }
    }
    if !plan.changed() {
        return Ok(());
    }
    let mut journal = Journal {
        version: 2,
        edits: plan.edits,
        added_rules: plan.added_rules,
        previous_added_rules: plan.previous_added_rules,
        undo_available: true,
        complete: false,
    };
    let bytes = serde_json::to_vec(&journal).map_err(|_| fail("cannot encode recovery journal"))?;
    crate::fsutil::write_private_atomic(&plan.state.join(JOURNAL), &bytes)
        .map_err(|_| fail("cannot save recovery journal; no registration was changed"))?;
    for edit in &journal.edits {
        if let Err(error) = replace(
            &plan.home,
            &edit.path,
            edit.before.as_deref(),
            edit.after.as_deref(),
        ) {
            // Restore only files still equal to our own postimage. Never overwrite a later edit.
            for prior in &journal.edits {
                if read(&plan.home, &prior.path, MAX_FILE).ok().flatten() == prior.after {
                    let _ = replace(
                        &plan.home,
                        &prior.path,
                        prior.after.as_deref(),
                        prior.before.as_deref(),
                    );
                }
            }
            return Err(format!("{error}; recovery journal retained"));
        }
    }
    journal.complete = true;
    let bytes = serde_json::to_vec(&journal)
        .map_err(|_| fail("cannot encode completed recovery journal"))?;
    crate::fsutil::write_private_atomic(&plan.state.join(JOURNAL),&bytes).map_err(|_|fail("configuration saved but recovery completion could not be recorded; recovery journal retained"))?;
    Ok(())
}

/// Restores only unchanged postimages. Refuses later user edits or a journal for another home.
pub fn undo(home: &Path, state: &Path) -> Result<(), String> {
    let home = fs::canonicalize(home).map_err(|_| fail("cannot resolve the selected home"))?;
    let _lock = lock(&home, state)?;
    let path = state.join(JOURNAL);
    let text = read(&home, &path, 16 * MAX_FILE)?
        .ok_or_else(|| fail("no registration recovery journal"))?;
    let mut journal: Journal =
        serde_json::from_str(&text).map_err(|_| fail("invalid recovery journal"))?;
    validate_journal(&home, &mut journal)?;
    if !journal.undo_available {
        return Err(fail("no undoable registration change remains"));
    }
    for edit in &journal.edits {
        let now = read(&home, &edit.path, MAX_FILE)?;
        if now != edit.after && now != edit.before {
            return Err(fail(
                "configuration changed after registration; undo refused to overwrite it",
            ));
        }
    }
    for edit in &journal.edits {
        if read(&home, &edit.path, MAX_FILE)? != edit.before {
            replace(
                &home,
                &edit.path,
                edit.after.as_deref(),
                edit.before.as_deref(),
            )?;
        }
    }
    if journal.previous_added_rules.is_empty() {
        fs::remove_file(path)
            .map_err(|_| fail("restored configuration but could not remove the recovery journal"))
    } else {
        // Keep the ownership ledger after undoing Remove/Repair; repeated Undo must not erase
        // it. It is not another undoable mutation and never contains a recursive journal.
        for edit in &mut journal.edits {
            edit.after = edit.before.clone();
        }
        journal.added_rules = journal.previous_added_rules;
        journal.previous_added_rules = Vec::new();
        journal.undo_available = false;
        journal.complete = true;
        journal.version = 2;
        let bytes = serde_json::to_vec(&journal)
            .map_err(|_| fail("cannot encode restored registration ownership"))?;
        crate::fsutil::write_private_atomic(&path, &bytes)
            .map_err(|_| fail("restored configuration but could not preserve managed ownership"))
    }
}

/// Non-quota diagnostic of the actual installed headless shim in the selected environment.
/// Bindings are stripped after profile overrides: this proves the MCP lifecycle and expected
/// unbound behaviour, not a provider's authentication, route admission or live capability.
/// Blocking; all I/O is bounded and the process group is stopped on every outcome.
pub fn bridge_diagnostic(
    command: &Path,
    env: &[(String, String)],
    unset: &[String],
) -> Result<(), String> {
    bridge_diagnostic_with_timeout(command, env, unset, std::time::Duration::from_secs(5))
}

fn bridge_diagnostic_with_timeout(
    command: &Path,
    env: &[(String, String)],
    unset: &[String],
    timeout: std::time::Duration,
) -> Result<(), String> {
    use std::io::{BufRead, Write};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            if let Ok(pid) = i32::try_from(self.0.id()) {
                // SAFETY: the child created its own process group; negative pid targets only it.
                unsafe { libc::kill(-pid, libc::SIGTERM) };
                let until = Instant::now() + Duration::from_millis(100);
                loop {
                    match self.0.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() < until => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        _ => break,
                    }
                }
                // SAFETY: same child process group as above, never a name-based kill.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    if !command.is_absolute() {
        return Err(fail("bridge diagnostic needs an absolute executable"));
    }
    let mut cmd = Command::new(command);
    cmd.arg(BRIDGE_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    for key in unset {
        cmd.env_remove(key);
    }
    cmd.envs(env.iter().map(|(key, value)| (key, value)));
    for key in agent_core::delegation::env::STRIP_ON_SPAWN {
        cmd.env_remove(key);
    }
    let deadline = Instant::now() + timeout;
    let mut process = Process(
        cmd.spawn()
            .map_err(|_| fail("cannot start the headless bridge diagnostic"))?,
    );
    let mut input = process
        .0
        .stdin
        .take()
        .ok_or_else(|| fail("bridge diagnostic has no input"))?;
    let output = process
        .0
        .stdout
        .take()
        .ok_or_else(|| fail("bridge diagnostic has no output"))?;
    let (sender, receiver) = mpsc::sync_channel::<Result<Value, ()>>(1);
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(output);
        loop {
            // `take` bounds a single read before allocation or JSON parsing. EOF mid-line is
            // invalid, and the parent kills the group if it does not answer by the deadline.
            let mut line = Vec::new();
            let read = reader
                .by_ref()
                .take(MAX_FILE + 1)
                .read_until(b'\n', &mut line);
            let value = match read {
                Ok(0) => break,
                Ok(_) if line.len() as u64 <= MAX_FILE && line.last() == Some(&b'\n') => {
                    serde_json::from_slice(&line).map_err(|_| ())
                }
                _ => Err(()),
            };
            let stop = value.is_err();
            if sender.send(value).is_err() || stop {
                break;
            }
        }
    });
    fn exchange(
        input: &mut impl Write,
        receiver: &mpsc::Receiver<Result<Value, ()>>,
        deadline: Instant,
        request: Value,
        expected: u64,
    ) -> Result<Value, String> {
        let mut bytes =
            serde_json::to_vec(&request).map_err(|_| fail("cannot encode bridge diagnostic"))?;
        bytes.push(b'\n');
        input
            .write_all(&bytes)
            .and_then(|()| input.flush())
            .map_err(|_| fail("headless bridge diagnostic input closed"))?;
        let reply = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| fail("headless bridge diagnostic timed out or exited"))?
            .map_err(|_| fail("headless bridge diagnostic sent an invalid or oversized reply"))?;
        if reply.get("id").and_then(Value::as_u64) != Some(expected) || reply.get("error").is_some()
        {
            return Err(fail(
                "headless bridge diagnostic returned an unexpected protocol reply",
            ));
        }
        Ok(reply)
    }
    let init = exchange(
        &mut input,
        &receiver,
        deadline,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"agent-terminal-diagnostic","version":"1"}}}),
        1,
    )?;
    if init
        .pointer("/result/serverInfo/name")
        .and_then(Value::as_str)
        != Some("agent-terminal")
    {
        return Err(fail(
            "headless bridge diagnostic identified an unexpected server",
        ));
    }
    input
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .map_err(|_| fail("headless bridge diagnostic input closed"))?;
    let tools = exchange(
        &mut input,
        &receiver,
        deadline,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        2,
    )?;
    let names = tools
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("headless bridge diagnostic did not list tools"))?;
    if names.len() != 1
        || names[0].get("name").and_then(Value::as_str) != Some("cross_agent_capabilities")
    {
        return Err(fail(
            "unbound headless bridge diagnostic exposed unexpected tools",
        ));
    }
    let capabilities = exchange(
        &mut input,
        &receiver,
        deadline,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"cross_agent_capabilities","arguments":{}}}),
        3,
    )?;
    if capabilities
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        != Some(true)
        || capabilities
            .pointer("/result/structuredContent/error/code")
            .and_then(Value::as_str)
            != Some("runtime_unavailable")
    {
        return Err(fail(
            "headless bridge diagnostic did not report its expected unbound state",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn paths() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let home = tmp.path().to_path_buf();
        let state = home.join(".config/agent-terminal/delegation");
        (tmp, home, state)
    }
    #[test]
    fn configure_is_private_idempotent_preserves_unrelated_and_undoes() {
        let (_tmp, home, state) = paths();
        let mcp = home.join(".gemini/config/mcp_config.json");
        put(
            &home,
            &mcp,
            Some("{\"mcpServers\":{\"other\":{\"command\":\"keep\"}}}"),
        )
        .unwrap();
        let before = fs::read(&mcp).unwrap();
        let command = Path::new("/usr/bin/agent-terminal");
        apply(preview(&home, &state, command, Action::Configure).unwrap()).unwrap();
        assert_eq!(
            fs::metadata(state.join(JOURNAL))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            object(read(&home, &mcp, MAX_FILE).unwrap().as_deref()).unwrap()["mcpServers"]["other"]
                ["command"],
            "keep"
        );
        assert!(!preview(&home, &state, command, Action::Configure)
            .unwrap()
            .changed());
        apply(preview(&home, &state, command, Action::Configure).unwrap()).unwrap();
        undo(&home, &state).unwrap();
        assert_eq!(fs::read(mcp).unwrap(), before);
        assert!(!home.join(".gemini/antigravity-cli/settings.json").exists());
    }
    #[test]
    fn stale_preview_and_later_edit_undo_are_refused() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let plan = preview(&home, &state, command, Action::Configure).unwrap();
        let mcp = home.join(".gemini/config/mcp_config.json");
        put(&home, &mcp, Some("{}")).unwrap();
        assert!(apply(plan).is_err());
        apply(preview(&home, &state, command, Action::Configure).unwrap()).unwrap();
        put(&home, &mcp, Some("{\"later\":true}")).unwrap();
        assert!(undo(&home, &state).is_err());
        assert_eq!(fs::read_to_string(mcp).unwrap(), "{\"later\":true}");
    }
    #[test]
    fn refuses_conflict_corruption_and_symlinks() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let mcp = home.join(".gemini/config/mcp_config.json");
        for body in [
            "bad",
            "[]",
            "{\"mcpServers\":{\"agent_terminal\":{\"command\":\"other\"}}}",
        ] {
            put(&home, &mcp, Some(body)).unwrap();
            assert!(preview(&home, &state, command, Action::Configure).is_err());
        }
        fs::remove_file(&mcp).unwrap();
        symlink("/dev/null", &mcp).unwrap();
        assert!(preview(&home, &state, command, Action::Configure).is_err());
    }
    #[test]
    fn removal_preserves_preexisting_allow_rules_and_unrelated_edits() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let settings = home.join(".gemini/antigravity-cli/settings.json");
        put(
            &home,
            &settings,
            Some(&json!({"permissions":{"allow":[ALLOW_RULES[0],"Read"]}}).to_string()),
        )
        .unwrap();
        apply(preview(&home, &state, command, Action::Configure).unwrap()).unwrap();
        let mut value = object(read(&home, &settings, MAX_FILE).unwrap().as_deref()).unwrap();
        value["later"] = json!("é🙂");
        put(&home, &settings, Some(&value.to_string())).unwrap();
        apply(preview(&home, &state, command, Action::Remove).unwrap()).unwrap();
        let value = object(read(&home, &settings, MAX_FILE).unwrap().as_deref()).unwrap();
        assert_eq!(
            value["permissions"]["allow"],
            json!([ALLOW_RULES[0], "Read"])
        );
        assert_eq!(value["later"], "é🙂");
    }

    #[test]
    fn remove_undo_remove_retains_rule_ownership_without_recursive_undo() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let settings = home.join(".gemini/antigravity-cli/settings.json");
        put(
            &home,
            &settings,
            Some(&json!({"permissions":{"allow":[ALLOW_RULES[0],"Read"]}}).to_string()),
        )
        .unwrap();
        apply(preview(&home, &state, command, Action::Configure).unwrap()).unwrap();
        apply(preview(&home, &state, command, Action::Remove).unwrap()).unwrap();
        undo(&home, &state).unwrap();
        assert!(
            undo(&home, &state).is_err(),
            "another undo must not erase the ownership ledger"
        );
        apply(preview(&home, &state, command, Action::Remove).unwrap()).unwrap();
        let after = object(read(&home, &settings, MAX_FILE).unwrap().as_deref()).unwrap();
        assert_eq!(
            after["permissions"]["allow"],
            json!([ALLOW_RULES[0], "Read"])
        );
        assert!(object(
            read(
                &home,
                &home.join(".gemini/config/mcp_config.json"),
                MAX_FILE
            )
            .unwrap()
            .as_deref()
        )
        .unwrap()["mcpServers"]
            .get(SERVER_NAME)
            .is_none());
    }

    #[test]
    fn parseable_but_invalid_journals_never_remove_unrelated_permissions() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let settings = home.join(".gemini/antigravity-cli/settings.json");
        put(
            &home,
            &settings,
            Some("{\"permissions\":{\"allow\":[\"Read\"]}}"),
        )
        .unwrap();
        apply(preview(&home, &state, command, Action::Configure).unwrap()).unwrap();
        let pristine = read(&home, &state.join(JOURNAL), 16 * MAX_FILE)
            .unwrap()
            .unwrap();
        let before = fs::read(&settings).unwrap();
        for mutation in 0..5 {
            let mut journal: Value = serde_json::from_str(&pristine).unwrap();
            match mutation {
                0 => journal["version"] = json!(255),
                1 => journal["edits"] = json!([]),
                2 => journal["added_rules"] = json!(["Read"]),
                3 => journal["added_rules"] = json!([ALLOW_RULES[0], ALLOW_RULES[0]]),
                _ => journal["previous_added_rules"] = json!(["Read"]),
            }
            put(&home, &state.join(JOURNAL), Some(&journal.to_string())).unwrap();
            assert!(preview(&home, &state, command, Action::Remove).is_err());
            assert!(undo(&home, &state).is_err());
            assert_eq!(fs::read(&settings).unwrap(), before);
        }
    }

    #[test]
    fn an_interrupted_apply_and_interrupted_undo_are_recoverable() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let plan = preview(&home, &state, command, Action::Configure).unwrap();
        let journal = Journal {
            version: 2,
            edits: plan.edits,
            added_rules: plan.added_rules,
            previous_added_rules: plan.previous_added_rules,
            complete: false,
            undo_available: true,
        };
        let _lock = lock(&home, &state).unwrap();
        crate::fsutil::write_private_atomic(
            &state.join(JOURNAL),
            &serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        let first = &journal.edits[0];
        replace(
            &home,
            &first.path,
            first.before.as_deref(),
            first.after.as_deref(),
        )
        .unwrap();
        drop(_lock);
        assert!(
            preview(&home, &state, command, Action::Configure).is_err(),
            "recovery must precede another mutation"
        );
        undo(&home, &state).unwrap();
        assert!(!first.path.exists());
        assert!(!journal.edits[1].path.exists());
        apply(preview(&home, &state, command, Action::Configure).unwrap()).unwrap();
        let journal: Journal = serde_json::from_str(
            &read(&home, &state.join(JOURNAL), 16 * MAX_FILE)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let first = &journal.edits[0];
        replace(
            &home,
            &first.path,
            first.after.as_deref(),
            first.before.as_deref(),
        )
        .unwrap();
        undo(&home, &state).unwrap();
        assert!(!first.path.exists());
        assert!(!journal.edits[1].path.exists());
    }

    #[test]
    fn stale_journal_previews_and_unsafe_homes_fail_closed() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        apply(preview(&home, &state, command, Action::Configure).unwrap()).unwrap();
        let plan = preview(&home, &state, command, Action::Remove).unwrap();
        let mut journal: Value = serde_json::from_str(
            &read(&home, &state.join(JOURNAL), 16 * MAX_FILE)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        journal["undo_available"] = json!(false);
        put(&home, &state.join(JOURNAL), Some(&journal.to_string())).unwrap();
        assert!(apply(plan).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(preview(&home, &state, command, Action::Configure).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o777
    }

    /// A umask-002 layout: owned, but group-writable directory and file.
    fn group_writable(home: &Path) -> (PathBuf, PathBuf) {
        let gemini = home.join(".gemini");
        let mcp = gemini.join("config/mcp_config.json");
        put(
            home,
            &mcp,
            Some("{\"mcpServers\":{\"other\":{\"command\":\"keep\"}}}\n"),
        )
        .unwrap();
        fs::set_permissions(&gemini, fs::Permissions::from_mode(0o775)).unwrap();
        fs::set_permissions(&mcp, fs::Permissions::from_mode(0o664)).unwrap();
        (gemini, mcp)
    }

    #[test]
    fn writable_paths_are_named_and_repaired_only_through_a_reviewed_apply() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let (gemini, mcp) = group_writable(&home);
        let strict = preview(&home, &state, command, Action::Configure)
            .err()
            .unwrap();
        assert!(strict.contains(&gemini.display().to_string()), "{strict}");
        let plan = preview_repairing(&home, &state, command, Action::Configure).unwrap();
        let listed: Vec<_> = plan.repairs().iter().map(Repair::describe).collect();
        assert_eq!(
            listed,
            [
                format!("{}: 775 → 755", gemini.display()),
                format!("{}: 664 → 644", mcp.display()),
            ]
        );
        assert!(plan.summary().contains("775 → 755"));
        // Previewing changes nothing.
        assert_eq!((mode(&gemini), mode(&mcp)), (0o775, 0o664));
        apply(plan).unwrap();
        assert_eq!(mode(&gemini), 0o755);
        let plan = preview(&home, &state, command, Action::Configure).unwrap();
        assert!(!plan.changed(), "configured and safe after one apply");
        let config = object(read(&home, &mcp, MAX_FILE).unwrap().as_deref()).unwrap();
        assert_eq!(config["mcpServers"]["other"]["command"], "keep");
        assert!(config["mcpServers"][SERVER_NAME].is_object());
    }

    #[test]
    fn a_repair_whose_path_changed_since_the_preview_is_refused() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let (gemini, mcp) = group_writable(&home);
        let plan = preview_repairing(&home, &state, command, Action::Configure).unwrap();
        fs::set_permissions(&mcp, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(apply(plan).is_err());
        assert!(!state.join(JOURNAL).exists(), "nothing was registered");
        // The parent was repaired before the changed file was refused; never widened.
        assert_eq!((mode(&gemini), mode(&mcp)), (0o755, 0o666));
    }

    #[test]
    fn links_and_foreign_paths_are_never_offered_as_repairs() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let (_, mcp) = group_writable(&home);
        fs::remove_file(&mcp).unwrap();
        symlink("/dev/null", &mcp).unwrap();
        let error = preview_repairing(&home, &state, command, Action::Configure)
            .err()
            .unwrap();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[test]
    fn formatting_alone_is_not_a_change() {
        let (_tmp, home, state) = paths();
        let command = Path::new("/usr/bin/agent-terminal");
        let mcp = home.join(".gemini/config/mcp_config.json");
        put(&home, &mcp, Some("{\"mcpServers\":{\"other\":{}}}")).unwrap();
        let plan = preview(&home, &state, command, Action::Remove).unwrap();
        assert!(!plan.changed(), "nothing to remove");
    }

    fn diagnostic_script(dir: &Path, body: &str) -> PathBuf {
        let script = dir.join("fake-agent-terminal");
        fs::write(&script, format!("#!/usr/bin/python3\n{body}")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        script
    }

    #[test]
    fn diagnostic_handshakes_in_selected_environment_with_no_binding_credentials() {
        if !Path::new("/usr/bin/python3").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let script = diagnostic_script(
            tmp.path(),
            r#"import json,os,sys
assert sys.argv[1:]==['--cross-agent-mcp']
assert os.environ['REGISTRATION_TEST_PROFILE']=='selected'
assert not any(k.startswith('AGENT_TERMINAL_DELEGATION_') for k in os.environ)
for line in sys.stdin:
 r=json.loads(line)
 if 'id' not in r: continue
 if r['id']==1: result={'serverInfo':{'name':'agent-terminal'}}
 elif r['id']==2: result={'tools':[{'name':'cross_agent_capabilities'}]}
 else: result={'isError':True,'structuredContent':{'error':{'code':'runtime_unavailable'}}}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':result}),flush=True)
"#,
        );
        let mut env = vec![("REGISTRATION_TEST_PROFILE".into(), "selected".into())];
        for key in agent_core::delegation::env::STRIP_ON_SPAWN {
            env.push((key.into(), "SYNTHETIC_TEST_VALUE".into()));
        }
        bridge_diagnostic(&script, &env, &[]).unwrap();
    }

    #[test]
    fn diagnostic_refuses_other_servers_and_never_returns_their_output() {
        if !Path::new("/usr/bin/python3").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let script=diagnostic_script(tmp.path(),"import json,sys\nfor line in sys.stdin:\n r=json.loads(line)\n print(json.dumps({'id':r.get('id'),'result':{'serverInfo':{'name':'private-untrusted-output'}}}),flush=True)\n");
        let error = bridge_diagnostic(&script, &[], &[]).unwrap_err();
        assert!(!error.contains("private-untrusted-output"));
        assert!(error.contains("unexpected server"));
    }

    #[test]
    fn diagnostic_timeout_stops_the_child_process_group() {
        if !Path::new("/usr/bin/python3").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("child.pid");
        let script=diagnostic_script(tmp.path(),"import os,subprocess,time\np=subprocess.Popen(['/bin/sleep','30'])\nwith open(os.environ['REGISTRATION_TEST_PIDFILE'],'w') as f: f.write(str(p.pid))\nwhile True: time.sleep(1)\n");
        assert!(bridge_diagnostic_with_timeout(
            &script,
            &[(
                "REGISTRATION_TEST_PIDFILE".into(),
                pidfile.to_string_lossy().into_owned()
            )],
            &[],
            std::time::Duration::from_secs(1)
        )
        .is_err());
        let pid = fs::read_to_string(pidfile).unwrap();
        // SIGKILL is asynchronous: allow the kernel a moment to reap the group on a loaded
        // runner before concluding the descendant survived.
        let gone = || {
            fs::read_to_string(format!("/proc/{}/stat", pid.trim())).map_or(true, |stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    == Some("Z")
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gone() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone(), "diagnostic descendant is still running");
    }
}
