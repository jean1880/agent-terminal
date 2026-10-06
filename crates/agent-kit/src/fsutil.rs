//! Small file helpers shared by the app's caches.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes `bytes` to `path` through a `0600` temp file in the same directory, `sync_all`, a
/// rename, then a sync of the directory, so a crash leaves the old file or the new one and never
/// a half-written one, and the contents are never readable by anyone else, even briefly. The
/// directory is created when missing. A failure removes the temp file.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    // Unique per call, not just per process: two threads writing the same file must not share a
    // temp file (one would rename the other's half-written bytes).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(
        ".tmp{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    } else {
        // The rename lives in the directory entry; without this a crash can still lose it. Some
        // filesystems refuse to sync a directory, and the file itself is already safe, so a
        // failure here is not an error.
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    result
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn the_write_is_private_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("sub").join("models.json");
        write_private_atomic(&path, b"one").expect("first");
        write_private_atomic(&path, b"two").expect("second");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "two");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["models.json"]);
    }

    #[test]
    fn concurrent_writers_never_share_a_temp_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("shared.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        let body = format!("{i}").repeat(4096);
                        write_private_atomic(&path, body.as_bytes()).expect("write");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }
        // Whole, from one writer: a shared temp file would interleave digits.
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.len(), 4096);
        let first = text.chars().next().expect("non-empty");
        assert!(text.chars().all(|c| c == first));
        let names: Vec<_> = std::fs::read_dir(dir.path()).expect("dir").collect();
        assert_eq!(names.len(), 1, "no temp file left behind");
    }

    #[test]
    fn a_failed_write_cleans_up_after_itself() {
        let dir = tempfile::tempdir().expect("tmp");
        // The target is a directory, so the rename fails.
        let target = dir.path().join("taken");
        std::fs::create_dir(&target).expect("dir");
        assert!(write_private_atomic(&target, b"x").is_err());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["taken"]);
    }
}
