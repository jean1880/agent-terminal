//! Route admission (contract §11) and per-driver delegation role facts.
//!
//! [`DelegationCaps`] is separate from `caps::Capabilities` on purpose: those are UI features
//! guarded one to one by `Feature::ALL`; these are role facts no control renders. The same
//! exhaustive-destructure guard test keeps them honest.

use serde::{Deserialize, Serialize};

use super::authority::TaskMode;
use super::wire::ThisRunSupport;
use crate::adapter::Driver;

/// Whether a route may be used (contract §3 `route_status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStatus {
    Verified,
    Unverified,
    Unsupported,
}

/// What still blocks a route: a proof gate or a missing piece of work (contract §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteBlocker {
    /// G-C1 (writer modes only).
    ClaudeBashUnderRestricted,
    /// G-C3.
    ClaudeTokenPersistence,
    /// G-X1.
    CodexAmbientMcp,
    /// G-X3.
    CodexTokenPersistence,
    /// The adapter resends the overlay on every resume.
    CodexOverlayOnResume,
    /// PR #44 merged: workspace plugin hooks, config-dir flags dropped, fresh proof per start.
    AgyHookProofFix,
    AgyHookProofInWorktree,
    /// §7 L2 in full (non-git walk, recursive scan, argv/env/home, allow rules).
    AgyWorkspaceMcpCheck,
    /// §7 L3 + census + L4 + hello window.
    AgyPeerAndSingleUse,
    /// G-A5.
    AgyAttestation,
    /// G-A1: also denies other servers and the sub-agent tools in children.
    AgyHookPayloadUnrecorded,
    /// G-A3.
    AgyTokenPersistence,
    AgyManagedEntry,
    AgyAllowRules,
    AgentSyncEntry,
    /// The route's P5/P8 evidence bundle is not recorded.
    EvidenceBundle,
}

impl RouteBlocker {
    pub const ALL: [RouteBlocker; 16] = [
        RouteBlocker::ClaudeBashUnderRestricted,
        RouteBlocker::ClaudeTokenPersistence,
        RouteBlocker::CodexAmbientMcp,
        RouteBlocker::CodexTokenPersistence,
        RouteBlocker::CodexOverlayOnResume,
        RouteBlocker::AgyHookProofFix,
        RouteBlocker::AgyHookProofInWorktree,
        RouteBlocker::AgyWorkspaceMcpCheck,
        RouteBlocker::AgyPeerAndSingleUse,
        RouteBlocker::AgyAttestation,
        RouteBlocker::AgyHookPayloadUnrecorded,
        RouteBlocker::AgyTokenPersistence,
        RouteBlocker::AgyManagedEntry,
        RouteBlocker::AgyAllowRules,
        RouteBlocker::AgentSyncEntry,
        RouteBlocker::EvidenceBundle,
    ];

    /// The snake_case name reported in `route_blockers` (at most 64 bytes).
    pub fn as_str(self) -> &'static str {
        match self {
            RouteBlocker::ClaudeBashUnderRestricted => "claude_bash_under_restricted",
            RouteBlocker::ClaudeTokenPersistence => "claude_token_persistence",
            RouteBlocker::CodexAmbientMcp => "codex_ambient_mcp",
            RouteBlocker::CodexTokenPersistence => "codex_token_persistence",
            RouteBlocker::CodexOverlayOnResume => "codex_overlay_on_resume",
            RouteBlocker::AgyHookProofFix => "agy_hook_proof_fix",
            RouteBlocker::AgyHookProofInWorktree => "agy_hook_proof_in_worktree",
            RouteBlocker::AgyWorkspaceMcpCheck => "agy_workspace_mcp_check",
            RouteBlocker::AgyPeerAndSingleUse => "agy_peer_and_single_use",
            RouteBlocker::AgyAttestation => "agy_attestation",
            RouteBlocker::AgyHookPayloadUnrecorded => "agy_hook_payload_unrecorded",
            RouteBlocker::AgyTokenPersistence => "agy_token_persistence",
            RouteBlocker::AgyManagedEntry => "agy_managed_entry",
            RouteBlocker::AgyAllowRules => "agy_allow_rules",
            RouteBlocker::AgentSyncEntry => "agent_sync_entry",
            RouteBlocker::EvidenceBundle => "evidence_bundle",
        }
    }
}

