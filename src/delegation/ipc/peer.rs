//! Lane C: `/proc` facts for the agy L3 peer check and the census (contract §7 L3, P4 §4.3).
//!
//! Blocking file reads: callers run them in `gio::spawn_blocking`, never on the main thread. The
//! parsing (the ppid after the LAST `)`, the bridge argv) is P1's `agy_guard`; this module only
//! reads the files and applies those helpers.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use agent_core::delegation::agy_guard::{is_bridge_cmdline, stat_ppid};

use crate::delegation::seam::ProcFacts;

/// The most of `/proc/<pid>/cmdline` read. A longer argv is never a bridge's (two short
/// fields), so it is returned cut with a non-NUL sentinel field appended: a cut that happened to
/// end right after `--cross-agent-mcp` can never pass as a bridge argv.
const MAX_CMDLINE: usize = 64 * 1024;
/// The most of `/proc/<pid>/stat` read (it is a few hundred bytes).
const MAX_STAT: usize = 16 * 1024;
/// The most of one `task/<tid>/children` file read.
const MAX_CHILDREN: usize = 256 * 1024;

/// Reads `/proc`. A pid that vanished between scan and read is `None` (skipped).
pub(crate) struct RealProc;

impl ProcFacts for RealProc {
    fn stat(&self, pid: u32) -> Option<String> {
        ProcRoot::new("/proc").stat(pid)
    }

    fn exe_id(&self, pid: u32) -> Option<(u64, u64)> {
        ProcRoot::new("/proc").exe_id(pid)
    }

    fn cmdline(&self, pid: u32) -> Option<Vec<u8>> {
        ProcRoot::new("/proc").cmdline(pid)
    }

    fn children_of(&self, ppid: u32) -> Vec<u32> {
        ProcRoot::new("/proc").children_of(ppid)
    }
}

/// [`RealProc`] over any directory laid out like `/proc` (tests point it at a fixture tree).
pub(super) struct ProcRoot {
    root: PathBuf,
}

impl ProcRoot {
    pub(super) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn pid_dir(&self, pid: u32) -> PathBuf {
        self.root.join(pid.to_string())
    }
}

/// At most `max` bytes of `path`, and whether the file held more.
fn read_bounded(path: &Path, max: usize) -> Option<(Vec<u8>, bool)> {
    let file = std::fs::File::open(path).ok()?;
    let mut out = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    file.take(limit).read_to_end(&mut out).ok()?;
    let over = out.len() > max;
    out.truncate(max);
    Some((out, over))
}

/// The pids in a `task/<tid>/children` file: space-separated decimals.
fn parse_children(text: &str) -> impl Iterator<Item = u32> + '_ {
    text.split_ascii_whitespace().filter_map(|p| p.parse().ok())
}

