// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure-ish discovery planning for general display groups.
//!
//! This module consumes a stable snapshot of connected outputs from every DRM device. It never
//! reaches back into mutable backend state, which keeps topology reconciliation deterministic and
//! makes the planning boundary unit-testable without a live KMS device.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use niri_config::{
    Config, DisplayGroup, GroupRefreshSync, GroupRenderPolicy, OutputConfigBinding, OutputName,
};
use niri_ipc::Transform;
use smithay::backend::drm::DrmNode;
use smithay::output::Mode as WlMode;
use smithay::reexports::drm::control::{
    connector, crtc, Mode as DrmMode, ModeFlags, ModeTypeFlags,
};

use crate::group::{
    choose_common_refresh, group_layout, MemberGeometry, MemberRect, MemberSize, RefreshMode,
    RefreshSync,
};

#[derive(Debug, Clone)]
pub struct ConnectedOutput {
    pub node: DrmNode,
    pub connector: connector::Info,
    pub crtc: crtc::Handle,
    /// Runtime-unique name used by niri's existing output bookkeeping.
    pub name: OutputName,
    /// Unmodified connector/EDID identity used for stable configuration matching.
    pub identity: OutputName,
}

#[derive(Debug, Clone)]
pub struct GroupMemberPlan {
    pub node: DrmNode,
    pub connector: connector::Info,
    pub crtc: crtc::Handle,
    pub mode: DrmMode,
    pub transform: Transform,
    pub rect: MemberRect,
}

