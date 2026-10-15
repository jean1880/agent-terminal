//! Lane C: `/proc` facts for the agy L3 peer check and the census (§4.3). Wave 0 stub: knows
//! nothing, so every agy hello fails L3.

use crate::delegation::seam::ProcFacts;

/// Reads `/proc`. A pid that vanished between scan and read is `None` (skipped).
pub(crate) struct RealProc;

impl ProcFacts for RealProc {
    fn stat(&self, _pid: u32) -> Option<String> {
        None
    }

    fn exe_id(&self, _pid: u32) -> Option<(u64, u64)> {
        None
    }

    fn cmdline(&self, _pid: u32) -> Option<Vec<u8>> {
        None
    }

    fn children_of(&self, _ppid: u32) -> Vec<u32> {
        Vec::new()
    }
}

/// The pids that are direct children of `agy_pid`, run the bridge executable `bridge_exe`
/// (`(dev, ino)`) and have a bridge argv.
pub(crate) fn census(_p: &dyn ProcFacts, _agy_pid: u32, _bridge_exe: (u64, u64)) -> Vec<u32> {
    Vec::new()
}
