//! The verified runtime directory.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use super::seam::RuntimeDir;

/// The directory's name under `$XDG_RUNTIME_DIR`.
const NAME: &str = "agent-terminal";

/// `$XDG_RUNTIME_DIR/agent-terminal`, created `0700` if absent, then proven by `lstat`: a
/// directory, not a symlink, owned by this uid, mode `0700`. There is no `/tmp` fallback: an
/// unset or relative `$XDG_RUNTIME_DIR` is refused. Blocking: run off the main thread.
pub(crate) fn verified() -> Result<RuntimeDir, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set to an absolute path".to_owned())?;
    verified_in(&base, effective_uid())
}

/// The calling process's effective uid.
pub(super) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// [`verified`] under `base`, for the owner `uid`.
pub(super) fn verified_in(base: &Path, uid: u32) -> Result<RuntimeDir, String> {
    let dir = base.join(NAME);
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
    }
    check(&dir, uid)?;
    Ok(RuntimeDir::from_verified(dir))
}

/// The `lstat` proof, used again before every overlay write: a real directory (never a symlink),
/// owned by `uid`, mode exactly `0700`.
pub(super) fn check(dir: &Path, uid: u32) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot inspect {}: {e}", dir.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", dir.display()));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if meta.uid() != uid {
        return Err(format!("{} is not owned by this user", dir.display()));
    }
    if meta.mode() & 0o7777 != 0o700 {
        return Err(format!(
            "{} has mode {:o}, not 700",
            dir.display(),
            meta.mode() & 0o7777
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn creates_a_private_directory_and_accepts_it_again() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let uid = effective_uid();
        let dir = verified_in(tmp.path(), uid).expect("created");
        assert_eq!(dir.path(), tmp.path().join(NAME));
        let meta = std::fs::symlink_metadata(dir.path()).expect("lstat");
        assert_eq!(meta.mode() & 0o7777, 0o700);
        // An existing good directory is accepted as it is.
        assert_eq!(verified_in(tmp.path(), uid), Ok(dir));
    }

    #[test]
    fn refuses_a_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&real)
            .expect("mkdir");
        std::os::unix::fs::symlink(&real, tmp.path().join(NAME)).expect("symlink");
        let err = verified_in(tmp.path(), effective_uid()).expect_err("symlink refused");
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn refuses_a_wrong_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(NAME);
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let err = verified_in(tmp.path(), effective_uid()).expect_err("mode refused");
        assert!(err.contains("mode 755"), "{err}");
    }

    #[test]
    fn refuses_a_wrong_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Tests cannot chown, so the expected owner is another uid instead.
        let other = effective_uid().wrapping_add(1);
        let err = verified_in(tmp.path(), other).expect_err("owner refused");
        assert!(err.contains("not owned"), "{err}");
    }

    #[test]
    fn refuses_a_regular_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(NAME), b"x").expect("write");
        let err = verified_in(tmp.path(), effective_uid()).expect_err("file refused");
        assert!(err.contains("not a directory"), "{err}");
    }
}