impl ProcFacts for ProcRoot {
    fn stat(&self, pid: u32) -> Option<String> {
        let (bytes, over) = read_bounded(&self.pid_dir(pid).join("stat"), MAX_STAT)?;
        if over {
            return None;
        }
        // `comm` is arbitrary bytes; the fields after it are ASCII, so a lossy decode keeps them.
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn exe_id(&self, pid: u32) -> Option<(u64, u64)> {
        // `metadata` follows the magic link to the executable the process runs.
        let meta = std::fs::metadata(self.pid_dir(pid).join("exe")).ok()?;
        Some((meta.dev(), meta.ino()))
    }

    fn cmdline(&self, pid: u32) -> Option<Vec<u8>> {
        let (mut bytes, over) = read_bounded(&self.pid_dir(pid).join("cmdline"), MAX_CMDLINE)?;
        if over {
            bytes.extend_from_slice(b"\0\xff");
        }
        Some(bytes)
    }

    fn children_of(&self, ppid: u32) -> Vec<u32> {
        let tasks = match std::fs::read_dir(self.pid_dir(ppid).join("task")) {
            Ok(t) => t,
            // The parent is gone: it has no children to count.
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        let mut supported = true;
        for task in tasks.flatten() {
            match read_bounded(&task.path().join("children"), MAX_CHILDREN) {
                Some((bytes, false)) => {
                    out.extend(parse_children(&String::from_utf8_lossy(&bytes)))
                }
                // A thread that exited mid-scan has no children file any more.
                None if task.path().exists() => supported = false,
                None => {}
                Some((_, true)) => supported = false,
            }
        }
        if !supported {
            // A kernel without `CONFIG_PROC_CHILDREN`: scan every process's ppid instead.
            out = self.scan_by_ppid(ppid);
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

impl ProcRoot {
    fn scan_by_ppid(&self, ppid: u32) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| self.stat(*pid).as_deref().and_then(stat_ppid) == Some(ppid))
            .collect()
    }
}

/// The pids that are direct children of `agy_pid`, run the bridge executable `bridge_exe`
/// (`(dev, ino)`) and have a bridge argv: exactly `--cross-agent-mcp` after argv[0], so the
/// app's own `--approval-hook` child (same executable, same parent) is never counted. Each
/// child's ppid is re-read from its `stat`, since the children list and the reads are not
/// atomic; a process that vanished between them is skipped.
pub(crate) fn census(p: &dyn ProcFacts, agy_pid: u32, bridge_exe: (u64, u64)) -> Vec<u32> {
    let mut out: Vec<u32> = p
        .children_of(agy_pid)
        .into_iter()
        .filter(|pid| p.stat(*pid).as_deref().and_then(stat_ppid) == Some(agy_pid))
        .filter(|pid| p.exe_id(*pid) == Some(bridge_exe))
        .filter(|pid| p.cmdline(*pid).is_some_and(|c| is_bridge_cmdline(&c)))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A `/proc`-shaped fixture tree.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("tmp"),
            }
        }

        fn root(&self) -> ProcRoot {
            ProcRoot::new(self.dir.path())
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.dir.path().join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, bytes).expect("write");
        }
    }

    #[test]
    fn stat_keeps_a_comm_with_parens_and_spaces_and_ppid_reads_after_the_last_paren() {
        let f = Fixture::new();
        f.write("40/stat", b"40 (evil) (S 1 x) S 4242 40 40 0 -1 4194560\n");
        f.write("41/stat", b"41 (a b) ) (c) R 7 1 1\n");
        let root = f.root();
        let stat = root.stat(40).expect("stat");
        assert_eq!(stat_ppid(&stat), Some(4242), "{stat}");
        assert_eq!(stat_ppid(&root.stat(41).expect("stat")), Some(7));
        assert_eq!(root.stat(99), None, "a vanished pid");
    }

    #[test]
    fn cmdline_with_and_without_a_trailing_nul() {
        let f = Fixture::new();
        f.write(
            "10/cmdline",
            b"/usr/bin/agent-terminal\0--cross-agent-mcp\0",
        );
        f.write("11/cmdline", b"/usr/bin/agent-terminal\0--cross-agent-mcp");
        f.write("12/cmdline", b"/usr/bin/agent-terminal\0--approval-hook\0");
        let root = f.root();
        for (pid, bridge) in [(10, true), (11, true), (12, false)] {
            let c = root.cmdline(pid).expect("cmdline");
            assert_eq!(is_bridge_cmdline(&c), bridge, "pid {pid}");
        }
        assert_eq!(root.cmdline(99), None);
    }

    #[test]
    fn an_oversized_cmdline_cut_after_the_bridge_flag_is_not_a_bridge() {
        let f = Fixture::new();
        // argv[0] long enough that the cut lands exactly after `--cross-agent-mcp\0`.
        let tail = b"\0--cross-agent-mcp\0";
        let mut bytes = vec![b'a'; MAX_CMDLINE - tail.len()];
        bytes.extend_from_slice(tail);
        bytes.extend_from_slice(b"--extra\0");
        f.write("10/cmdline", &bytes);
        let c = f.root().cmdline(10).expect("cmdline");
        assert!(c.len() <= MAX_CMDLINE + 2);
        assert!(!is_bridge_cmdline(&c));
    }

    #[test]
    fn exe_is_the_dev_and_inode_of_the_link_target() {
        let f = Fixture::new();
        let bin = f.dir.path().join("bridge-bin");
        std::fs::write(&bin, b"#!").expect("bin");
        std::fs::create_dir_all(f.dir.path().join("10")).expect("dir");
        std::os::unix::fs::symlink(&bin, f.dir.path().join("10/exe")).expect("link");
        let meta = std::fs::metadata(&bin).expect("meta");
        assert_eq!(f.root().exe_id(10), Some((meta.dev(), meta.ino())));
        assert_eq!(f.root().exe_id(11), None);
    }

    #[test]
    fn children_are_the_union_over_every_thread() {
        let f = Fixture::new();
        f.write("5/task/5/children", b"10 11 ");
        f.write("5/task/6/children", b"12\n");
        f.write("5/task/7/children", b"");
        assert_eq!(f.root().children_of(5), vec![10, 11, 12]);
        assert!(f.root().children_of(6).is_empty(), "a vanished parent");
    }

    #[test]
    fn children_fall_back_to_a_ppid_scan_without_children_files() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.dir.path().join("5/task/5")).expect("task");
        f.write("10/stat", b"10 (x) S 5 1 1");
        f.write("11/stat", b"11 (y) S 6 1 1");
        f.write("12/stat", b"12 (a) S 9) S 5 1 1");
        assert_eq!(f.root().children_of(5), vec![10, 12]);
    }

    #[test]
    fn real_proc_reads_this_process_and_its_child() {
        let me = std::process::id();
        let p = RealProc;
        assert!(p.stat(me).is_some_and(|s| stat_ppid(&s).is_some()));
        let exe = std::fs::metadata(std::env::current_exe().expect("exe")).expect("meta");
        assert_eq!(p.exe_id(me), Some((exe.dev(), exe.ino())));
        assert!(p.cmdline(me).is_some_and(|c| !c.is_empty()));
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let found = p.children_of(me).contains(&child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(found, "the spawned child is listed");
    }

    #[derive(Default)]
    struct FakeProc {
        stat: HashMap<u32, String>,
        exe: HashMap<u32, (u64, u64)>,
        cmdline: HashMap<u32, Vec<u8>>,
        children: HashMap<u32, Vec<u32>>,
    }

    impl FakeProc {
        fn add(&mut self, pid: u32, ppid: u32, exe: (u64, u64), cmdline: &[u8]) {
            self.stat
                .insert(pid, format!("{pid} (agent) (terminal) S {ppid} 1 1"));
            self.exe.insert(pid, exe);
            self.cmdline.insert(pid, cmdline.to_vec());
            self.children.entry(ppid).or_default().push(pid);
        }
    }

    impl ProcFacts for FakeProc {
        fn stat(&self, pid: u32) -> Option<String> {
            self.stat.get(&pid).cloned()
        }
        fn exe_id(&self, pid: u32) -> Option<(u64, u64)> {
            self.exe.get(&pid).copied()
        }
        fn cmdline(&self, pid: u32) -> Option<Vec<u8>> {
            self.cmdline.get(&pid).cloned()
        }
        fn children_of(&self, ppid: u32) -> Vec<u32> {
            self.children.get(&ppid).cloned().unwrap_or_default()
        }
    }

    const AGY: u32 = 100;
    const BRIDGE: (u64, u64) = (8, 4711);

    #[test]
    fn census_counts_only_bridge_argv_children_running_the_bridge() {
        let mut p = FakeProc::default();
        p.add(200, AGY, BRIDGE, b"agent-terminal\0--cross-agent-mcp\0");
        // The app's own approval hook: same exe, same parent, never a bridge.
        p.add(201, AGY, BRIDGE, b"agent-terminal\0--approval-hook\0");
        // Another executable with a bridge argv.
        p.add(202, AGY, (8, 1), b"agent-terminal\0--cross-agent-mcp");
        // A grandchild bridge.
        p.add(203, 200, BRIDGE, b"agent-terminal\0--cross-agent-mcp");
        assert_eq!(census(&p, AGY, BRIDGE), vec![200]);
        // A second bridge under the same agy is reported: the compromise case.
        p.add(204, AGY, BRIDGE, b"agent-terminal\0--cross-agent-mcp");
        assert_eq!(census(&p, AGY, BRIDGE), vec![200, 204]);
    }

    #[test]
    fn census_skips_vanished_and_reparented_processes() {
        let mut p = FakeProc::default();
        p.add(200, AGY, BRIDGE, b"agent-terminal\0--cross-agent-mcp\0");
        p.add(205, AGY, BRIDGE, b"agent-terminal\0--cross-agent-mcp\0");
        p.cmdline.remove(&205); // exited between the scan and the cmdline read
        p.add(206, AGY, BRIDGE, b"agent-terminal\0--cross-agent-mcp\0");
        p.stat.insert(206, "206 (x) S 1 1 1".into()); // reparented to init since the scan
        assert_eq!(census(&p, AGY, BRIDGE), vec![200]);
    }
}