/// Which task modes a blocker counts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockerScope {
    AllModes,
    WriterModes,
}

/// G-C1 is `WriterModes`; every other blocker `AllModes`.
pub fn blocker_scope(b: RouteBlocker) -> BlockerScope {
    match b {
        RouteBlocker::ClaudeBashUnderRestricted => BlockerScope::WriterModes,
        RouteBlocker::ClaudeTokenPersistence
        | RouteBlocker::CodexAmbientMcp
        | RouteBlocker::CodexTokenPersistence
        | RouteBlocker::CodexOverlayOnResume
        | RouteBlocker::AgyHookProofFix
        | RouteBlocker::AgyHookProofInWorktree
        | RouteBlocker::AgyWorkspaceMcpCheck
        | RouteBlocker::AgyPeerAndSingleUse
        | RouteBlocker::AgyAttestation
        | RouteBlocker::AgyHookPayloadUnrecorded
        | RouteBlocker::AgyTokenPersistence
        | RouteBlocker::AgyManagedEntry
        | RouteBlocker::AgyAllowRules
        | RouteBlocker::AgentSyncEntry
        | RouteBlocker::EvidenceBundle => BlockerScope::AllModes,
    }
}

/// An agy child's variant: with its own token and bridge, or without (contract §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildBridge {
    WithBridge,
    /// Only meaningful for an agy child; other children always have their bridge.
    WithoutBridge,
}

/// One route: `(parent driver, child driver, task_mode, child bridge)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteQuery {
    pub parent: Driver,
    pub child: Driver,
    pub task_mode: TaskMode,
    pub child_bridge: ChildBridge,
}

/// A route's admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    pub status: RouteStatus,
    pub blockers: Vec<RouteBlocker>,
}

fn parent_group(d: Driver) -> &'static [RouteBlocker] {
    use RouteBlocker::*;
    match d {
        Driver::Claude => &[ClaudeTokenPersistence],
        Driver::Codex => &[CodexTokenPersistence, CodexOverlayOnResume],
        Driver::Agy => &[
            AgyManagedEntry,
            AgyAllowRules,
            AgentSyncEntry,
            AgyHookProofFix,
            AgyWorkspaceMcpCheck,
            AgyPeerAndSingleUse,
            AgyAttestation,
            AgyTokenPersistence,
        ],
    }
}

fn child_group(d: Driver, bridge: ChildBridge) -> &'static [RouteBlocker] {
    use RouteBlocker::*;
    match (d, bridge) {
        (Driver::Claude, _) => &[ClaudeTokenPersistence, ClaudeBashUnderRestricted],
        (Driver::Codex, _) => &[CodexAmbientMcp, CodexTokenPersistence, CodexOverlayOnResume],
        (Driver::Agy, ChildBridge::WithoutBridge) => &[
            AgyHookProofFix,
            AgyHookProofInWorktree,
            AgyHookPayloadUnrecorded,
        ],
        (Driver::Agy, ChildBridge::WithBridge) => &[
            AgyHookProofFix,
            AgyHookProofInWorktree,
            AgyHookPayloadUnrecorded,
            AgyManagedEntry,
            AgyAllowRules,
            AgyWorkspaceMcpCheck,
            AgyPeerAndSingleUse,
            AgyAttestation,
            AgyTokenPersistence,
        ],
    }
}

/// Reviewed evidence for an individual gate. A passing provider proof never clears a separate
/// application/source-review gate. The recorded proofs are in `tests/fixtures/real/delegation/route-proof/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateEvidence {
    Passed,
    PendingReview,
    Unsupported,
}

