// SPDX-License-Identifier: GPL-3.0-or-later
//! Build-time tiled and multi-monitor output extension for niri.
//!
//! Niri owns compositor integration through a small backend adapter; this crate owns the output
//! domain model, KMS request construction, frame state, planning, and DRM resources. It is copied
//! into a pinned niri source tree before compilation because niri does not expose a runtime output
//! backend ABI.

mod group;
mod output;
mod plan;

/// KMS-free geometry, refresh, event-ordering, and logical-frame lifecycle primitives.
pub mod frame {
    pub use crate::group::{
        aggregate_member_events, choose_common_refresh, group_layout, merge_render_element_states,
        pacing_refresh_interval, submit_prepared_members, CrtcEventClassification,
        CrtcEventTracker, CrtcSubmissionId, FormationJournal, GammaNodeState, GammaTransaction,
        GroupSubmission, GroupSubmissionOutcome, GroupTeardownLifecycle, LogicalFrameCompletion,
        LogicalFrameDisposition, LogicalFrameEvent, LogicalFrameLifecycle, LogicalFrameOutcome,
        MemberEvent, MemberGeometry, MemberRect, MemberRetirement, MemberSize, PendingGroupFrame,
        RefreshMode, RefreshSync, TeardownCommand, MAX_GROUP_MEMBERS,
    };
}

/// Same-device native TILE controller and KMS resource ownership.
pub mod native {
    pub use crate::output::{
        choose_tiled_scanout_format, destroy_tiled_gamma_blobs, filter_render_formats,
        find_drm_property, is_ccs_modifier, matching_tiled_gamma_size, plan_tiled_groups,
        read_tile_info, resolve_tiled_members, restore_tiled_gamma, set_tiled_gamma,
        tiled_candidate, tiled_claim_pair, tiled_gamma_size, tiled_output_name,
        tiled_scanout_formats, validate_tile_pair, ConnectedTile, GammaProps, OutputId,
        PairRejection, PendingTiledFrame, TileCandidate, TileInfo, TiledCommitState,
        TiledDeviceState, TiledFailure, TiledFailureClass, TiledFailurePolicy, TiledFailureTracker,
        TiledGroup, TiledGroupPlan, TiledMember, TiledMemberPlan, TiledPlanningResult,
        TiledPresentation, TiledRetry, TiledScanoutBuffer, TiledTraceNames, TimingFingerprint,
        ValidatedPair,
    };
}

/// Cross-device discovery snapshot and immutable generalized group plans.
pub mod planning {
    pub use crate::plan::{
        configured_scale_matches, plan_display_groups, ConnectedOutput, GroupMemberClaim,
        GroupMemberPlan, GroupPlanRejection, GroupPlanningResult, OutputGroupPlan,
    };
}
