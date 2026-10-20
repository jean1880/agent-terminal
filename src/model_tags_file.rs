//! The model tag file (`TerminalConfig::model_tags_path`, by default
//! `~/.config/agent-terminal/model-tags.json`), read off the main thread and cached until its
//! modification time or size changes. Parsing is [`agent_core::model_tags::parse`]; a missing or
//! refused file is never an error to the caller, only a reason (`status`) its models carry no
//! tags.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use agent_core::delegation::error::truncate_bytes;
use agent_core::delegation::wire::MAX_TAGS_STATUS_BYTES;
use agent_core::model_tags::{parse, ModelTags};
use gtk4::gio;

/// A larger file is refused unread.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// What the capabilities call gets: the tags, or why there are none.
#[derive(Debug, Clone, Default)]
pub struct TagsState {
    pub tags: Option<Arc<ModelTags>>,
    /// Why `tags` is `None`.
    pub status: Option<String>,
}

/// What identifies one version of the file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

/// One look at the file: `Unchanged` when its stamp is `known`.
#[derive(Debug)]
pub enum Reading {
    Unchanged,
    Loaded(Option<Stamp>, TagsState),
}

/// Blocking: stats `path` and, unless its stamp is `known`, reads and parses it.
pub fn read(path: &Path, known: Option<Stamp>) -> Reading {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Reading::Loaded(None, refused("no model tag file (model-tags.json)"));
        }
        Err(e) => {
            return Reading::Loaded(
                None,
                refused(&format!("model tag file unreadable: {}", e.kind())),
            )
        }
    };
    let stamp = Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    };
    if known == Some(stamp) {
        return Reading::Unchanged;
    }
    if !meta.is_file() {
        return Reading::Loaded(Some(stamp), refused("model tag file is not a regular file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Reading::Loaded(Some(stamp), refused("model tag file is too large"));
    }
    let state = match std::fs::read_to_string(path) {
        Ok(text) => match parse(&text) {
            Ok(tags) => TagsState {
                tags: Some(Arc::new(tags)),
                status: None,
            },
            Err(why) => refused(&format!("model tag file refused: {why}")),
        },
        Err(e) => refused(&format!("model tag file unreadable: {}", e.kind())),
    };
    Reading::Loaded(Some(stamp), state)
}

fn refused(why: &str) -> TagsState {
    TagsState {
        tags: None,
        status: Some(truncate_bytes(why, MAX_TAGS_STATUS_BYTES).to_owned()),
    }
}

/// What an `Unchanged` reading of `path` stands for: the cached state, when the cache still holds
/// `path` (a concurrent call for another path may have replaced it meanwhile).
fn cached_state(last: Option<&(PathBuf, Option<Stamp>, TagsState)>, path: &Path) -> TagsState {
    match last {
        Some((p, _, state)) if p == path => state.clone(),
        _ => refused("model tag file not loaded"),
    }
}

/// The last reading of each path asked for (one in practice).
#[derive(Default)]
pub struct TagsCache {
    last: RefCell<Option<(PathBuf, Option<Stamp>, TagsState)>>,
}

impl TagsCache {
    /// The file's tags: re-read off the main thread only when it changed since the last call.
    pub async fn current(&self, path: PathBuf) -> TagsState {
        let known = self
            .last
            .borrow()
            .as_ref()
            .filter(|(p, _, _)| *p == path)
            .and_then(|(_, stamp, _)| *stamp);
        let job_path = path.clone();
        let reading = gio::spawn_blocking(move || read(&job_path, known))
            .await
            .unwrap_or_else(|_| Reading::Loaded(None, refused("model tag file could not be read")));
        match reading {
            Reading::Unchanged => cached_state(self.last.borrow().as_ref(), &path),
            Reading::Loaded(stamp, state) => {
                *self.last.borrow_mut() = Some((path, stamp, state.clone()));
                state
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = r#"{"version": 1, "vocabulary": {"code": "writes code"},
        "models": [{"match": "*opus*", "tags": ["code"], "tier": "deep"}]}"#;

    #[test]
    fn a_missing_or_refused_file_says_why_and_a_good_one_loads() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("model-tags.json");
        let Reading::Loaded(None, missing) = read(&path, None) else {
            panic!("missing file");
        };
        assert!(missing.tags.is_none());
        assert_eq!(
            missing.status.as_deref(),
            Some("no model tag file (model-tags.json)")
        );

        std::fs::write(&path, r#"{"version": 9, "models": []}"#).expect("write");
        let Reading::Loaded(Some(_), bad) = read(&path, None) else {
            panic!("bad file");
        };
        assert!(bad.tags.is_none());
        assert!(bad
            .status
            .as_deref()
            .is_some_and(|s| s.contains("unsupported version 9")));

        std::fs::write(&path, V1).expect("write");
        let Reading::Loaded(Some(stamp), good) = read(&path, None) else {
            panic!("good file");
        };
        assert!(good.status.is_none());
        let tags = good.tags.expect("tags");
        assert_eq!(tags.vocabulary["code"], "writes code");
        assert!(matches!(read(&path, Some(stamp)), Reading::Unchanged));

        std::fs::write(&path, "x".repeat(MAX_FILE_BYTES as usize + 1)).expect("write");
        let Reading::Loaded(_, big) = read(&path, Some(stamp)) else {
            panic!("big file");
        };
        assert_eq!(big.status.as_deref(), Some("model tag file is too large"));
    }

    #[test]
    fn an_unchanged_reading_of_a_path_no_longer_cached_says_so() {
        let a = PathBuf::from("/a/model-tags.json");
        let loaded = TagsState {
            tags: Some(Arc::new(ModelTags::default())),
            status: None,
        };
        let last = (a.clone(), None, loaded);
        assert!(cached_state(Some(&last), &a).tags.is_some());
        let other = cached_state(Some(&last), Path::new("/b/model-tags.json"));
        assert!(other.tags.is_none());
        assert_eq!(other.status.as_deref(), Some("model tag file not loaded"));
        assert_eq!(
            cached_state(None, &a).status.as_deref(),
            Some("model tag file not loaded")
        );
        // A long reason is cut to the wire bound on a character boundary.
        let long = refused(&"é".repeat(1000));
        let status = long.status.expect("status");
        assert!(status.len() <= MAX_TAGS_STATUS_BYTES && status.starts_with('é'));
    }

    #[test]
    fn the_cache_rereads_only_a_changed_file() {
        use crate::testutil::in_loop;
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("model-tags.json");
        std::fs::write(&path, V1).expect("write");
        in_loop(|ctx| {
            let cache = TagsCache::default();
            let first = ctx.block_on(cache.current(path.clone()));
            let second = ctx.block_on(cache.current(path.clone()));
            let (a, b) = (first.tags.expect("a"), second.tags.expect("b"));
            assert!(Arc::ptr_eq(&a, &b), "unchanged: the cached parse");
            // Another size is another version, whatever the clock's granularity.
            std::fs::write(&path, V1.replace("writes code", "writes more code")).expect("write");
            let third = ctx.block_on(cache.current(path.clone()));
            assert_eq!(
                third.tags.expect("c").vocabulary["code"],
                "writes more code"
            );
            std::fs::remove_file(&path).expect("rm");
            let gone = ctx.block_on(cache.current(path.clone()));
            assert!(gone.tags.is_none() && gone.status.is_some());
        });
    }
}
