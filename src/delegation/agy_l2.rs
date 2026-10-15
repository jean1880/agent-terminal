//! Lane D: contract §7 L2, the file-system part (the pure parts are `agent_core::delegation::
//! agy_guard`). Wave 0 stub: always withholds. Blocking: run off the main thread.

use std::path::PathBuf;

use agent_core::delegation::agy_guard::L2Refusal;
use agent_core::delegation::Limits;

/// One agy launch, as L2 judges it: the final argv and environment and the physical cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct L2Input {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub physical_cwd: PathBuf,
    pub verified_home: PathBuf,
    /// No approval hook: the allow rules in `~/.gemini/antigravity-cli/settings.json` must exist.
    pub hookless: bool,
}

/// `Ok` only when every L2 check passes; else why delegation is withheld.
pub(crate) fn scan(_input: &L2Input, _limits: &Limits) -> Result<(), L2Refusal> {
    Err(L2Refusal::DiscoveryFailed)
}