/// This exhaustive table is the admission checkpoint: each remaining implementation gate must
/// be reviewed explicitly rather than being cleared by an unrelated successful provider run.
pub fn gate_evidence(gate: RouteBlocker) -> GateEvidence {
    use GateEvidence::*;
    use RouteBlocker::*;
    match gate {
        ClaudeBashUnderRestricted
        | ClaudeTokenPersistence
        | CodexAmbientMcp
        | CodexTokenPersistence
        | CodexOverlayOnResume
        | AgyHookProofFix
        | AgyAttestation
        | AgyHookPayloadUnrecorded
        | AgyTokenPersistence
        | AgyHookProofInWorktree
        | AgyWorkspaceMcpCheck
        | AgyPeerAndSingleUse
        | AgyManagedEntry
        | AgyAllowRules
        | AgentSyncEntry => Passed,
        // Bundle admission remains route-specific; bridge-free AGY is not implemented.
        EvidenceBundle => PendingReview,
    }
}

fn reviewed_bundle(q: &RouteQuery) -> bool {
    match (q.child, q.child_bridge) {
        (Driver::Claude | Driver::Codex, _) | (Driver::Agy, ChildBridge::WithBridge) => true,
        (Driver::Agy, ChildBridge::WithoutBridge) => false,
    }
}

/// What a route allows but cannot supervise: admitted (the user chose it), and reported to the
/// parent beside the route so it delegates knowing the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteCaveat {
    /// G-X2: Codex 0.161.0 starts its own native workers despite both the process and the
    /// thread disables. They run outside the app's depth, mode and worktree limits; the app
    /// shows them in the worker dock but cannot stop them starting.
    CodexNativeWorkersUnsupervised,
}

impl RouteCaveat {
    pub const ALL: [RouteCaveat; 1] = [RouteCaveat::CodexNativeWorkersUnsupervised];

    /// The snake_case name reported in `route_caveats` (at most 64 bytes).
    pub fn as_str(self) -> &'static str {
        match self {
            RouteCaveat::CodexNativeWorkersUnsupervised => "codex_native_workers_unsupervised",
        }
    }

    /// One line for a person: the setup screen and the capabilities response.
    pub fn explain(self) -> &'static str {
        match self {
            RouteCaveat::CodexNativeWorkersUnsupervised => {
                "Codex starts its own built-in sub-agents even as a delegated worker. They run \
                 outside Agent Terminal's depth, mode and worktree limits."
            }
        }
    }
}

/// The caveats of a route: properties of its child, whatever the parent or mode.
pub fn route_caveats(child: Driver) -> &'static [RouteCaveat] {
    match child {
        Driver::Codex => &[RouteCaveat::CodexNativeWorkersUnsupervised],
        Driver::Claude | Driver::Agy => &[],
    }
}

/// Contract §11 route admission: the parent group plus the child group (variant-specific for
/// agy), mode-conditional blockers counted only for their modes, plus the evidence bundle.
/// `child_bridge` is ignored for a non-agy child (it always has its bridge). A child's
/// unsupervised behaviour is a caveat ([`route_caveats`]), never a blocker.
pub fn admit_route(q: &RouteQuery) -> RouteSpec {
    let mut blockers: Vec<RouteBlocker> = Vec::new();
    let groups = parent_group(q.parent)
        .iter()
        .chain(child_group(q.child, q.child_bridge));
    for &b in groups {
        let counts = match blocker_scope(b) {
            BlockerScope::AllModes => true,
            BlockerScope::WriterModes => q.task_mode.writes(),
        };
        if counts && gate_evidence(b) != GateEvidence::Passed && !blockers.contains(&b) {
            blockers.push(b);
        }
    }
    let normalised = RouteQuery {
        child_bridge: if q.child == Driver::Agy {
            q.child_bridge
        } else {
            ChildBridge::WithBridge
        },
        ..*q
    };
    if !reviewed_bundle(&normalised) {
        blockers.push(RouteBlocker::EvidenceBundle);
    }
    let status = if blockers.is_empty() {
        RouteStatus::Verified
    } else {
        RouteStatus::Unverified
    };
    RouteSpec { status, blockers }
}