#[derive(Debug, Clone)]
pub struct OutputGroupPlan {
    pub key: String,
    pub name: OutputName,
    pub config: Option<OutputConfigBinding>,
    pub members: Vec<GroupMemberPlan>,
    pub primary: usize,
    pub size: MemberSize,
    pub refresh_sync: RefreshSync,
    pub pacing_refresh: RefreshMode,
    pub composited_only: bool,
    /// Explicit logical scale from the declaring output block. A change requires rebuilding the
    /// hidden member outputs so their transforms, damage history, and logical-to-physical mapping
    /// cannot remain on the old scale.
    pub configured_scale: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPlanRejection {
    pub output: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct GroupPlanningResult {
    pub plans: Vec<OutputGroupPlan>,
    pub claims: HashMap<(DrmNode, crtc::Handle), GroupMemberClaim>,
    pub rejections: Vec<GroupPlanRejection>,
}

/// What reconciliation may do with a present member claimed by an explicit declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMemberClaim {
    /// A complete enabled plan owns the member while it is live or being formed. If formation is
    /// suppressed after a failure, the member may temporarily fall back to an ordinary output.
    Planned,
    /// An off, incomplete, or invalid declaration remains authoritative and suppresses both
    /// automatic grouping and ordinary-output fallback.
    SuppressStandalone,
}

fn explicit_claim_policy(off: bool, plan_succeeded: bool) -> GroupMemberClaim {
    if plan_succeeded && !off {
        GroupMemberClaim::Planned
    } else {
        GroupMemberClaim::SuppressStandalone
    }
}

fn commit_present_claims<K>(
    claims: &mut HashMap<K, GroupMemberClaim>,
    present: impl IntoIterator<Item = K>,
    conflicts_with_prior_group: bool,
    policy: GroupMemberClaim,
) where
    K: Eq + Hash,
{
    if !conflicts_with_prior_group {
        claims.extend(present.into_iter().map(|member| (member, policy)));
    }
}

pub fn configured_scale_matches(previous: Option<f64>, planned: Option<f64>) -> bool {
    previous == planned
}

pub fn plan_display_groups(
    config: &Config,
    connected: &[ConnectedOutput],
    occupied: &HashSet<(DrmNode, crtc::Handle)>,
) -> GroupPlanningResult {
    let mut result = GroupPlanningResult::default();

    for output in config
        .outputs
        .0
        .iter()
        .filter(|output| output.display_group.is_some())
    {
        let group = output.display_group.as_ref().unwrap();
        let prior_claimed = result.claims.keys().copied().collect::<HashSet<_>>();
        let (present_claims, conflicts_with_prior_group) =
            present_member_claims(group, connected, occupied, &prior_claimed);
        match plan_display_group(
            output.name.as_str(),
            group,
            connected,
            occupied,
            &prior_claimed,
        ) {
            Ok(mut plan) => {
                plan.config = Some(OutputConfigBinding::from(output));
                plan.configured_scale = output.scale.map(|scale| scale.0);
                commit_present_claims(
                    &mut result.claims,
                    present_claims,
                    conflicts_with_prior_group,
                    explicit_claim_policy(output.off, true),
                );
                if !output.off {
                    result.plans.push(plan);
                }
            }
            Err(reason) => {
                // An incomplete explicit declaration remains authoritative over auto-detection
                // and standalone fallback for every member that is already present. If the
                // declaration conflicts with an earlier group, commit none of its local claims.
                commit_present_claims(
                    &mut result.claims,
                    present_claims,
                    conflicts_with_prior_group,
                    explicit_claim_policy(output.off, false),
                );
                result.rejections.push(GroupPlanRejection {
                    output: output.name.clone(),
                    reason,
                });
            }
        }
    }

    result
}

fn present_member_claims(
    group: &DisplayGroup,
    connected: &[ConnectedOutput],
    occupied: &HashSet<(DrmNode, crtc::Handle)>,
    already_claimed: &HashSet<(DrmNode, crtc::Handle)>,
) -> (HashSet<(DrmNode, crtc::Handle)>, bool) {
    let mut selected_indices = HashSet::new();
    let mut claims = HashSet::new();
    let mut conflicts = false;

    for configured in &group.members {
        let candidates = connected
            .iter()
            .enumerate()
            .filter(|(idx, output)| {
                !selected_indices.contains(idx)
                    && output_matches_selector(output, &configured.output)
                    && !occupied.contains(&(output.node, output.crtc))
            })
            .collect::<Vec<_>>();
        let choices = candidates
            .iter()
            .map(|(idx, output)| {
                (
                    *idx,
                    output
                        .name
                        .connector
                        .eq_ignore_ascii_case(&configured.output),
                )
            })
            .collect::<Vec<_>>();
        let Ok(idx) = choose_candidate(&choices) else {
            continue;
        };
        let output = &connected[idx];
        selected_indices.insert(idx);
        let member = (output.node, output.crtc);
        conflicts |= already_claimed.contains(&member);
        claims.insert(member);
    }

    (claims, conflicts)
}

fn plan_display_group(
    output_name: &str,
    group: &DisplayGroup,
    connected: &[ConnectedOutput],
    occupied: &HashSet<(DrmNode, crtc::Handle)>,
    already_claimed: &HashSet<(DrmNode, crtc::Handle)>,
) -> Result<OutputGroupPlan, String> {
    let mut selected_indices = HashSet::new();
    let mut selected = Vec::with_capacity(group.members.len());
    for configured in &group.members {
        let candidates = connected
            .iter()
            .enumerate()
            .filter(|(idx, output)| {
                !selected_indices.contains(idx)
                    && output_matches_selector(output, &configured.output)
                    && !occupied.contains(&(output.node, output.crtc))
                    && !already_claimed.contains(&(output.node, output.crtc))
            })
            .collect::<Vec<_>>();
        let choices = candidates
            .iter()
            .map(|(idx, output)| {
                (
                    *idx,
                    output
                        .name
                        .connector
                        .eq_ignore_ascii_case(&configured.output),
                )
            })
            .collect::<Vec<_>>();
        let idx = match choose_candidate(&choices) {
            Ok(idx) => idx,
            Err(CandidateChoiceError::Missing) => {
                return Err(format!(
                    "member `{}` is not connected or is already in use",
                    configured.output
                ));
            }
            Err(CandidateChoiceError::Ambiguous) => {
                return Err(format!(
                    "member selector `{}` matches more than one connected output; use connector names to disambiguate identical displays",
                    configured.output
                ));
            }
        };
        let output = &connected[idx];
        selected_indices.insert(idx);
        selected.push((configured, output));
    }

    let mode_sets = selected
        .iter()
        .map(|(configured, connected)| candidate_modes(&connected.connector, configured.mode))
        .collect::<Vec<_>>();
    if mode_sets.iter().any(Vec::is_empty) {
        return Err("a member has no usable non-interlaced mode".into());
    }
    let refresh_policy = match group.refresh_sync {
        GroupRefreshSync::Strict => RefreshSync::Strict,
        GroupRefreshSync::Relaxed => RefreshSync::Relaxed,
    };
    let refresh_modes = mode_sets
        .iter()
        .map(|modes| {
            modes
                .iter()
                .map(|mode| RefreshMode {
                    millihz: mode_refresh(*mode),
                    preferred: mode.mode_type().contains(ModeTypeFlags::PREFERRED),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let chosen_refresh = choose_common_refresh(&refresh_modes, refresh_policy)
        .ok_or_else(|| "members do not advertise a compatible refresh rate".to_string())?;
    let selected_modes = mode_sets
        .iter()
        .zip(&chosen_refresh)
        .map(|(modes, chosen)| {
            modes
                .iter()
                .copied()
                .min_by_key(|mode| mode_refresh(*mode).abs_diff(chosen.millihz))
                .unwrap()
        })
        .collect::<Vec<_>>();

    let geometries = selected
        .iter()
        .zip(&selected_modes)
        .map(|((configured, _), mode)| {
            let (width, height) = transformed_size(mode.size(), configured.transform);
            let position = if let Some(position) = configured.position {
                let x = u32::try_from(position.x)
                    .map_err(|_| "member positions cannot be negative".to_string())?;
                let y = u32::try_from(position.y)
                    .map_err(|_| "member positions cannot be negative".to_string())?;
                Some((x, y))
            } else {
                None
            };
            Ok(MemberGeometry {
                size: MemberSize {
                    width: u32::from(width),
                    height: u32::from(height),
                },
                position,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let (size, rects) = group_layout(&geometries).map_err(|err| err.to_string())?;

    let primary = group
        .primary
        .as_ref()
        .and_then(|primary| {
            group
                .members
                .iter()
                .position(|member| member.output.eq_ignore_ascii_case(primary))
        })
        .unwrap_or(0);
    let primary_name = &selected[primary].1.identity;
    let name = OutputName {
        connector: output_name.to_owned(),
        make: primary_name.make.clone(),
        model: primary_name.model.clone(),
        serial: primary_name.serial.clone(),
    };
    let members = selected
        .into_iter()
        .zip(selected_modes)
        .zip(rects)
        .map(|(((configured, connected), mode), rect)| GroupMemberPlan {
            node: connected.node,
            connector: connected.connector.clone(),
            crtc: connected.crtc,
            mode,
            transform: configured.transform,
            rect,
        })
        .collect::<Vec<_>>();
    let key = stable_group_key(group.members.iter().map(|member| member.output.clone()));
    let pacing_refresh = chosen_refresh
        .into_iter()
        .min_by_key(|mode| mode.millihz)
        .unwrap();

    Ok(OutputGroupPlan {
        key,
        name,
        // Replaced by the caller, which owns the declaration.
        config: None,
        members,
        primary,
        size,
        refresh_sync: refresh_policy,
        pacing_refresh,
        composited_only: group.render_policy == GroupRenderPolicy::Composited,
        configured_scale: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateChoiceError {
    Missing,
    Ambiguous,
}

/// Choose a unique connector candidate, preferring one exact connector-name match over identity
/// matches. Keeping this decision pure makes ambiguity behavior testable without a DRM device.
fn choose_candidate(candidates: &[(usize, bool)]) -> Result<usize, CandidateChoiceError> {
    let mut exact = candidates.iter().filter(|(_, exact)| *exact);
    if let Some(&(idx, _)) = exact.next() {
        return if exact.next().is_none() {
            Ok(idx)
        } else {
            Err(CandidateChoiceError::Ambiguous)
        };
    }
    match candidates {
        [(idx, _)] => Ok(*idx),
        [] => Err(CandidateChoiceError::Missing),
        [_, _, ..] => Err(CandidateChoiceError::Ambiguous),
    }
}

fn output_matches_selector(output: &ConnectedOutput, selector: &str) -> bool {
    output.name.matches(selector) || output.identity.matches(selector)
}

fn candidate_modes(
    connector: &connector::Info,
    configured: Option<niri_config::output::Mode>,
) -> Vec<DrmMode> {
    let mut modes = connector
        .modes()
        .iter()
        .copied()
        .filter(|mode| !mode.flags().contains(ModeFlags::INTERLACE))
        .filter(|mode| {
            let Some(configured) = configured else {
                return true;
            };
            if configured.custom || mode.size() != (configured.mode.width, configured.mode.height) {
                return false;
            }
            configured.mode.refresh.is_none_or(|refresh| {
                mode_refresh(*mode) == (refresh * 1000.0).round().max(0.0) as u32
            })
        })
        .collect::<Vec<_>>();

    // Refresh negotiation must not change a member's resolution. Without an explicit mode,
    // choose the connector's preferred resolution class first, then search compatible refreshes
    // within that class. Otherwise an exact lower-resolution refresh can beat a near-identical
    // native refresh and make an otherwise valid group fail geometry validation.
    if configured.is_none() {
        retain_preferred_resolution(&mut modes);
    }

    modes
}

fn preferred_mode_size(modes: &[DrmMode]) -> Option<(u16, u16)> {
    modes
        .iter()
        .filter(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .max_by_key(|mode| {
            let (width, height) = mode.size();
            u32::from(width) * u32::from(height)
        })
        .or_else(|| {
            modes.iter().max_by_key(|mode| {
                let (width, height) = mode.size();
                u32::from(width) * u32::from(height)
            })
        })
        .map(|mode| mode.size())
}

fn retain_preferred_resolution(modes: &mut Vec<DrmMode>) {
    if let Some(preferred_size) = preferred_mode_size(modes) {
        modes.retain(|mode| mode.size() == preferred_size);
    }
}

fn mode_refresh(mode: DrmMode) -> u32 {
    WlMode::from(mode).refresh.max(0) as u32
}

fn transformed_size(size: (u16, u16), transform: Transform) -> (u16, u16) {
    match transform {
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
            (size.1, size.0)
        }
        Transform::Normal | Transform::_180 | Transform::Flipped | Transform::Flipped180 => size,
    }
}

fn stable_group_key(members: impl IntoIterator<Item = String>) -> String {
    let mut members = members
        .into_iter()
        .map(|member| member.to_ascii_lowercase())
        .collect::<Vec<_>>();
    members.sort_unstable();
    members.join("\u{1f}")
}

#[cfg(test)]
mod tests {
    use drm_ffi::drm_mode_modeinfo;

    use super::*;

    fn modeinfo(clock: u32, width: u16, height: u16, preferred: bool) -> DrmMode {
        DrmMode::from(drm_mode_modeinfo {
            clock,
            hdisplay: width,
            hsync_start: width + 8,
            hsync_end: width + 32,
            htotal: width + 80,
            vdisplay: height,
            vsync_start: height + 3,
            vsync_end: height + 10,
            vtotal: height + 28,
            vrefresh: 60,
            flags: drm_ffi::DRM_MODE_FLAG_NHSYNC | drm_ffi::DRM_MODE_FLAG_PVSYNC,
            name: [0; 32],
            type_: if preferred {
                drm_ffi::DRM_MODE_TYPE_PREFERRED
            } else {
                drm_ffi::DRM_MODE_TYPE_DRIVER
            },
            hskew: 0,
            vscan: 0,
        })
    }

    #[test]
    fn stable_key_ignores_member_order_and_case() {
        assert_eq!(
            stable_group_key(["DVI-I-2".into(), "dvi-i-1".into()]),
            stable_group_key(["DVI-I-1".into(), "DVI-I-2".into()]),
        );
    }

    #[test]
    fn transformed_portrait_size_swaps_axes() {
        assert_eq!(transformed_size((3840, 2160), Transform::_90), (2160, 3840));
        assert_eq!(
            transformed_size((3840, 2160), Transform::_270),
            (2160, 3840)
        );
        assert_eq!(
            transformed_size((3840, 2160), Transform::Normal),
            (3840, 2160)
        );
    }

    #[test]
    fn preferred_resolution_is_kept_during_refresh_selection() {
        let native = modeinfo(533_223, 3840, 2160, true);
        let lower_exact = modeinfo(148_500, 1920, 1080, false);
        let mut modes = vec![native, lower_exact];

        retain_preferred_resolution(&mut modes);

        assert_eq!(modes, vec![native]);
    }

    #[test]
    fn connector_name_disambiguates_identity_matches() {
        assert_eq!(choose_candidate(&[(2, false), (5, true)]), Ok(5));
        assert_eq!(
            choose_candidate(&[(2, false), (5, false)]),
            Err(CandidateChoiceError::Ambiguous),
        );
        assert_eq!(choose_candidate(&[]), Err(CandidateChoiceError::Missing));
    }

    #[test]
    fn scale_transition_fingerprint_rebuilds_hidden_members() {
        assert!(configured_scale_matches(None, None));
        assert!(configured_scale_matches(Some(1.25), Some(1.25)));
        assert!(!configured_scale_matches(None, Some(1.0)));
        assert!(!configured_scale_matches(Some(1.25), Some(1.5)));
    }

    #[test]
    fn explicit_claim_policy_suppresses_off_and_incomplete_groups() {
        assert_eq!(
            explicit_claim_policy(false, true),
            GroupMemberClaim::Planned
        );
        assert_eq!(
            explicit_claim_policy(true, true),
            GroupMemberClaim::SuppressStandalone
        );
        assert_eq!(
            explicit_claim_policy(false, false),
            GroupMemberClaim::SuppressStandalone
        );
        assert_eq!(
            explicit_claim_policy(true, false),
            GroupMemberClaim::SuppressStandalone
        );
    }

    #[test]
    fn conflicting_explicit_group_commits_no_local_claims() {
        let mut claims = HashMap::from([(1_u8, GroupMemberClaim::Planned)]);
        commit_present_claims(
            &mut claims,
            [2_u8, 3],
            true,
            GroupMemberClaim::SuppressStandalone,
        );
        assert_eq!(claims, HashMap::from([(1, GroupMemberClaim::Planned)]));

        commit_present_claims(
            &mut claims,
            [2_u8, 3],
            false,
            GroupMemberClaim::SuppressStandalone,
        );
        assert_eq!(claims.len(), 3);
        assert_eq!(claims[&2], GroupMemberClaim::SuppressStandalone);
        assert_eq!(claims[&3], GroupMemberClaim::SuppressStandalone);
    }
}