/// Picks an agy child's variant from the two admissions: with bridge if admitted and L2
/// passed, else without bridge if admitted, else `None` (`route_unverified`).
pub fn choose_agy_variant(
    with_bridge: RouteStatus,
    without_bridge: RouteStatus,
    l2_passed: bool,
) -> Option<ChildBridge> {
    if with_bridge == RouteStatus::Verified && l2_passed {
        Some(ChildBridge::WithBridge)
    } else if without_bridge == RouteStatus::Verified {
        Some(ChildBridge::WithoutBridge)
    } else {
        None
    }
}

/// The agy child variant the supervisor uses for `parent` and `mode`.
pub fn agy_child_variant(parent: Driver, mode: TaskMode, l2_passed: bool) -> Option<ChildBridge> {
    let q = |child_bridge| RouteQuery {
        parent,
        child: Driver::Agy,
        task_mode: mode,
        child_bridge,
    };
    choose_agy_variant(
        admit_route(&q(ChildBridge::WithBridge)).status,
        admit_route(&q(ChildBridge::WithoutBridge)).status,
        l2_passed,
    )
}

/// How a parent's bridge entry reaches the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentBinding {
    /// Claude `--mcp-config <overlay>`.
    LaunchOverlay,
    /// Codex `thread/start` / `thread/resume` config.
    PerThreadConfig,
    /// agy's managed entry in `~/.gemini/config/mcp_config.json`.
    PersistentGlobal,
}

/// Where the token travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenPlacement {
    /// Only in the bridge server's own `env`.
    ServerEnv,
    /// In the provider process's environment (agy, the only channel).
    ProcessEnv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenReuse {
    /// Re-hello with the same token within a generation.
    SequentialPerGeneration,
    /// The first accepted hello consumes it (§7 L4).
    SingleUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeRestart {
    RespawnedByProvider,
    DeadUntilResume,
    DeadUntilRelaunch,
}

/// What lets the provider call our 8 tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeGate {
    /// Claude `can_use_tool` for `agent_terminal` with `source: "dynamic"`.
    HostAutoAllowByServerName,
    /// Codex `default_tools_approval_mode = "approve"`.
    ApproveMode,
    /// agy allow rules (hookless) or the app hook.
    AllowRulesOrHook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanEnforcement {
    ToolList,
    Sandbox,
    Hook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildMcpIsolation {
    /// Claude `--strict-mcp-config`.
    StrictConfig,
    /// Codex `enabled=false` per server, read back.
    DisabledAndReadBack,
    /// agy: the app hook denies other servers.
    HookDeniesOthers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeDelegationControl {
    ToolListReadBack,
    Unproven,
    HookDenies,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebind {
    ResumeWithFreshOverlay,
    ThreadResumeWithFreshConfig,
    RelaunchConversation,
}

/// How a delivered message is confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipt {
    /// Claude `--replay-user-messages` echoes, FIFO.
    ReplayEchoFifo,
    /// Codex `turn/steer` plus the `userMessage` item.
    SteerAndItem,
    None,
}

/// A driver's delegation role facts (contract §5 mapping table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegationCaps {
    pub parent_binding: ParentBinding,
    pub token_placement: TokenPlacement,
    pub token_reuse: TokenReuse,
    /// The §7 L3 peer check is enforced (advisory for Claude and Codex).
    pub peer_check_required: bool,
    /// Every call must be attested (agy; Claude and Codex once G-C5/G-X5 pass).
    pub attestation_required: bool,
    pub bridge_restart: BridgeRestart,
    pub rebind: Rebind,
    pub bridge_gate: BridgeGate,
    pub child_requires_restricted: bool,
    pub child_requires_proven_hook: bool,
    pub child_mcp_isolation: ChildMcpIsolation,
    pub native_delegation: NativeDelegationControl,
    pub plan_enforcement: PlanEnforcement,
    /// Codex `Strict`, Claude `BestEffort`, agy `Unsupported`.
    pub this_run: ThisRunSupport,
    pub receipt: Receipt,
    /// agy: write the next message only after a `result`.
    pub write_only_after_result: bool,
    /// The provider sends `notifications/cancelled` on interrupt (none was observed).
    pub mcp_cancel_on_interrupt: bool,
}

impl DelegationCaps {
    pub fn of(d: Driver) -> Self {
        match d {
            Driver::Claude => Self {
                parent_binding: ParentBinding::LaunchOverlay,
                token_placement: TokenPlacement::ServerEnv,
                token_reuse: TokenReuse::SequentialPerGeneration,
                peer_check_required: false,
                attestation_required: false,
                bridge_restart: BridgeRestart::RespawnedByProvider,
                rebind: Rebind::ResumeWithFreshOverlay,
                bridge_gate: BridgeGate::HostAutoAllowByServerName,
                child_requires_restricted: true,
                child_requires_proven_hook: false,
                child_mcp_isolation: ChildMcpIsolation::StrictConfig,
                native_delegation: NativeDelegationControl::ToolListReadBack,
                plan_enforcement: PlanEnforcement::ToolList,
                this_run: ThisRunSupport::BestEffort,
                receipt: Receipt::ReplayEchoFifo,
                write_only_after_result: false,
                mcp_cancel_on_interrupt: false,
            },
            Driver::Codex => Self {
                parent_binding: ParentBinding::PerThreadConfig,
                token_placement: TokenPlacement::ServerEnv,
                token_reuse: TokenReuse::SequentialPerGeneration,
                peer_check_required: false,
                attestation_required: false,
                bridge_restart: BridgeRestart::DeadUntilResume,
                rebind: Rebind::ThreadResumeWithFreshConfig,
                bridge_gate: BridgeGate::ApproveMode,
                child_requires_restricted: false,
                child_requires_proven_hook: false,
                child_mcp_isolation: ChildMcpIsolation::DisabledAndReadBack,
                native_delegation: NativeDelegationControl::Unproven,
                plan_enforcement: PlanEnforcement::Sandbox,
                this_run: ThisRunSupport::Strict,
                receipt: Receipt::SteerAndItem,
                write_only_after_result: false,
                mcp_cancel_on_interrupt: false,
            },
            Driver::Agy => Self {
                parent_binding: ParentBinding::PersistentGlobal,
                token_placement: TokenPlacement::ProcessEnv,
                token_reuse: TokenReuse::SingleUse,
                peer_check_required: true,
                attestation_required: true,
                bridge_restart: BridgeRestart::DeadUntilRelaunch,
                rebind: Rebind::RelaunchConversation,
                bridge_gate: BridgeGate::AllowRulesOrHook,
                child_requires_restricted: false,
                child_requires_proven_hook: true,
                child_mcp_isolation: ChildMcpIsolation::HookDeniesOthers,
                native_delegation: NativeDelegationControl::HookDenies,
                plan_enforcement: PlanEnforcement::Hook,
                this_run: ThisRunSupport::Unsupported,
                receipt: Receipt::None,
                write_only_after_result: true,
                mcp_cancel_on_interrupt: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Adding a field to `DelegationCaps` stops this compiling until it is named here and set
    /// for every driver.
    #[test]
    fn every_delegation_caps_field_is_named() {
        for d in Driver::ALL {
            let DelegationCaps {
                parent_binding: _,
                token_placement: _,
                token_reuse: _,
                peer_check_required: _,
                attestation_required: _,
                bridge_restart: _,
                rebind: _,
                bridge_gate: _,
                child_requires_restricted: _,
                child_requires_proven_hook: _,
                child_mcp_isolation: _,
                native_delegation: _,
                plan_enforcement: _,
                this_run: _,
                receipt: _,
                write_only_after_result: _,
                mcp_cancel_on_interrupt: _,
            } = DelegationCaps::of(d);
        }
    }

    #[test]
    fn route_blocker_all_is_exhaustive() {
        use RouteBlocker::*;
        fn next(b: RouteBlocker) -> Option<RouteBlocker> {
            match b {
                ClaudeBashUnderRestricted => Some(ClaudeTokenPersistence),
                ClaudeTokenPersistence => Some(CodexAmbientMcp),
                CodexAmbientMcp => Some(CodexTokenPersistence),
                CodexTokenPersistence => Some(CodexOverlayOnResume),
                CodexOverlayOnResume => Some(AgyHookProofFix),
                AgyHookProofFix => Some(AgyHookProofInWorktree),
                AgyHookProofInWorktree => Some(AgyWorkspaceMcpCheck),
                AgyWorkspaceMcpCheck => Some(AgyPeerAndSingleUse),
                AgyPeerAndSingleUse => Some(AgyAttestation),
                AgyAttestation => Some(AgyHookPayloadUnrecorded),
                AgyHookPayloadUnrecorded => Some(AgyTokenPersistence),
                AgyTokenPersistence => Some(AgyManagedEntry),
                AgyManagedEntry => Some(AgyAllowRules),
                AgyAllowRules => Some(AgentSyncEntry),
                AgentSyncEntry => Some(EvidenceBundle),
                EvidenceBundle => None,
            }
        }
        let mut walked = vec![ClaudeBashUnderRestricted];
        while let Some(b) = next(*walked.last().expect("non-empty")) {
            walked.push(b);
        }
        assert_eq!(walked, RouteBlocker::ALL);
        for b in RouteBlocker::ALL {
            assert!(b.as_str().len() <= 64);
            assert_eq!(
                serde_json::to_value(b).expect("ser"),
                serde_json::json!(b.as_str())
            );
        }
    }

    #[test]
    fn every_route_has_the_reviewed_status_and_unique_blockers() {
        for parent in Driver::ALL {
            for child in Driver::ALL {
                for task_mode in TaskMode::ALL {
                    for child_bridge in [ChildBridge::WithBridge, ChildBridge::WithoutBridge] {
                        let spec = admit_route(&RouteQuery {
                            parent,
                            child,
                            task_mode,
                            child_bridge,
                        });
                        if child == Driver::Agy && child_bridge == ChildBridge::WithoutBridge {
                            assert_eq!(spec.status, RouteStatus::Unverified);
                            assert_eq!(spec.blockers, vec![RouteBlocker::EvidenceBundle]);
                        } else {
                            assert_eq!(spec.status, RouteStatus::Verified);
                            assert!(spec.blockers.is_empty());
                        }
                        let mut seen = std::collections::HashSet::new();
                        assert!(spec.blockers.iter().all(|b| seen.insert(*b)));
                    }
                }
            }
        }
    }

    fn blockers(
        parent: Driver,
        child: Driver,
        mode: TaskMode,
        bridge: ChildBridge,
    ) -> Vec<RouteBlocker> {
        admit_route(&RouteQuery {
            parent,
            child,
            task_mode: mode,
            child_bridge: bridge,
        })
        .blockers
    }

    #[test]
    fn reviewed_provider_and_application_gates_do_not_admit_unimplemented_variants() {
        use RouteBlocker::*;
        for gate in RouteBlocker::ALL {
            let expected = match gate {
                EvidenceBundle => GateEvidence::PendingReview,
                _ => GateEvidence::Passed,
            };
            assert_eq!(gate_evidence(gate), expected, "{gate:?}");
        }
        for parent in Driver::ALL {
            for mode in TaskMode::ALL {
                for child in Driver::ALL {
                    assert!(blockers(parent, child, mode, ChildBridge::WithBridge).is_empty());
                }
                assert_eq!(
                    blockers(parent, Driver::Agy, mode, ChildBridge::WithoutBridge),
                    vec![EvidenceBundle]
                );
            }
        }
        assert_eq!(
            blocker_scope(ClaudeBashUnderRestricted),
            BlockerScope::WriterModes
        );
        assert_eq!(blocker_scope(EvidenceBundle), BlockerScope::AllModes);
    }

    /// Every agent can be every agent's sub-agent; Codex as a child carries its caveat, which
    /// never turns into a blocker.
    #[test]
    fn every_agent_pair_is_admitted_and_only_codex_children_carry_a_caveat() {
        for parent in Driver::ALL {
            for child in Driver::ALL {
                for task_mode in TaskMode::ALL {
                    let spec = admit_route(&RouteQuery {
                        parent,
                        child,
                        task_mode,
                        child_bridge: ChildBridge::WithBridge,
                    });
                    assert_eq!(spec.status, RouteStatus::Verified, "{parent:?} → {child:?}");
                }
                let expected: &[RouteCaveat] = if child == Driver::Codex {
                    &[RouteCaveat::CodexNativeWorkersUnsupervised]
                } else {
                    &[]
                };
                assert_eq!(route_caveats(child), expected);
            }
        }
        // Adding a caveat stops this compiling until it is placed in `ALL`.
        fn position(c: RouteCaveat) -> usize {
            match c {
                RouteCaveat::CodexNativeWorkersUnsupervised => 0,
            }
        }
        for (i, c) in RouteCaveat::ALL.into_iter().enumerate() {
            assert_eq!(position(c), i);
            assert!(c.as_str().len() <= 64);
            assert_eq!(
                serde_json::to_value(c).expect("ser"),
                serde_json::json!(c.as_str())
            );
            assert!(!c.explain().is_empty());
        }
    }

    #[test]
    fn agy_child_variant_falls_back_without_the_bridge() {
        use RouteStatus::*;
        assert_eq!(
            choose_agy_variant(Verified, Verified, true),
            Some(ChildBridge::WithBridge)
        );
        assert_eq!(
            choose_agy_variant(Verified, Verified, false),
            Some(ChildBridge::WithoutBridge)
        );
        assert_eq!(
            choose_agy_variant(Unverified, Verified, true),
            Some(ChildBridge::WithoutBridge)
        );
        assert_eq!(choose_agy_variant(Verified, Unverified, false), None);
        assert_eq!(choose_agy_variant(Unverified, Unverified, true), None);
        for parent in Driver::ALL {
            for m in TaskMode::ALL {
                assert_eq!(
                    agy_child_variant(parent, m, true),
                    Some(ChildBridge::WithBridge)
                );
                assert_eq!(agy_child_variant(parent, m, false), None);
            }
        }
    }

    #[test]
    fn delegation_caps_match_the_mapping_table() {
        assert_eq!(
            DelegationCaps::of(Driver::Codex).this_run,
            ThisRunSupport::Strict
        );
        assert_eq!(
            DelegationCaps::of(Driver::Claude).this_run,
            ThisRunSupport::BestEffort
        );
        let agy = DelegationCaps::of(Driver::Agy);
        assert_eq!(agy.this_run, ThisRunSupport::Unsupported);
        assert!(agy.attestation_required);
        assert_eq!(agy.token_reuse, TokenReuse::SingleUse);
        assert_eq!(agy.token_placement, TokenPlacement::ProcessEnv);
        assert!(DelegationCaps::of(Driver::Claude).child_requires_restricted);
        for d in [Driver::Claude, Driver::Codex] {
            let caps = DelegationCaps::of(d);
            assert!(!caps.attestation_required);
            assert_eq!(caps.token_reuse, TokenReuse::SequentialPerGeneration);
            assert_eq!(caps.token_placement, TokenPlacement::ServerEnv);
        }
    }
}
