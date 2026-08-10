// SPDX-License-Identifier: GPL-3.0-or-later
//! Native tiled-output support ("stitch"): presents two physical tiles of one panel as a
//! single logical output.
//!
//! The motivating hardware is the LG UltraFine 5K: a dual-SST panel connected over two
//! independent DisplayPort links (two connector/crtc/plane triples on the same DRM device),
//! each tile running an identical native mode (e.g. 2560x2880@60). The code generalizes to
//! any horizontal dual-tile pair with byte-identical timings.
//!
//! Design notes:
//!
//! * One [`smithay::output::Output`] is backed by two CRTCs. We render one framebuffer at the full
//!   synthesized size (e.g. 5120x2880) and commit the *same* FB to both tiles' primary planes with
//!   per-tile `SRC_X` crops, in a single atomic request. Global driver validation (AMD DC
//!   clocks/bandwidth) is preserved because the request covers every head of the group at once.
//! * The two CRTCs are not genlocked. Identical native timings remove systematic skew and one
//!   atomic commit aligns enablement; hardware-level phase drift can remain. Completion events of
//!   the two CRTCs are aggregated into a single vblank/presentation feedback using the later tile's
//!   timestamp with the primary tile's stable sequence.
//! * `amdgpu_dm_force_timing_sync` is not used for phase alignment: it is card-global and can stall
//!   page flips on mixed-refresh configurations.
//! * Only side-by-side tiles (`num_h_tiles == 2`, `num_v_tiles == 1`) with identical timings are
//!   grouped. Anything suspicious (mismatching timings, missing topology, different EDID identity,
//!   different plane formats) falls back to two separate outputs.
//! * Explicit KMS fences (`IN_FENCE_FD`/`OUT_FENCE_PTR`) are not wired up. Composited frames
//!   therefore wait for renderer completion before the atomic commit; the aggregated member vblank
//!   event supplies presentation completion timing.
//! * Strict direct scanout is limited to one opaque, native-size, untransformed client dmabuf.
//!   Overlay and cursor planes remain composited. Composited frames use swapchain buffer ages for
//!   partial rendering.

use std::collections::{HashMap, HashSet};
use std::iter::zip;
use std::mem;
use std::num::NonZeroU64;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use bytemuck::cast_slice_mut;
use niri_config::{Config, OutputConfigBinding, OutputName};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBuffer};
use smithay::backend::allocator::{Fourcc, Slot, Swapchain};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::exporter::{ExportBuffer, ExportFramebuffer};
use smithay::backend::drm::gbm::GbmFramebuffer;
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEventMetadata, DrmEventTime, DrmNode, DrmSurface,
};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::utils::Buffer as RendererBuffer;
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::output::Mode as WlMode;
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::atomic::AtomicModeReq;
use smithay::reexports::drm::control::{
    connector, crtc, framebuffer, plane, property, AtomicCommitFlags, Device, Mode as DrmMode,
    ModeFlags, ResourceHandle,
};
use smithay::reexports::gbm::Modifier;
use tracing::{debug, error, warn};

use crate::group::CrtcSubmissionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(u64);

static OUTPUT_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

impl OutputId {
    pub fn next() -> Self {
        Self(OUTPUT_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Per-DRM-device state owned by the tiled-output engine.
///
/// Keeping these indexes together makes the niri backend an integration host rather than the
/// owner of tiled-output policy and lifecycle bookkeeping. The host retains its DRM device,
/// renderers, and ordinary single-CRTC surfaces; this state owns only the dual-CRTC extension.
#[derive(Default)]
pub struct TiledDeviceState {
    /// Live groups keyed by their primary (leftmost) CRTC.
    groups: HashMap<crtc::Handle, TiledGroup>,
    /// Maps either member CRTC to the group's primary CRTC.
    by_crtc: HashMap<crtc::Handle, crtc::Handle>,
    /// Stable output identities across a disconnect/reconnect.
    group_ids: HashMap<String, OutputId>,
    /// Stable Tracy names. Dynamic Tracy names are process-lifetime allocations, so cache them
    /// independently of group attempts and reuse them across reconnects and validation failures.
    trace_names: HashMap<String, TiledTraceNames>,
    /// Formation failures and their bounded retry policy.
    failures: TiledFailureTracker,
    /// Failed all-head disables, retried independently from formation.
    disable_failures: TiledFailureTracker,
    /// Groups whose all-head disable could not be confirmed and whose KMS resources must remain
    /// retained until a later retry or device removal.
    quarantined_groups: HashMap<crtc::Handle, TiledGroup>,
    /// Increments whenever the host successfully rescans connector topology.
    generation: u64,
}

#[derive(Clone, Copy)]
pub struct TiledTraceNames {
    pub vblank_frame: tracy_client::FrameName,
    pub time_since_presentation: tracy_client::PlotName,
    pub presentation_misprediction: tracy_client::PlotName,
    pub sequence_delta: tracy_client::PlotName,
}

impl TiledDeviceState {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
    }

    pub fn live_groups(&self) -> impl Iterator<Item = (&crtc::Handle, &TiledGroup)> {
        self.groups.iter()
    }

    pub fn live_groups_mut(&mut self) -> impl Iterator<Item = (&crtc::Handle, &mut TiledGroup)> {
        self.groups.iter_mut()
    }

    pub fn live_group(&self, primary: crtc::Handle) -> Option<&TiledGroup> {
        self.groups.get(&primary)
    }

    pub fn live_group_mut(&mut self, primary: crtc::Handle) -> Option<&mut TiledGroup> {
        self.groups.get_mut(&primary)
    }

    pub fn live_group_and_formation_failures_mut(
        &mut self,
        primary: crtc::Handle,
    ) -> Option<(&mut TiledGroup, &mut TiledFailureTracker)> {
        let group = self.groups.get_mut(&primary)?;
        Some((group, &mut self.failures))
    }

    pub fn contains_primary(&self, primary: crtc::Handle) -> bool {
        self.groups.contains_key(&primary)
    }

    pub fn primary_for_member(&self, crtc: crtc::Handle) -> Option<crtc::Handle> {
        self.by_crtc.get(&crtc).copied()
    }

    pub fn owns_crtc(&self, crtc: crtc::Handle) -> bool {
        self.by_crtc.contains_key(&crtc)
    }

    pub fn occupied_crtcs(&self) -> impl Iterator<Item = crtc::Handle> + '_ {
        self.by_crtc.keys().copied()
    }

    pub fn live_primaries(&self) -> impl Iterator<Item = crtc::Handle> + '_ {
        self.groups.keys().copied()
    }

    /// Publishes a group and both reverse mappings as one invariant-preserving operation.
    pub fn insert_live_group(&mut self, group: TiledGroup) {
        let primary = group.primary_crtc();
        let members = group.member_crtcs();
        assert!(
            members
                .iter()
                .all(|member| !self.by_crtc.contains_key(member)),
            "tiled member crtc must not already be owned"
        );
        assert!(
            !self.groups.contains_key(&primary),
            "tiled group must not have already existed"
        );
        for member in members {
            self.by_crtc.insert(member, primary);
        }
        self.groups.insert(primary, group);
    }

    /// Removes the group containing `crtc`, pruning all reverse mappings even if the primary
    /// map is already inconsistent.
    pub fn take_live_group_by_member(
        &mut self,
        crtc: crtc::Handle,
    ) -> Option<(crtc::Handle, TiledGroup)> {
        let primary = self.by_crtc.get(&crtc).copied()?;
        self.by_crtc.retain(|_, mapped| *mapped != primary);
        self.groups.remove(&primary).map(|group| (primary, group))
    }

    pub fn output_identity(&mut self, key: &str) -> (OutputId, TiledTraceNames) {
        let id = *self
            .group_ids
            .entry(key.to_owned())
            .or_insert_with(OutputId::next);
        let names = *self
            .trace_names
            .entry(key.to_owned())
            .or_insert_with(|| TiledTraceNames {
                vblank_frame: tracy_client::FrameName::new_leak(format!("vblank on {key}")),
                time_since_presentation: tracy_client::PlotName::new_leak(format!(
                    "{key} time since presentation, ms"
                )),
                presentation_misprediction: tracy_client::PlotName::new_leak(format!(
                    "{key} presentation misprediction, ms"
                )),
                sequence_delta: tracy_client::PlotName::new_leak(format!("{key} sequence delta")),
            });
        (id, names)
    }

    pub fn formation_failures(&self) -> &TiledFailureTracker {
        &self.failures
    }

    pub fn formation_failures_mut(&mut self) -> &mut TiledFailureTracker {
        &mut self.failures
    }

    pub fn disable_failures(&self) -> &TiledFailureTracker {
        &self.disable_failures
    }

    pub fn disable_failures_mut(&mut self) -> &mut TiledFailureTracker {
        &mut self.disable_failures
    }

    pub fn clear_failures(&mut self) {
        self.failures.clear();
        self.disable_failures.clear();
    }

    pub fn quarantined_groups(&self) -> impl Iterator<Item = (&crtc::Handle, &TiledGroup)> {
        self.quarantined_groups.iter()
    }

    pub fn take_quarantined_group(&mut self, primary: crtc::Handle) -> Option<TiledGroup> {
        self.quarantined_groups.remove(&primary)
    }

    pub fn quarantine_group(&mut self, primary: crtc::Handle, group: TiledGroup) {
        let old = self.quarantined_groups.insert(primary, group);
        assert!(old.is_none(), "tiled group must not already be quarantined");
    }

    pub fn crtc_is_quarantined(&self, crtc: crtc::Handle) -> bool {
        self.quarantined_groups
            .values()
            .any(|group| group.member_crtcs().contains(&crtc))
    }

    pub fn members_are_quarantined(&self, members: [crtc::Handle; 2]) -> bool {
        self.quarantined_groups.values().any(|group| {
            members
                .into_iter()
                .any(|member| group.member_crtcs().contains(&member))
        })
    }
}

/// Returns whether a modifier is known to consume disproportionate display bandwidth on Intel.
pub fn is_ccs_modifier(modifier: Modifier) -> bool {
    matches!(
        modifier,
        Modifier::I915_y_tiled_ccs
            // I915_FORMAT_MOD_Yf_TILED_CCS
            | Modifier::Unrecognized(0x100000000000005)
            | Modifier::I915_y_tiled_gen12_rc_ccs
            | Modifier::I915_y_tiled_gen12_mc_ccs
            // I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC
            | Modifier::Unrecognized(0x100000000000008)
            // I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
            | Modifier::Unrecognized(0x10000000000000a)
            // I915_FORMAT_MOD_4_TILED_DG2_MC_CCS
            | Modifier::Unrecognized(0x10000000000000b)
            // I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC
            | Modifier::Unrecognized(0x10000000000000c)
    )
}

/// Applies niri's common render-format policy for ordinary and tiled scanout.
pub fn filter_render_formats(formats: &FormatSet, display_only: bool) -> FormatSet {
    formats
        .iter()
        .copied()
        .filter(|format| {
            if display_only {
                format.modifier == Modifier::Linear
            } else {
                !is_ccs_modifier(format.modifier)
            }
        })
        .collect()
}

/// Intersects renderer formats with every tiled primary plane.
pub fn tiled_scanout_formats(
    render_formats: &FormatSet,
    member_plane_formats: &[&FormatSet],
) -> FormatSet {
    render_formats
        .iter()
        .copied()
        .filter(|format| {
            member_plane_formats
                .iter()
                .all(|formats| formats.contains(format))
        })
        .collect()
}

/// Chooses the preferred tiled scanout format and its shared modifiers.
pub fn choose_tiled_scanout_format(
    render_formats: &FormatSet,
    member_plane_formats: &[&FormatSet],
) -> Option<(Fourcc, Vec<Modifier>)> {
    let common = tiled_scanout_formats(render_formats, member_plane_formats);
    for fourcc in [Fourcc::Xrgb8888, Fourcc::Argb8888] {
        let modifiers = common
            .iter()
            .filter(|format| format.code == fourcc)
            .map(|format| format.modifier)
            .collect::<Vec<_>>();
        if !modifiers.is_empty() {
            return Some((fourcc, modifiers));
        }
    }

    None
}

pub fn find_drm_property(
    drm: &DrmDevice,
    resource: impl ResourceHandle,
    name: &str,
) -> Option<(property::Handle, property::Info, property::RawValue)> {
    let props = match drm.get_properties(resource) {
        Ok(props) => props,
        Err(err) => {
            warn!("error getting properties: {err:?}");
            return None;
        }
    };
    props.into_iter().find_map(|(handle, value)| {
        let info = drm.get_property(handle).ok()?;
        (info.name().to_str().ok()? == name).then_some((handle, info, value))
    })
}

fn get_drm_property(
    drm: &DrmDevice,
    resource: impl ResourceHandle,
    prop: property::Handle,
) -> Option<property::RawValue> {
    let props = match drm.get_properties(resource) {
        Ok(props) => props,
        Err(err) => {
            warn!("error getting properties: {err:?}");
            return None;
        }
    };
    props
        .into_iter()
        .find_map(|(handle, value)| (handle == prop).then_some(value))
}

pub struct GammaProps {
    crtc: crtc::Handle,
    gamma_lut: property::Handle,
    gamma_lut_size: property::Handle,
    previous_blob: Option<NonZeroU64>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct DrmColorLut {
    red: u16,
    green: u16,
    blue: u16,
    reserved: u16,
}

impl GammaProps {
    pub fn new(device: &DrmDevice, crtc: crtc::Handle) -> anyhow::Result<Self> {
        let mut gamma_lut = None;
        let mut gamma_lut_size = None;
        for (prop, _) in device
            .get_properties(crtc)
            .context("error getting properties")?
        {
            let Ok(info) = device.get_property(prop) else {
                continue;
            };
            let Ok(name) = info.name().to_str() else {
                continue;
            };
            match name {
                "GAMMA_LUT" => {
                    ensure!(
                        matches!(info.value_type(), property::ValueType::Blob),
                        "wrong GAMMA_LUT value type"
                    );
                    gamma_lut = Some(prop);
                }
                "GAMMA_LUT_SIZE" => {
                    ensure!(
                        matches!(info.value_type(), property::ValueType::UnsignedRange(_, _)),
                        "wrong GAMMA_LUT_SIZE value type"
                    );
                    gamma_lut_size = Some(prop);
                }
                _ => (),
            }
        }
        Ok(Self {
            crtc,
            gamma_lut: gamma_lut.context("missing GAMMA_LUT property")?,
            gamma_lut_size: gamma_lut_size.context("missing GAMMA_LUT_SIZE property")?,
            previous_blob: None,
        })
    }

    pub fn gamma_size(&self, device: &DrmDevice) -> anyhow::Result<u32> {
        Ok(get_drm_property(device, self.crtc, self.gamma_lut_size)
            .context("missing GAMMA_LUT_SIZE property")? as u32)
    }

    pub fn create_blob(
        &self,
        device: &DrmDevice,
        gamma: Option<&[u16]>,
    ) -> anyhow::Result<Option<NonZeroU64>> {
        let Some(gamma) = gamma else { return Ok(None) };
        let gamma_size = self
            .gamma_size(device)
            .context("error getting gamma size")? as usize;
        ensure!(gamma.len() == gamma_size * 3, "wrong gamma length");
        let (red, rest) = gamma.split_at(gamma_size);
        let (green, blue) = rest.split_at(gamma_size);
        let mut data = zip(zip(red, green), blue)
            .map(|((&red, &green), &blue)| DrmColorLut {
                red,
                green,
                blue,
                reserved: 0,
            })
            .collect::<Vec<_>>();
        let blob = drm_ffi::mode::create_property_blob(device.as_fd(), cast_slice_mut(&mut data))
            .context("error creating property blob")?;
        Ok(NonZeroU64::new(u64::from(blob.blob_id)))
    }

    pub fn add_blob_to_request(&self, request: &mut AtomicModeReq, blob: Option<NonZeroU64>) {
        request.add_property(
            self.crtc,
            self.gamma_lut,
            property::Value::Blob(blob.map(NonZeroU64::get).unwrap_or(0)),
        );
    }

    pub fn current_blob_property(&self) -> (property::Handle, u64) {
        (
            self.gamma_lut,
            self.previous_blob.map(NonZeroU64::get).unwrap_or(0),
        )
    }

    pub fn destroy_blob(device: &DrmDevice, blob: Option<NonZeroU64>) {
        if let Some(blob) = blob {
            if let Err(err) = device.destroy_property_blob(blob.get()) {
                warn!("error destroying GAMMA_LUT property blob: {err:?}");
            }
        }
    }

    pub fn install_blob(&mut self, device: &DrmDevice, blob: Option<NonZeroU64>) {
        Self::destroy_blob(device, mem::replace(&mut self.previous_blob, blob));
    }

    pub fn previous_blob(&self) -> Option<NonZeroU64> {
        self.previous_blob
    }

    pub fn set_gamma(&mut self, device: &DrmDevice, gamma: Option<&[u16]>) -> anyhow::Result<()> {
        let blob = self.create_blob(device, gamma)?;
        let result = device
            .set_property(
                self.crtc,
                self.gamma_lut,
                property::Value::Blob(blob.map(NonZeroU64::get).unwrap_or(0)).into(),
            )
            .context("error setting GAMMA_LUT");
        if let Err(err) = result {
            Self::destroy_blob(device, blob);
            return Err(err);
        }
        self.install_blob(device, blob);
        Ok(())
    }

    pub fn restore_gamma(&self, device: &DrmDevice) -> anyhow::Result<()> {
        device
            .set_property(
                self.crtc,
                self.gamma_lut,
                property::Value::Blob(self.previous_blob.map(NonZeroU64::get).unwrap_or(0)).into(),
            )
            .context("error setting GAMMA_LUT")?;
        Ok(())
    }

    pub fn destroy(&mut self, device: &DrmDevice) {
        Self::destroy_blob(device, self.previous_blob.take());
    }
}

fn tiled_gamma_props(group: &TiledGroup) -> anyhow::Result<[&GammaProps; 2]> {
    let [left, right] = &group.members;
    Ok([
        left.gamma_props
            .as_ref()
            .context("left tiled member lacks atomic GAMMA_LUT support")?,
        right
            .gamma_props
            .as_ref()
            .context("right tiled member lacks atomic GAMMA_LUT support")?,
    ])
}

pub fn matching_tiled_gamma_size(sizes: [u32; 2]) -> anyhow::Result<u32> {
    ensure!(
        sizes[0] == sizes[1],
        "tiled group members have incompatible gamma sizes: {} and {}",
        sizes[0],
        sizes[1],
    );
    Ok(sizes[0])
}

pub fn tiled_gamma_size(device: &DrmDevice, group: &TiledGroup) -> anyhow::Result<u32> {
    let [left, right] = tiled_gamma_props(group)?;
    matching_tiled_gamma_size([left.gamma_size(device)?, right.gamma_size(device)?])
}

pub fn set_tiled_gamma(
    device: &DrmDevice,
    group: &mut TiledGroup,
    ramp: Option<&[u16]>,
) -> anyhow::Result<()> {
    let [left_member, right_member] = &mut group.members;
    let left = left_member
        .gamma_props
        .as_mut()
        .context("left tiled member lacks atomic GAMMA_LUT support")?;
    let right = right_member
        .gamma_props
        .as_mut()
        .context("right tiled member lacks atomic GAMMA_LUT support")?;

    // Preflight and allocate both blobs before changing either CRTC.
    let gamma_size =
        matching_tiled_gamma_size([left.gamma_size(device)?, right.gamma_size(device)?])? as usize;
    if let Some(ramp) = ramp {
        ensure!(gamma_size != 0, "setting gamma is not supported");
        ensure!(ramp.len() == gamma_size * 3, "wrong gamma length");
    }

    let left_blob = left.create_blob(device, ramp)?;
    let right_blob = match right.create_blob(device, ramp) {
        Ok(blob) => blob,
        Err(err) => {
            GammaProps::destroy_blob(device, left_blob);
            return Err(err);
        }
    };

    let mut request = AtomicModeReq::new();
    left.add_blob_to_request(&mut request, left_blob);
    right.add_blob_to_request(&mut request, right_blob);
    if let Err(err) = device.atomic_commit(AtomicCommitFlags::empty(), request) {
        GammaProps::destroy_blob(device, left_blob);
        GammaProps::destroy_blob(device, right_blob);
        return Err(err).context("error atomically setting tiled GAMMA_LUT properties");
    }

    left.install_blob(device, left_blob);
    right.install_blob(device, right_blob);
    Ok(())
}

pub fn restore_tiled_gamma(device: &DrmDevice, group: &TiledGroup) -> anyhow::Result<()> {
    let [left, right] = tiled_gamma_props(group)?;
    let mut request = AtomicModeReq::new();
    left.add_blob_to_request(&mut request, left.previous_blob());
    right.add_blob_to_request(&mut request, right.previous_blob());
    device
        .atomic_commit(AtomicCommitFlags::empty(), request)
        .context("error atomically restoring tiled GAMMA_LUT properties")
}

pub fn destroy_tiled_gamma_blobs(device: &DrmDevice, group: &mut TiledGroup) {
    for member in &mut group.members {
        if let Some(gamma_props) = &mut member.gamma_props {
            gamma_props.destroy(device);
        }
    }
}

const TILED_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(5),
];

/// Two buffers are sufficient in steady state; a fourth Smithay slot indicates unusual
/// retention or completion latency worth surfacing in logs.
const EXPECTED_TILED_SWAPCHAIN_SLOTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiledFailureClass {
    UntilEnvironmentChange,
    Retryable,
    UntilSessionResume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiledFailurePolicy {
    UntilEnvironmentChange,
    Retryable { attempt: u8, retry_at: Instant },
    UntilSessionResume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TiledFailure {
    pub generation: u64,
    pub policy: TiledFailurePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TiledRetry {
    pub generation: u64,
    pub attempt: u8,
    pub delay: Duration,
}

#[derive(Debug, Default)]
pub struct TiledFailureTracker {
    entries: HashMap<String, TiledFailure>,
    next_generation: u64,
}

impl TiledFailureTracker {
    pub fn blocks(&self, key: &str, now: Instant) -> bool {
        self.entries
            .get(key)
            .is_some_and(|failure| match failure.policy {
                TiledFailurePolicy::Retryable { retry_at, .. } => now < retry_at,
                TiledFailurePolicy::UntilEnvironmentChange
                | TiledFailurePolicy::UntilSessionResume => true,
            })
    }

    pub fn record(
        &mut self,
        key: String,
        class: TiledFailureClass,
        now: Instant,
    ) -> Option<TiledRetry> {
        let policy = match class {
            TiledFailureClass::UntilEnvironmentChange => TiledFailurePolicy::UntilEnvironmentChange,
            TiledFailureClass::UntilSessionResume => TiledFailurePolicy::UntilSessionResume,
            TiledFailureClass::Retryable => {
                let attempt = match self.entries.get(&key).map(|failure| failure.policy) {
                    Some(TiledFailurePolicy::Retryable { attempt, .. }) => {
                        attempt.saturating_add(1)
                    }
                    _ => 0,
                };
                let Some(&delay) = TILED_RETRY_DELAYS.get(usize::from(attempt)) else {
                    return self.record(key, TiledFailureClass::UntilEnvironmentChange, now);
                };
                TiledFailurePolicy::Retryable {
                    attempt,
                    retry_at: now + delay,
                }
            }
        };

        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.entries
            .insert(key, TiledFailure { generation, policy });

        match policy {
            TiledFailurePolicy::Retryable { attempt, retry_at } => Some(TiledRetry {
                generation,
                attempt,
                delay: retry_at.duration_since(now),
            }),
            TiledFailurePolicy::UntilEnvironmentChange | TiledFailurePolicy::UntilSessionResume => {
                None
            }
        }
    }

    pub fn retry_due(&self, key: &str, generation: u64, now: Instant) -> bool {
        self.entries.get(key).is_some_and(|failure| {
            failure.generation == generation
                && matches!(
                    failure.policy,
                    TiledFailurePolicy::Retryable { retry_at, .. } if now >= retry_at
                )
        })
    }

    pub fn policy(&self, key: &str) -> Option<TiledFailurePolicy> {
        self.entries.get(key).map(|failure| failure.policy)
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Parsed kernel `TILE` connector property blob.
///
/// The kernel encodes tiled-display topology (from the DisplayID Tiled Display Topology data
/// block) as a string: `group_id:single_monitor:num_h_tiles:num_v_tiles:tile_h_loc:tile_v_loc:
/// tile_w:tile_h`. For example, the left tile of an LG UltraFine 5K reads
/// `"1:1:2:1:0:0:2560:2880"` and the right tile `"1:1:2:1:1:0:2560:2880"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileInfo {
    pub group_id: u32,
    pub single_monitor: bool,
    pub num_h_tiles: u32,
    pub num_v_tiles: u32,
    pub tile_h_loc: u32,
    pub tile_v_loc: u32,
    pub tile_w: u32,
    pub tile_h: u32,
}

impl TileInfo {
    /// Parses the kernel TILE blob, validating positions and using checked arithmetic (sink
    /// data is not trustworthy).
    pub fn parse(blob: &[u8]) -> Option<TileInfo> {
        let s = std::str::from_utf8(blob).ok()?;
        let mut fields = s.trim_end_matches('\0').split(':');
        let mut next = || -> Option<u32> { fields.next()?.parse().ok() };
        let info = TileInfo {
            group_id: next()?,
            single_monitor: next()? != 0,
            num_h_tiles: next()?,
            num_v_tiles: next()?,
            tile_h_loc: next()?,
            tile_v_loc: next()?,
            tile_w: next()?,
            tile_h: next()?,
        };
        // Exactly 8 fields.
        if fields.next().is_some() {
            return None;
        }
        if info.num_h_tiles == 0
            || info.num_v_tiles == 0
            || info.tile_h_loc >= info.num_h_tiles
            || info.tile_v_loc >= info.num_v_tiles
            || info.tile_w == 0
            || info.tile_h == 0
            || info.tile_w > u32::from(u16::MAX)
            || info.tile_h > u32::from(u16::MAX)
        {
            return None;
        }
        info.x_off()?;
        info.y_off()?;
        Some(info)
    }

    /// Horizontal offset of this tile within the full image, in pixels.
    pub fn x_off(&self) -> Option<u32> {
        self.tile_h_loc.checked_mul(self.tile_w)
    }

    /// Vertical offset of this tile within the full image, in pixels.
    pub fn y_off(&self) -> Option<u32> {
        self.tile_v_loc.checked_mul(self.tile_h)
    }
}

/// Reads and parses the `TILE` property of a connector, if the kernel exposes one.
pub fn read_tile_info(device: &DrmDevice, conn: connector::Handle) -> Option<TileInfo> {
    let (_, info, value) = find_drm_property(device, conn, "TILE")?;
    let blob = info.value_type().convert_value(value).as_blob()?;
    let data = device.get_property_blob(blob).ok()?;
    TileInfo::parse(&data)
}

/// Byte-for-byte timing identity of a DRM mode, excluding name and type.
///
/// Two tiles may only be stitched when they run byte-identical timings, preventing systematic
/// drift while both halves scan out the same submitted frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingFingerprint {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub hskew: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
}

impl From<DrmMode> for TimingFingerprint {
    fn from(mode: DrmMode) -> Self {
        let (hdisplay, hsync_start, hsync_end, htotal) = {
            let (w, _) = mode.size();
            let (start, end, total) = mode.hsync();
            (w, start, end, total)
        };
        let (vdisplay, vsync_start, vsync_end, vtotal) = {
            let (_, h) = mode.size();
            let (start, end, total) = mode.vsync();
            (h, start, end, total)
        };
        TimingFingerprint {
            clock: mode.clock(),
            hdisplay,
            hsync_start,
            hsync_end,
            htotal,
            vdisplay,
            vsync_start,
            vsync_end,
            vtotal,
            hskew: mode.hskew(),
            vscan: mode.vscan(),
            vrefresh: mode.vrefresh(),
            flags: mode.flags().bits(),
        }
    }
}

/// A connected connector that might become a tiled-group member.
///
/// Built by the TTY backend from live DRM state; grouping decisions are made by
/// [`validate_tile_pair`].
pub struct TileCandidate {
    /// Normalized EDID identity (`make model serial`).
    pub identity: String,
    /// Kernel TILE topology, if exposed.
    pub tile: Option<TileInfo>,
    /// Connector mode list, in kernel order (preferred first).
    pub modes: Vec<DrmMode>,
    /// Primary-plane pixel formats of the candidate's CRTC.
    pub plane_fourccs: Vec<Fourcc>,
}

/// Live desktop connector data supplied by the niri backend adapter.
pub struct ConnectedTile {
    pub connector: connector::Info,
    pub crtc: crtc::Handle,
    pub name: OutputName,
}

/// The desired tiled topology for one DRM device.
pub struct TiledPlanningResult {
    pub plans: Vec<TiledGroupPlan>,
    pub consumed_crtcs: HashSet<crtc::Handle>,
    pub force_disconnect: Vec<crtc::Handle>,
}

/// Builds a grouping candidate from live DRM state.
pub fn tiled_candidate(
    drm: &DrmDevice,
    connector: &connector::Info,
    crtc: crtc::Handle,
    name: &OutputName,
) -> Option<TileCandidate> {
    let planes = match drm.planes(&crtc) {
        Ok(planes) => planes,
        Err(err) => {
            warn!(
                "tiled-groups: error getting planes for {}: {err:?}",
                name.connector,
            );
            return None;
        }
    };
    let mut plane_fourccs = Vec::new();
    for info in &planes.primary {
        for format in info.formats.iter() {
            if !plane_fourccs.contains(&format.code) {
                plane_fourccs.push(format.code);
            }
        }
    }

    Some(TileCandidate {
        identity: name.format_make_model_serial(),
        tile: read_tile_info(drm, connector.handle()),
        modes: connector.modes().to_vec(),
        plane_fourccs,
    })
}

/// Returns the synthesized logical output name for two member connectors.
pub fn tiled_output_name(
    left: &OutputName,
    right: &OutputName,
    disable_monitor_names: bool,
) -> OutputName {
    // The connector name must not depend on which physical link ended up left; DP-4/DP-5 can
    // swap roles across replugs. EDID identity still comes from the physical left tile.
    let mut connector_names = [left.connector.clone(), right.connector.clone()];
    connector_names.sort();
    let (make, model, serial) = if disable_monitor_names {
        (None, None, None)
    } else {
        (left.make.clone(), left.model.clone(), left.serial.clone())
    };

    OutputName {
        connector: connector_names.join("+"),
        make,
        model,
        serial,
    }
}

/// Resolves configured connector names against a connected-connector list.
pub fn resolve_tiled_members<'a>(
    configured: &'a [String],
    connected_names: &[&str],
) -> (Vec<usize>, Vec<&'a str>) {
    let mut members = Vec::new();
    let mut missing = Vec::new();

    for name in configured {
        match connected_names
            .iter()
            .position(|connected| connected.eq_ignore_ascii_case(name))
        {
            Some(idx) => members.push(idx),
            None => missing.push(name.as_str()),
        }
    }

    (members, missing)
}

fn resolve_and_claim_explicit_members<'a>(
    configured: &'a [String],
    connected_names: &[&str],
    claimed: &mut HashSet<usize>,
) -> (Vec<usize>, Vec<&'a str>) {
    let (members, missing) = resolve_tiled_members(configured, connected_names);
    if !members.is_empty() {
        claimed.extend(members.iter().copied());
    }
    (members, missing)
}

/// Why two candidates cannot be stitched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRejection {
    /// TILE topology is required (auto-detection) but missing on a connector.
    MissingTopology,
    /// The topology does not describe a single monitor.
    NotSingleMonitor,
    /// The topology is not a horizontal 2x1 grid.
    NotHorizontalPair,
    /// The tiles belong to different topology groups.
    GroupIdMismatch,
    /// The tiles disagree on tile dimensions.
    TileSizeMismatch,
    /// Both tiles claim the same position in the grid.
    DuplicatePosition,
    /// The tiles have different EDID identities (different physical panels).
    IdentityMismatch,
    /// No mode with byte-identical timings exists on both tiles.
    NoCommonMode,
    /// The tiles' potentially claimable primary planes have no pixel format in common.
    PlaneFormatMismatch,
}

impl std::fmt::Display for PairRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            PairRejection::MissingTopology => "missing TILE topology",
            PairRejection::NotSingleMonitor => "not a single-monitor topology",
            PairRejection::NotHorizontalPair => "not a horizontal 2x1 tile grid",
            PairRejection::GroupIdMismatch => "different TILE group ids",
            PairRejection::TileSizeMismatch => "different tile sizes",
            PairRejection::DuplicatePosition => "duplicate tile position",
            PairRejection::IdentityMismatch => "different EDID identities",
            PairRejection::NoCommonMode => "no mode with identical timings",
            PairRejection::PlaneFormatMismatch => "no common primary plane format",
        };
        f.write_str(msg)
    }
}

/// Result of a successful [`validate_tile_pair`].
#[derive(Debug, PartialEq)]
pub struct ValidatedPair {
    /// The common native tile mode to drive both CRTCs with.
    pub tile_mode: DrmMode,
    /// Horizontal offset of candidate `a` within the full image, in pixels.
    pub a_x: u32,
    /// Horizontal offset of candidate `b` within the full image, in pixels.
    pub b_x: u32,
    /// Tile width in pixels.
    pub tile_w: u32,
    /// Tile height in pixels.
    pub tile_h: u32,
}

/// Finds the first mode in `modes` (filtered to `size` if given) whose timings are
/// byte-identical to some mode in `other` (same filter).
fn common_mode(modes: &[DrmMode], other: &[DrmMode], size: Option<(u32, u32)>) -> Option<DrmMode> {
    let size = match size {
        Some((w, h)) => Some((w.try_into().ok()?, h.try_into().ok()?)),
        None => None,
    };
    for m in modes {
        if m.flags().contains(ModeFlags::INTERLACE) {
            continue;
        }
        if let Some(size) = size {
            if m.size() != size {
                continue;
            }
        }
        let fingerprint = TimingFingerprint::from(*m);
        let matches = other.iter().any(|n| {
            !n.flags().contains(ModeFlags::INTERLACE)
                && size.is_none_or(|size| n.size() == size)
                && TimingFingerprint::from(*n) == fingerprint
        });
        if matches {
            return Some(*m);
        }
    }
    None
}

fn have_common_fourcc(a: &[Fourcc], b: &[Fourcc]) -> bool {
    a.iter().any(|format| b.contains(format))
}

/// Checks that two candidates form a valid tile pair and returns the mode and geometry to
/// drive them with.
///
/// With `require_topology` (auto-detection), both candidates must expose kernel TILE
/// topology describing one horizontal 2x1 monitor, share an EDID identity, and have
/// byte-identical timings. Without it (explicit `tiled-group` config), identity and topology
/// presence are not required; if both tiles still expose topology, it is validated and its
/// positions are authoritative, otherwise a side-by-side layout is synthesized with the
/// first candidate as the left tile and the tile size taken from the common mode.
pub fn validate_tile_pair(
    a: &TileCandidate,
    b: &TileCandidate,
    require_topology: bool,
) -> Result<ValidatedPair, PairRejection> {
    if require_topology {
        // Auto-detection must never cross-pair two physical panels.
        if a.identity != b.identity {
            return Err(PairRejection::IdentityMismatch);
        }
        if a.tile.is_none() || b.tile.is_none() {
            return Err(PairRejection::MissingTopology);
        }
    }

    // Whenever both tiles expose topology (auto-detection, or explicit config on tiled
    // hardware), it must describe one horizontal 2x1 monitor, and the live positions are
    // authoritative over the config order.
    if let (Some(ta), Some(tb)) = (a.tile, b.tile) {
        if !ta.single_monitor || !tb.single_monitor {
            return Err(PairRejection::NotSingleMonitor);
        }
        if ta.num_h_tiles != 2 || ta.num_v_tiles != 1 || tb.num_h_tiles != 2 || tb.num_v_tiles != 1
        {
            return Err(PairRejection::NotHorizontalPair);
        }
        if ta.group_id != tb.group_id {
            return Err(PairRejection::GroupIdMismatch);
        }
        if ta.tile_w != tb.tile_w || ta.tile_h != tb.tile_h {
            return Err(PairRejection::TileSizeMismatch);
        }
        if (ta.tile_h_loc, ta.tile_v_loc) == (tb.tile_h_loc, tb.tile_v_loc) {
            return Err(PairRejection::DuplicatePosition);
        }
        // Vertical stacking is not supported; both tiles must be in row 0.
        if ta.tile_v_loc != 0 || tb.tile_v_loc != 0 {
            return Err(PairRejection::NotHorizontalPair);
        }
    }

    if !have_common_fourcc(&a.plane_fourccs, &b.plane_fourccs) {
        return Err(PairRejection::PlaneFormatMismatch);
    }

    // A one-sided TILE property is still useful for selecting the native mode. Without
    // this constraint an explicit group could select an earlier, lower-resolution common
    // mode merely because the other connector did not expose topology.
    let size = a.tile.or(b.tile).map(|t| (t.tile_w, t.tile_h));
    let Some(tile_mode) = common_mode(&a.modes, &b.modes, size) else {
        return Err(PairRejection::NoCommonMode);
    };

    let (a_x, b_x, tile_w, tile_h) = match (a.tile, b.tile) {
        (Some(ta), Some(tb)) => {
            let (Some(a_x), Some(b_x)) = (ta.x_off(), tb.x_off()) else {
                return Err(PairRejection::MissingTopology);
            };
            (a_x, b_x, ta.tile_w, ta.tile_h)
        }
        // Synthesize a side-by-side layout. The first connector listed is the left tile,
        // unless exactly one tile exposes topology placing it on the right.
        (ta, tb) => {
            let (w, h) = tile_mode.size();
            let (w, h) = (u32::from(w), u32::from(h));
            let a_right =
                ta.is_some_and(|t| t.tile_h_loc == 1) || tb.is_some_and(|t| t.tile_h_loc == 0);
            if a_right {
                (w, 0, w, h)
            } else {
                (0, w, w, h)
            }
        }
    };

    Ok(ValidatedPair {
        tile_mode,
        a_x,
        b_x,
        tile_w,
        tile_h,
    })
}

/// One member of a planned tiled group.
pub struct TiledMemberPlan {
    pub connector: connector::Info,
    pub crtc: crtc::Handle,
    /// Horizontal offset of this tile within the full image, in pixels.
    pub crtc_x: u32,
    pub tile_w: u32,
    pub tile_h: u32,
}

/// A fully validated plan to stitch two connectors into one logical output.
pub struct TiledGroupPlan {
    /// Ordered by `crtc_x`; `members[0]` is the left (primary) tile.
    pub members: [TiledMemberPlan; 2],
    /// Group output name; connector is the member names joined with `+` (e.g. `DP-4+DP-5`),
    /// make/model/serial come from the left tile's EDID unless monitor names are disabled.
    pub name: OutputName,
    /// Exact output declaration selected for this group, when one exists.
    pub config: Option<OutputConfigBinding>,
    /// The common native tile mode each member CRTC is driven with.
    pub tile_mode: DrmMode,
}

impl TiledGroupPlan {
    pub fn primary_crtc(&self) -> crtc::Handle {
        self.members[0].crtc
    }

    pub fn member_crtcs(&self) -> [crtc::Handle; 2] {
        [self.members[0].crtc, self.members[1].crtc]
    }

    /// Stable key for caching the group's [`OutputId`] across hotplug.
    pub fn key(&self) -> &str {
        &self.name.connector
    }

    /// The synthesized full-size mode advertised on the logical output (e.g. 5120x2880@60).
    ///
    /// This mode is a pure container for clients; each tile CRTC is driven with its own
    /// native half-mode, so no pixel clock needs to be derived here.
    pub fn wl_mode(&self) -> WlMode {
        let tile = WlMode::from(self.tile_mode);
        let width = self.members.iter().map(|m| m.tile_w).sum::<u32>() as i32;
        let height = self.members[0].tile_h as i32;
        WlMode {
            size: (width, height).into(),
            refresh: tile.refresh,
        }
    }
}

/// Validates two live connectors and assembles a left-to-right tiled-group plan.
pub fn tiled_claim_pair(
    drm: &DrmDevice,
    a: &ConnectedTile,
    b: &ConnectedTile,
    require_topology: bool,
    disable_monitor_names: bool,
) -> Option<TiledGroupPlan> {
    let (Some(ca), Some(cb)) = (
        tiled_candidate(drm, &a.connector, a.crtc, &a.name),
        tiled_candidate(drm, &b.connector, b.crtc, &b.name),
    ) else {
        return None;
    };

    match validate_tile_pair(&ca, &cb, require_topology) {
        Ok(pair) => {
            let (left, right) = if pair.a_x <= pair.b_x { (a, b) } else { (b, a) };
            let mut members = [
                TiledMemberPlan {
                    connector: left.connector.clone(),
                    crtc: left.crtc,
                    crtc_x: pair.a_x.min(pair.b_x),
                    tile_w: pair.tile_w,
                    tile_h: pair.tile_h,
                },
                TiledMemberPlan {
                    connector: right.connector.clone(),
                    crtc: right.crtc,
                    crtc_x: pair.a_x.max(pair.b_x),
                    tile_w: pair.tile_w,
                    tile_h: pair.tile_h,
                },
            ];
            members.sort_by_key(|member| member.crtc_x);

            Some(TiledGroupPlan {
                members,
                name: tiled_output_name(&left.name, &right.name, disable_monitor_names),
                config: None,
                tile_mode: pair.tile_mode,
            })
        }
        Err(rejection) => {
            warn!(
                "tiled-groups: cannot stitch {} and {}: {rejection}",
                a.name.connector, b.name.connector,
            );
            None
        }
    }
}

/// Plans the tiled groups for one DRM device from host-supplied live connectors.
///
/// The host is responsible only for discovering desktop connectors and reporting which CRTCs
/// are currently occupied. Explicit declarations are authoritative over auto-detection.
pub fn plan_tiled_groups(
    drm: &DrmDevice,
    node: DrmNode,
    config: &Config,
    connected: &[ConnectedTile],
    occupied_crtcs: &HashSet<crtc::Handle>,
    auto: bool,
) -> TiledPlanningResult {
    let mut plans = Vec::new();
    let mut consumed_crtcs = HashSet::new();
    let mut force_disconnect = Vec::new();
    let mut claimed = HashSet::new();
    let disable_monitor_names = config.debug.disable_monitor_names;
    let connected_names = connected
        .iter()
        .map(|candidate| candidate.name.connector.as_str())
        .collect::<Vec<_>>();

    for out_cfg in &config.outputs.0 {
        let Some(tiled_group) = &out_cfg.tiled_group else {
            continue;
        };
        let (members, missing) =
            resolve_and_claim_explicit_members(&tiled_group.0, &connected_names, &mut claimed);

        // Output config is global, while planning runs once per DRM device.
        if members.is_empty() {
            continue;
        }

        // Every explicit declaration is authoritative over auto-detection, even while one of
        // its members is absent. Otherwise the present member could be paired with a different
        // auto-detected tile and contradict the user's configuration.
        if explicit_group_suppresses_standalone(out_cfg.off, missing.len()) {
            // Off and incomplete declarations own every present member. This prevents a
            // half-connected explicit panel from appearing standalone while its other link is
            // absent, and forces down a member that was published before the declaration became
            // incomplete during hotplug.
            for &idx in &members {
                let crtc = connected[idx].crtc;
                consumed_crtcs.insert(crtc);
                if occupied_crtcs.contains(&crtc) {
                    force_disconnect.push(crtc);
                }
            }
        }

        if !missing.is_empty() {
            error!(
                "tiled-groups: tiled-group on output {:?}: connectors {missing:?} are not \
                 connected to DRM device {node}; tiled groups require all members on the \
                 same device",
                out_cfg.name,
            );
            continue;
        }
        if members.len() != 2 {
            error!(
                "tiled-groups: tiled-group on output {:?}: exactly two member connectors \
                 are supported",
                out_cfg.name,
            );
            continue;
        }
        if out_cfg.off {
            continue;
        }

        let a = &connected[members[0]];
        let b = &connected[members[1]];
        if let Some(mut plan) = tiled_claim_pair(drm, a, b, false, disable_monitor_names) {
            plan.config = Some(OutputConfigBinding::from(out_cfg));
            plans.push(plan);
        } else {
            error!(
                "tiled-groups: tiled-group on output {:?} is invalid; connecting members \
                 separately",
                out_cfg.name,
            );
        }
    }

    if auto {
        let mut by_group: HashMap<(u32, String), Vec<usize>> = HashMap::new();
        for (idx, candidate) in connected.iter().enumerate() {
            if claimed.contains(&idx)
                || config
                    .outputs
                    .find(&candidate.name)
                    .is_some_and(|cfg| cfg.off)
            {
                continue;
            }
            let Some(tile) = read_tile_info(drm, candidate.connector.handle()) else {
                continue;
            };
            if candidate.name.make.is_none()
                && candidate.name.model.is_none()
                && candidate.name.serial.is_none()
            {
                debug!(
                    "tiled-groups: {} has TILE topology but no EDID identity; not grouping",
                    candidate.name.connector,
                );
                continue;
            }
            by_group
                .entry((tile.group_id, candidate.name.format_make_model_serial()))
                .or_default()
                .push(idx);
        }

        for ((_group_id, identity), indices) in by_group {
            if indices.len() < 2 {
                debug!(
                    "tiled-groups: only {}/2 tiles connected for {identity}",
                    indices.len(),
                );
                continue;
            }
            if indices.len() > 2 {
                warn!(
                    "tiled-groups: {} tiles connected for {identity}; only 2x1 groups are \
                     supported",
                    indices.len(),
                );
                continue;
            }
            let a = &connected[indices[0]];
            let b = &connected[indices[1]];
            if let Some(mut plan) = tiled_claim_pair(drm, a, b, true, disable_monitor_names) {
                plan.config = config
                    .outputs
                    .find(&plan.name)
                    .map(OutputConfigBinding::from);
                if config
                    .outputs
                    .find_bound(&plan.name, plan.config.as_ref())
                    .is_some_and(|cfg| cfg.off)
                {
                    for member in &plan.members {
                        consumed_crtcs.insert(member.crtc);
                        if occupied_crtcs.contains(&member.crtc) {
                            force_disconnect.push(member.crtc);
                        }
                    }
                    continue;
                }
                plans.push(plan);
            }
        }
    }

    TiledPlanningResult {
        plans,
        consumed_crtcs,
        force_disconnect,
    }
}

fn explicit_group_suppresses_standalone(off: bool, missing_members: usize) -> bool {
    off || missing_members != 0
}

/// A framebuffer retained for as long as either tiled CRTC may still scan it out.
pub enum TiledScanoutBuffer {
    Composited(Slot<GbmBuffer>),
    Direct {
        /// Retains the Wayland client buffer and its release point.
        buffer: RendererBuffer,
        /// Retains the DRM framebuffer until both tiles have replaced it.
        framebuffer: GbmFramebuffer,
    },
}

impl TiledScanoutBuffer {
    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct { .. })
    }
}

/// A framebuffer that has been committed but not yet presented.
pub struct PendingTiledFrame {
    pub buffer: TiledScanoutBuffer,
    pub feedback: OutputPresentationFeedback,
    pub target_presentation_time: Duration,
    pub submissions: [CrtcSubmissionId; 2],
}

pub struct TiledPresentation {
    pub feedback: OutputPresentationFeedback,
    pub target_presentation_time: Duration,
}

#[derive(Debug, Default)]
enum TiledFlipState<F> {
    #[default]
    Idle,
    AwaitingBoth(F),
    AwaitingSecond {
        frame: F,
        first_member: usize,
        first_event: DrmEventMetadata,
    },
}

impl<F> TiledFlipState<F> {
    fn has_pending_frame(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    fn frame_committed(&mut self, frame: F) {
        assert!(
            matches!(self, Self::Idle),
            "cannot commit a second tiled frame while one is pending"
        );
        *self = Self::AwaitingBoth(frame);
    }

    fn pending(&self) -> Option<&F> {
        match self {
            Self::Idle => None,
            Self::AwaitingBoth(frame) | Self::AwaitingSecond { frame, .. } => Some(frame),
        }
    }

    fn record_member_event(
        &mut self,
        member: usize,
        meta: DrmEventMetadata,
    ) -> Option<(F, DrmEventMetadata)> {
        match std::mem::take(self) {
            Self::Idle => None,
            Self::AwaitingBoth(frame) => {
                *self = Self::AwaitingSecond {
                    frame,
                    first_member: member,
                    first_event: meta,
                };
                None
            }
            state @ Self::AwaitingSecond { first_member, .. } if first_member == member => {
                // Keep the first event. A duplicate from one member must not complete a frame.
                *self = state;
                None
            }
            Self::AwaitingSecond {
                frame,
                first_member,
                first_event,
            } => {
                let meta = if first_member == 0 {
                    aggregate_member_events(first_event, meta)
                } else {
                    aggregate_member_events(meta, first_event)
                };
                Some((frame, meta))
            }
        }
    }

    fn reset(&mut self) -> bool {
        !matches!(std::mem::take(self), Self::Idle)
    }
}

/// Marker stored in slot userdata to count lazy Smithay swapchain allocations.
struct TiledSlotAllocation;

/// Cached DRM property handles for one member, resolved once at group creation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberProps {
    conn_crtc_id: property::Handle,
    crtc_mode_id: property::Handle,
    crtc_active: property::Handle,
    plane_fb_id: property::Handle,
    plane_crtc_id: property::Handle,
    plane_src_x: property::Handle,
    plane_src_y: property::Handle,
    plane_src_w: property::Handle,
    plane_src_h: property::Handle,
    plane_crtc_x: property::Handle,
    plane_crtc_y: property::Handle,
    plane_crtc_w: property::Handle,
    plane_crtc_h: property::Handle,
    /// Neutral-state properties (handle, raw value), applied on modeset commits.
    connector_neutral: Vec<(property::Handle, u64)>,
    crtc_neutral: Vec<(property::Handle, u64)>,
    plane_neutral: Vec<(property::Handle, u64)>,
}

/// Looks up an enum property value by name, e.g. `("Broadcast RGB", "Full")`.
///
/// Only touches the property if both it and the requested enum member exist.
fn enum_prop_value(
    device: &DrmDevice,
    resource: impl ResourceHandle,
    prop_name: &str,
    value_name: &str,
) -> Option<(property::Handle, u64)> {
    let (handle, info, _current) = find_drm_property(device, resource, prop_name)?;
    let property::ValueType::Enum(values) = info.value_type() else {
        return None;
    };
    let (_raw, values) = values.values();
    let value = values
        .iter()
        .find(|v| v.name().to_str().ok() == Some(value_name))?;
    Some((handle, value.value()))
}

/// Looks up a property handle by name.
fn prop_handle(
    device: &DrmDevice,
    resource: impl ResourceHandle,
    name: &str,
) -> Option<property::Handle> {
    find_drm_property(device, resource, name).map(|(handle, _, _)| handle)
}

/// Neutral connector state, from the sstitch atomic contract: HDR metadata cleared, default
/// colorspace, full-range RGB, no scaling/underscan, HDCP undesired.
///
/// Everything is optional; only properties the driver actually exposes are touched, and enum
/// values are resolved by name (raw zero is never assumed to mean neutral).
fn connector_neutral_props(
    device: &DrmDevice,
    conn: connector::Handle,
) -> Vec<(property::Handle, u64)> {
    let mut out = Vec::new();

    if let Some(handle) = prop_handle(device, conn, "HDR_OUTPUT_METADATA") {
        out.push((handle, 0));
    }

    for (prop, value) in [
        ("Colorspace", "Default"),
        ("Broadcast RGB", "Full"),
        ("Content Protection", "Undesired"),
        ("scaling mode", "None"),
        ("underscan", "off"),
        ("content type", "No Data"),
        ("HDCP Content Type", "HDCP Type0"),
        ("allm_mode", "Disabled"),
    ] {
        if let Some(x) = enum_prop_value(device, conn, prop, value) {
            out.push(x);
        }
    }

    out
}

/// Neutral CRTC state: VRR off, gamma/degamma/CTM cleared.
fn crtc_neutral_props(device: &DrmDevice, crtc: crtc::Handle) -> Vec<(property::Handle, u64)> {
    let mut out = Vec::new();

    if let Some((handle, info, _)) = find_drm_property(device, crtc, "VRR_ENABLED") {
        if matches!(info.value_type(), property::ValueType::Boolean) {
            out.push((handle, 0));
        }
    }

    for name in ["GAMMA_LUT", "DEGAMMA_LUT", "CTM"] {
        if let Some(handle) = prop_handle(device, crtc, name) {
            out.push((handle, 0));
        }
    }

    out
}

/// Neutral plane state: full-frame damage, no rotation.
fn plane_neutral_props(device: &DrmDevice, plane: plane::Handle) -> Vec<(property::Handle, u64)> {
    let mut out = Vec::new();

    if let Some(handle) = prop_handle(device, plane, "FB_DAMAGE_CLIPS") {
        out.push((handle, 0));
    }

    // DRM_MODE_ROTATE_0.
    if let Some((handle, info, _)) = find_drm_property(device, plane, "rotation") {
        if matches!(info.value_type(), property::ValueType::Bitmask) {
            out.push((handle, 1));
        }
    }

    out
}

impl MemberProps {
    fn new(
        device: &DrmDevice,
        connector: connector::Handle,
        crtc: crtc::Handle,
        plane: plane::Handle,
    ) -> anyhow::Result<Self> {
        let missing = |name: &str| anyhow::anyhow!("missing required property: {name}");

        Ok(MemberProps {
            conn_crtc_id: prop_handle(device, connector, "CRTC_ID")
                .ok_or_else(|| missing("connector CRTC_ID"))?,
            crtc_mode_id: prop_handle(device, crtc, "MODE_ID")
                .ok_or_else(|| missing("crtc MODE_ID"))?,
            crtc_active: prop_handle(device, crtc, "ACTIVE")
                .ok_or_else(|| missing("crtc ACTIVE"))?,
            plane_fb_id: prop_handle(device, plane, "FB_ID")
                .ok_or_else(|| missing("plane FB_ID"))?,
            plane_crtc_id: prop_handle(device, plane, "CRTC_ID")
                .ok_or_else(|| missing("plane CRTC_ID"))?,
            plane_src_x: prop_handle(device, plane, "SRC_X")
                .ok_or_else(|| missing("plane SRC_X"))?,
            plane_src_y: prop_handle(device, plane, "SRC_Y")
                .ok_or_else(|| missing("plane SRC_Y"))?,
            plane_src_w: prop_handle(device, plane, "SRC_W")
                .ok_or_else(|| missing("plane SRC_W"))?,
            plane_src_h: prop_handle(device, plane, "SRC_H")
                .ok_or_else(|| missing("plane SRC_H"))?,
            plane_crtc_x: prop_handle(device, plane, "CRTC_X")
                .ok_or_else(|| missing("plane CRTC_X"))?,
            plane_crtc_y: prop_handle(device, plane, "CRTC_Y")
                .ok_or_else(|| missing("plane CRTC_Y"))?,
            plane_crtc_w: prop_handle(device, plane, "CRTC_W")
                .ok_or_else(|| missing("plane CRTC_W"))?,
            plane_crtc_h: prop_handle(device, plane, "CRTC_H")
                .ok_or_else(|| missing("plane CRTC_H"))?,
            connector_neutral: connector_neutral_props(device, connector),
            crtc_neutral: crtc_neutral_props(device, crtc),
            plane_neutral: plane_neutral_props(device, plane),
        })
    }
}

/// One physical tile of a connected tiled group.
pub struct TiledMember {
    pub crtc: crtc::Handle,
    pub connector: connector::Handle,
    /// Held for plane info and the primary-plane claim; never committed through directly.
    surface: DrmSurface,
    /// Horizontal offset of this tile within the full image, in pixels.
    pub crtc_x: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    mode_blob: Option<u64>,
    props: MemberProps,
    pub gamma_props: Option<GammaProps>,
}

impl TiledMember {
    pub fn new(
        device: &DrmDevice,
        plan: &TiledMemberPlan,
        surface: DrmSurface,
        gamma_props: Option<GammaProps>,
    ) -> anyhow::Result<Self> {
        let connector = plan.connector.handle();
        let props = MemberProps::new(device, connector, plan.crtc, surface.plane())?;
        Ok(TiledMember {
            crtc: plan.crtc,
            connector,
            surface,
            crtc_x: plan.crtc_x,
            tile_w: plan.tile_w,
            tile_h: plan.tile_h,
            mode_blob: None,
            props,
            gamma_props,
        })
    }

    pub fn plane(&self) -> plane::Handle {
        self.surface.plane()
    }

    pub fn surface(&self) -> &DrmSurface {
        &self.surface
    }

    fn mode_blob(&mut self, device: &DrmDevice, mode: DrmMode) -> anyhow::Result<u64> {
        if let Some(blob) = self.mode_blob {
            return Ok(blob);
        }
        let value = device
            .create_property_blob(&mode)
            .context("error creating mode blob")?;
        let property::Value::Blob(id) = value else {
            bail!("mode blob property has unexpected value type");
        };
        self.mode_blob = Some(id);
        Ok(id)
    }

    fn destroy_mode_blob(&mut self, device: &DrmDevice) {
        if let Some(id) = self.mode_blob.take() {
            if let Err(err) = device.destroy_property_blob(id) {
                debug!("error destroying mode blob: {err:?}");
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TiledKmsMemberFingerprint {
    crtc: crtc::Handle,
    connector: connector::Handle,
    plane: plane::Handle,
    crtc_x: u32,
    tile_w: u32,
    tile_h: u32,
    props: MemberProps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TiledKmsFingerprint {
    timing: TimingFingerprint,
    members: [TiledKmsMemberFingerprint; 2],
}

impl TiledKmsFingerprint {
    fn new(members: &[TiledMember; 2], tile_mode: DrmMode) -> Self {
        let member = |member: &TiledMember| TiledKmsMemberFingerprint {
            crtc: member.crtc,
            connector: member.connector,
            plane: member.plane(),
            crtc_x: member.crtc_x,
            tile_w: member.tile_w,
            tile_h: member.tile_h,
            props: member.props.clone(),
        };
        Self {
            timing: TimingFingerprint::from(tile_mode),
            members: [member(&members[0]), member(&members[1])],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TiledValidationFingerprint {
    generation: u64,
    kms: TiledKmsFingerprint,
    buffer_format: smithay::backend::allocator::Format,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiledCommitState {
    NeedsValidation,
    ValidatedNeedsModeset,
    Active,
}

#[derive(Debug)]
struct TiledCommitTracker<F> {
    state: TiledCommitState,
    validated: Option<F>,
}

impl<F> Default for TiledCommitTracker<F> {
    fn default() -> Self {
        Self {
            state: TiledCommitState::NeedsValidation,
            validated: None,
        }
    }
}

impl<F> TiledCommitTracker<F> {
    fn prepare(&mut self, matches: impl FnOnce(&F) -> bool) -> TiledCommitState {
        if self.state != TiledCommitState::NeedsValidation
            && !self.validated.as_ref().is_some_and(matches)
        {
            self.reset();
        }
        self.state
    }

    fn validated(&mut self, fingerprint: F) {
        self.validated = Some(fingerprint);
        self.state = TiledCommitState::ValidatedNeedsModeset;
    }

    fn activated(&mut self) {
        assert_eq!(self.state, TiledCommitState::ValidatedNeedsModeset);
        self.state = TiledCommitState::Active;
    }

    fn reset(&mut self) {
        self.validated = None;
        self.state = TiledCommitState::NeedsValidation;
    }
}

/// A connected tiled group: one logical output rendered as a single framebuffer and scanned
/// out through two CRTCs.
pub struct TiledGroup {
    pub name: OutputName,
    pub config: Option<OutputConfigBinding>,
    pub id: OutputId,
    /// Ordered by `crtc_x`; `members[0]` is the left (primary) tile.
    pub members: [TiledMember; 2],
    /// The common native tile mode each member CRTC is driven with.
    pub tile_mode: DrmMode,
    /// Synthesized full-size Wayland mode for the logical output.
    wl_mode: WlMode,
    kms_fingerprint: TiledKmsFingerprint,
    commit: TiledCommitTracker<TiledValidationFingerprint>,
    swapchain: Swapchain<GbmAllocator<DrmDeviceFd>>,
    allocated_slots: usize,
    fb_exporter: GbmFramebufferExporter<DrmDeviceFd>,
    pub damage_tracker: OutputDamageTracker,
    flip_state: TiledFlipState<PendingTiledFrame>,
    /// Buffer currently scanned out; held until its replacement completes on both CRTCs.
    displayed_buffer: Option<TiledScanoutBuffer>,
    /// Watchdog armed for every submitted frame; tears down the group if both completion
    /// events do not arrive.
    pub completion_watchdog: Option<RegistrationToken>,
    /// Gamma change to apply upon session resume.
    pub pending_gamma_change: Option<Option<Vec<u16>>>,
    format: Fourcc,
    /// Tracy frame that goes from vblank to vblank.
    pub vblank_frame: Option<tracy_client::Frame>,
    /// Frame name for the VBlank frame.
    pub vblank_frame_name: tracy_client::FrameName,
    /// Plot name for the time since presentation plot.
    pub time_since_presentation_plot_name: tracy_client::PlotName,
    /// Plot name for the presentation misprediction plot.
    pub presentation_misprediction_plot_name: tracy_client::PlotName,
    /// Plot name for the vblank sequence delta plot.
    pub sequence_delta_plot_name: tracy_client::PlotName,
}

impl TiledGroup {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: OutputName,
        config: Option<OutputConfigBinding>,
        id: OutputId,
        members: [TiledMember; 2],
        tile_mode: DrmMode,
        wl_mode: WlMode,
        allocator: GbmAllocator<DrmDeviceFd>,
        fb_exporter: GbmFramebufferExporter<DrmDeviceFd>,
        damage_tracker: OutputDamageTracker,
        format: Fourcc,
        modifiers: Vec<Modifier>,
        trace_names: TiledTraceNames,
    ) -> Self {
        let kms_fingerprint = TiledKmsFingerprint::new(&members, tile_mode);
        let swapchain = Swapchain::new(
            allocator,
            wl_mode.size.w as u32,
            wl_mode.size.h as u32,
            format,
            modifiers,
        );

        TiledGroup {
            name,
            config,
            id,
            members,
            tile_mode,
            wl_mode,
            kms_fingerprint,
            commit: TiledCommitTracker::default(),
            swapchain,
            allocated_slots: 0,
            fb_exporter,
            damage_tracker,
            flip_state: TiledFlipState::Idle,
            displayed_buffer: None,
            completion_watchdog: None,
            pending_gamma_change: None,
            format,
            vblank_frame: None,
            vblank_frame_name: trace_names.vblank_frame,
            time_since_presentation_plot_name: trace_names.time_since_presentation,
            presentation_misprediction_plot_name: trace_names.presentation_misprediction,
            sequence_delta_plot_name: trace_names.sequence_delta,
        }
    }

    pub fn primary_crtc(&self) -> crtc::Handle {
        self.members[0].crtc
    }

    pub fn member_crtcs(&self) -> [crtc::Handle; 2] {
        [self.members[0].crtc, self.members[1].crtc]
    }

    /// Whether every live KMS and configuration property still matches a fresh plan.
    pub fn matches_plan(&self, plan: &TiledGroupPlan) -> bool {
        let config_matches = match (&self.config, &plan.config) {
            (Some(a), Some(b)) => a.equivalent(b),
            (None, None) => true,
            _ => false,
        };

        self.name == plan.name
            && config_matches
            && TimingFingerprint::from(self.tile_mode) == TimingFingerprint::from(plan.tile_mode)
            && self
                .members
                .iter()
                .zip(&plan.members)
                .all(|(member, planned)| {
                    member.crtc == planned.crtc
                        && member.connector == planned.connector.handle()
                        && member.crtc_x == planned.crtc_x
                        && member.tile_w == planned.tile_w
                        && member.tile_h == planned.tile_h
                })
    }

    pub fn format(&self) -> Fourcc {
        self.format
    }

    /// Whether a frame has been committed but not yet presented on both tiles.
    pub fn has_pending_frame(&self) -> bool {
        self.flip_state.has_pending_frame()
    }

    fn reset_frame_state(&mut self) -> bool {
        let had_pending_frame = self.flip_state.reset();
        self.commit.reset();
        self.displayed_buffer = None;
        self.swapchain.reset_buffers();
        self.allocated_slots = 0;
        had_pending_frame
    }

    pub fn cancel_completion_watchdog<Data>(&mut self, event_loop: &LoopHandle<'_, Data>) {
        cancel_event_source(&mut self.completion_watchdog, event_loop);
    }

    /// Invalidates render and validation state after resume without releasing any framebuffer
    /// that either CRTC may still scan out. Pending completion remains pending and is watched
    /// again after monitor activation; if its events were lost, normal watchdog teardown safely
    /// retires both heads.
    pub fn invalidate_after_resume<Data>(&mut self, event_loop: &LoopHandle<'_, Data>) {
        self.cancel_completion_watchdog(event_loop);
        self.commit.reset();
        self.swapchain.reset_buffer_ages();
    }

    /// The synthesized full-size mode advertised on the logical output (e.g. 5120x2880@60).
    pub fn wl_mode(&self) -> WlMode {
        self.wl_mode
    }

    /// Acquires a swapchain slot for rendering the next frame.
    ///
    /// Dropping the returned slot releases it automatically on every pre-commit error path.
    pub fn acquire(&mut self) -> anyhow::Result<Option<Slot<GbmBuffer>>> {
        let slot = self
            .swapchain
            .acquire()
            .context("error allocating tiled scanout buffer")?;
        if let Some(slot) = &slot {
            if slot.userdata().get::<TiledSlotAllocation>().is_none() {
                slot.userdata().insert_if_missing(|| TiledSlotAllocation);
                self.allocated_slots += 1;
                if self.allocated_slots > EXPECTED_TILED_SWAPCHAIN_SLOTS {
                    warn!(
                        "tiled-groups: allocated an unexpected fourth swapchain slot for {}",
                        self.name.connector
                    );
                } else {
                    debug!(
                        "tiled-groups: allocated swapchain slot {} for {}",
                        self.allocated_slots, self.name.connector
                    );
                }
            }
        }
        Ok(slot)
    }

    /// The cached dmabuf for a slot, for binding as the render target.
    pub fn slot_dmabuf(&self, slot: &Slot<GbmBuffer>) -> anyhow::Result<Dmabuf> {
        slot.export().context("error exporting tiled dmabuf")
    }

    /// The cached DRM framebuffer for a slot, exported on first use.
    pub fn slot_framebuffer(
        &self,
        device: &DrmDevice,
        slot: &Slot<GbmBuffer>,
    ) -> anyhow::Result<framebuffer::Handle> {
        if slot.userdata().get::<GbmFramebuffer>().is_none() {
            let fb = self
                .fb_exporter
                .add_framebuffer(device.device_fd(), ExportBuffer::Allocator(&**slot), false)
                .context("error exporting tiled framebuffer")?
                .context("framebuffer export was skipped")?;
            slot.userdata().insert_if_missing(|| fb);
        }

        Ok(*slot.userdata().get::<GbmFramebuffer>().unwrap().as_ref())
    }

    pub fn frame_submitted(&mut self, slot: &Slot<GbmBuffer>) {
        self.swapchain.submitted(slot);
    }

    pub fn reset_buffer_ages(&mut self) {
        self.swapchain.reset_buffer_ages();
    }

    pub fn invalidate_validation(&mut self) {
        self.commit.reset();
        self.swapchain.reset_buffer_ages();
    }

    /// Returns whether the already-active KMS state may accept a strict direct flip.
    pub fn direct_scanout_active(&mut self, generation: u64) -> bool {
        let kms = &self.kms_fingerprint;
        let was_validated = self.commit.state != TiledCommitState::NeedsValidation;
        let state = self
            .commit
            .prepare(|validated| validated.generation == generation && validated.kms == *kms);
        if was_validated && state == TiledCommitState::NeedsValidation {
            self.swapchain.reset_buffer_ages();
        }
        state == TiledCommitState::Active
    }

    /// Returning from direct scanout invalidates all composited buffer history.
    pub fn prepare_composited_frame(&mut self) {
        if self
            .displayed_buffer
            .as_ref()
            .is_some_and(TiledScanoutBuffer::is_direct)
        {
            self.swapchain.reset_buffer_ages();
        }
    }

    pub fn prepare_commit(&mut self, generation: u64, slot: &Slot<GbmBuffer>) -> TiledCommitState {
        let buffer_format = smithay::backend::allocator::Buffer::format(&**slot);
        let kms = &self.kms_fingerprint;
        let was_validated = self.commit.state != TiledCommitState::NeedsValidation;
        let state = self.commit.prepare(|validated| {
            validated.generation == generation
                && validated.buffer_format == buffer_format
                && validated.kms == *kms
        });
        if was_validated && state == TiledCommitState::NeedsValidation {
            self.swapchain.reset_buffer_ages();
        }
        state
    }

    pub fn validation_succeeded(&mut self, generation: u64, slot: &Slot<GbmBuffer>) {
        self.commit.validated(TiledValidationFingerprint {
            generation,
            kms: self.kms_fingerprint.clone(),
            buffer_format: smithay::backend::allocator::Buffer::format(&**slot),
        });
    }

    pub fn modeset_succeeded(&mut self) {
        self.commit.activated();
    }

    pub fn direct_framebuffer(
        &self,
        device: &DrmDevice,
        buffer: &RendererBuffer,
    ) -> anyhow::Result<Option<GbmFramebuffer>> {
        self.fb_exporter
            .add_framebuffer(device.device_fd(), ExportBuffer::Wayland(buffer), true)
            .context("error exporting tiled direct-scanout framebuffer")
    }

    fn build_full_request(
        &mut self,
        device: &DrmDevice,
        fb: Option<framebuffer::Handle>,
        active: bool,
    ) -> anyhow::Result<AtomicModeReq> {
        let mut req = AtomicModeReq::new();

        for member in &mut self.members {
            let mode_blob = if active {
                Some(member.mode_blob(device, self.tile_mode)?)
            } else {
                None
            };
            let props = &member.props;

            req.add_property(
                member.connector,
                props.conn_crtc_id,
                property::Value::CRTC(active.then_some(member.crtc)),
            );
            req.add_property(
                member.crtc,
                props.crtc_mode_id,
                property::Value::Blob(mode_blob.unwrap_or(0)),
            );
            req.add_property(
                member.crtc,
                props.crtc_active,
                property::Value::Boolean(active),
            );

            if active {
                let fb = fb.context("no framebuffer for active tiled commit")?;
                req.add_property(
                    member.plane(),
                    props.plane_fb_id,
                    property::Value::Framebuffer(Some(fb)),
                );
                req.add_property(
                    member.plane(),
                    props.plane_crtc_id,
                    property::Value::CRTC(Some(member.crtc)),
                );
                // SRC_* are 16.16 fixed point; the crop selects this tile's half of the
                // shared framebuffer. CRTC_* are plain pixels relative to the tile's own
                // CRTC, so the destination is (0, 0) on both tiles.
                req.add_property(
                    member.plane(),
                    props.plane_src_x,
                    property::Value::UnsignedRange(u64::from(member.crtc_x) << 16),
                );
                req.add_property(
                    member.plane(),
                    props.plane_src_y,
                    property::Value::UnsignedRange(0),
                );
                req.add_property(
                    member.plane(),
                    props.plane_src_w,
                    property::Value::UnsignedRange(u64::from(member.tile_w) << 16),
                );
                req.add_property(
                    member.plane(),
                    props.plane_src_h,
                    property::Value::UnsignedRange(u64::from(member.tile_h) << 16),
                );
                req.add_property(
                    member.plane(),
                    props.plane_crtc_x,
                    property::Value::SignedRange(0),
                );
                req.add_property(
                    member.plane(),
                    props.plane_crtc_y,
                    property::Value::SignedRange(0),
                );
                req.add_property(
                    member.plane(),
                    props.plane_crtc_w,
                    property::Value::UnsignedRange(u64::from(member.tile_w)),
                );
                req.add_property(
                    member.plane(),
                    props.plane_crtc_h,
                    property::Value::UnsignedRange(u64::from(member.tile_h)),
                );
            } else {
                req.add_property(
                    member.plane(),
                    props.plane_fb_id,
                    property::Value::Framebuffer(None),
                );
                req.add_property(
                    member.plane(),
                    props.plane_crtc_id,
                    property::Value::CRTC(None),
                );
            }

            for (handle, value) in props.connector_neutral.iter().copied() {
                req.add_raw_property(member.connector.into(), handle, value);
            }
            let current_gamma = active
                .then_some(member.gamma_props.as_ref())
                .flatten()
                .map(GammaProps::current_blob_property);
            for (handle, value) in props.crtc_neutral.iter().copied() {
                let value = current_gamma
                    .filter(|(gamma_handle, _)| *gamma_handle == handle)
                    .map_or(value, |(_, gamma_blob)| gamma_blob);
                req.add_raw_property(member.crtc.into(), handle, value);
            }
            for (handle, value) in props.plane_neutral.iter().copied() {
                req.add_raw_property(member.plane().into(), handle, value);
            }
        }

        Ok(req)
    }

    /// Builds a full TEST_ONLY request for both tiles from one framebuffer.
    pub fn build_validation_request(
        &mut self,
        device: &DrmDevice,
        fb: framebuffer::Handle,
    ) -> anyhow::Result<AtomicModeReq> {
        self.build_full_request(device, Some(fb), true)
    }

    /// Builds the real all-or-nothing modeset after validation.
    pub fn build_modeset_request(
        &mut self,
        device: &DrmDevice,
        fb: framebuffer::Handle,
    ) -> anyhow::Result<AtomicModeReq> {
        self.build_full_request(device, Some(fb), true)
    }

    /// Builds a steady-state page flip. All geometry and routing remain unchanged.
    pub fn build_flip_request(&self, fb: framebuffer::Handle) -> AtomicModeReq {
        let mut req = AtomicModeReq::new();
        for member in &self.members {
            req.add_property(
                member.plane(),
                member.props.plane_fb_id,
                property::Value::Framebuffer(Some(fb)),
            );
        }
        req
    }

    /// Builds a blocking request that disables both member CRTCs together.
    pub fn build_disable_request(&mut self, device: &DrmDevice) -> anyhow::Result<AtomicModeReq> {
        self.build_full_request(device, None, false)
    }

    pub fn modeset_property_count(&self) -> usize {
        self.members
            .iter()
            .map(|member| {
                13 + member.props.connector_neutral.len()
                    + member.props.crtc_neutral.len()
                    + member.props.plane_neutral.len()
            })
            .sum()
    }

    /// Records a member vblank event; once both tiles delivered an event for the current
    /// frame, returns the primary tile's sequence with the later tile's timestamp.
    pub fn record_member_event(
        &mut self,
        crtc: crtc::Handle,
        meta: DrmEventMetadata,
    ) -> Option<(PendingTiledFrame, DrmEventMetadata)> {
        let idx = self.members.iter().position(|m| m.crtc == crtc)?;
        self.flip_state.record_member_event(idx, meta)
    }

    pub fn pending_submission(&self, crtc: crtc::Handle) -> Option<CrtcSubmissionId> {
        let member = self.members.iter().position(|member| member.crtc == crtc)?;
        Some(self.flip_state.pending()?.submissions[member])
    }

    /// Holds the newly displayed slot, releases the previous one, and returns presentation data.
    pub fn frame_presented(&mut self, frame: PendingTiledFrame) -> TiledPresentation {
        self.displayed_buffer = Some(frame.buffer);
        TiledPresentation {
            feedback: frame.feedback,
            target_presentation_time: frame.target_presentation_time,
        }
    }

    /// Marks a frame as committed, awaiting presentation on both tiles.
    pub fn frame_committed(&mut self, frame: PendingTiledFrame) {
        self.flip_state.frame_committed(frame);
    }

    /// Disables both member CRTCs with a single blocking commit and releases all buffers.
    pub fn clear<Data>(
        &mut self,
        device: &DrmDevice,
        event_loop: &LoopHandle<'_, Data>,
    ) -> anyhow::Result<bool> {
        let req = self.build_disable_request(device)?;
        device
            .atomic_commit(
                smithay::reexports::drm::control::AtomicCommitFlags::ALLOW_MODESET,
                req,
            )
            .context("error clearing tiled group")?;
        self.cancel_completion_watchdog(event_loop);
        Ok(self.reset_frame_state())
    }

    /// Destroys kernel resources (mode blobs) held by the group.
    pub fn destroy(&mut self, device: &DrmDevice) {
        for member in &mut self.members {
            member.destroy_mode_blob(device);
        }
    }
}

fn aggregate_member_events(
    primary: DrmEventMetadata,
    secondary: DrmEventMetadata,
) -> DrmEventMetadata {
    let timestamp = |meta: &DrmEventMetadata| match meta.time {
        DrmEventTime::Monotonic(time) => time,
        DrmEventTime::Realtime(_) => Duration::ZERO,
    };
    let time = if timestamp(&primary) >= timestamp(&secondary) {
        primary.time
    } else {
        secondary.time
    };

    DrmEventMetadata {
        sequence: primary.sequence,
        time,
    }
}

fn cancel_event_source<Data>(
    token: &mut Option<RegistrationToken>,
    event_loop: &LoopHandle<'_, Data>,
) {
    if let Some(token) = token.take() {
        event_loop.remove(token);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use drm_ffi::drm_mode_modeinfo;
    use smithay::backend::allocator::{Allocator, Buffer, Format};
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    use smithay::reexports::calloop::EventLoop;
    use smithay::utils::{Buffer as BufferCoords, Size};

    use super::*;

    #[derive(Debug)]
    struct FakeBuffer {
        size: Size<i32, BufferCoords>,
        format: Format,
    }

    impl Buffer for FakeBuffer {
        fn size(&self) -> Size<i32, BufferCoords> {
            self.size
        }

        fn format(&self) -> Format {
            self.format
        }
    }

    #[derive(Debug, Default)]
    struct FakeAllocator;

    impl Allocator for FakeAllocator {
        type Buffer = FakeBuffer;
        type Error = std::convert::Infallible;

        fn create_buffer(
            &mut self,
            width: u32,
            height: u32,
            fourcc: Fourcc,
            modifiers: &[Modifier],
        ) -> Result<Self::Buffer, Self::Error> {
            Ok(FakeBuffer {
                size: (width as i32, height as i32).into(),
                format: Format {
                    code: fourcc,
                    modifier: modifiers[0],
                },
            })
        }
    }

    #[test]
    fn swapchain_tracks_age_and_resets_conservatively() {
        let mut swapchain = Swapchain::new(
            FakeAllocator,
            5120,
            2880,
            Fourcc::Xrgb8888,
            vec![Modifier::Linear],
        );

        let first = swapchain.acquire().unwrap().unwrap();
        assert_eq!(first.age(), 0);
        swapchain.submitted(&first);
        assert_eq!(first.age(), 1);

        let displayed = swapchain.acquire().unwrap().unwrap();
        assert_eq!(displayed.age(), 0);
        swapchain.submitted(&displayed);
        assert_eq!(displayed.age(), 1);
        assert_eq!(first.age(), 2);

        drop(first);
        let reused = swapchain.acquire().unwrap().unwrap();
        assert_eq!(reused.age(), 2);

        swapchain.reset_buffer_ages();
        drop(displayed);
        drop(reused);
        assert_eq!(swapchain.acquire().unwrap().unwrap().age(), 0);
    }

    #[test]
    fn commit_tracker_reuses_only_an_exact_validation_fingerprint() {
        let mut commit = TiledCommitTracker::<u8>::default();
        assert_eq!(
            commit.prepare(|fingerprint| *fingerprint == 7),
            TiledCommitState::NeedsValidation
        );

        commit.validated(7);
        assert_eq!(
            commit.prepare(|fingerprint| *fingerprint == 7),
            TiledCommitState::ValidatedNeedsModeset
        );
        commit.activated();
        assert_eq!(
            commit.prepare(|fingerprint| *fingerprint == 7),
            TiledCommitState::Active
        );

        assert_eq!(
            commit.prepare(|fingerprint| *fingerprint == 8),
            TiledCommitState::NeedsValidation
        );
        commit.validated(8);
        commit.reset();
        assert_eq!(
            commit.prepare(|fingerprint| *fingerprint == 8),
            TiledCommitState::NeedsValidation
        );
    }

    #[test]
    fn cancel_event_source_removes_watchdog() {
        let mut event_loop = EventLoop::<()>::try_new().unwrap();
        let handle = event_loop.handle();
        let fired = Arc::new(AtomicBool::new(false));
        let fired_in_callback = fired.clone();
        let token = handle
            .insert_source(Timer::from_duration(Duration::ZERO), move |_, _, _| {
                fired_in_callback.store(true, Ordering::SeqCst);
                TimeoutAction::Drop
            })
            .unwrap();
        let mut watchdog = Some(token);

        cancel_event_source(&mut watchdog, &handle);
        event_loop.dispatch(Duration::ZERO, &mut ()).unwrap();

        assert!(watchdog.is_none());
        assert!(!fired.load(Ordering::SeqCst));
    }

    #[test]
    fn aggregate_events_keeps_primary_sequence_and_later_timestamp() {
        let primary = DrmEventMetadata {
            sequence: 1_000,
            time: DrmEventTime::Monotonic(Duration::from_millis(10)),
        };
        let secondary = DrmEventMetadata {
            sequence: 5,
            time: DrmEventTime::Monotonic(Duration::from_millis(12)),
        };

        let aggregated = aggregate_member_events(primary, secondary);
        assert_eq!(aggregated.sequence, 1_000);
        assert!(matches!(
            aggregated.time,
            DrmEventTime::Monotonic(time) if time == Duration::from_millis(12)
        ));
    }

    #[test]
    fn flip_state_handles_both_event_orders_and_duplicate_members() {
        let primary = DrmEventMetadata {
            sequence: 1_000,
            time: DrmEventTime::Monotonic(Duration::from_millis(10)),
        };
        let secondary = DrmEventMetadata {
            sequence: 5,
            time: DrmEventTime::Monotonic(Duration::from_millis(12)),
        };

        for order in [
            [(0, primary), (1, secondary)],
            [(1, secondary), (0, primary)],
        ] {
            let mut state = TiledFlipState::default();
            state.frame_committed("frame");
            assert!(state.record_member_event(order[0].0, order[0].1).is_none());
            // A duplicate from the first member must not complete the frame.
            assert!(state.record_member_event(order[0].0, order[0].1).is_none());
            let (frame, meta) = state.record_member_event(order[1].0, order[1].1).unwrap();
            assert_eq!(frame, "frame");
            assert_eq!(meta.sequence, primary.sequence);
            assert!(matches!(
                meta.time,
                DrmEventTime::Monotonic(time) if time == Duration::from_millis(12)
            ));
            assert!(!state.has_pending_frame());
        }
    }

    #[test]
    fn flip_state_reset_covers_every_pending_state() {
        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let event = DrmEventMetadata {
            sequence: 1,
            time: DrmEventTime::Monotonic(Duration::ZERO),
        };
        let mut state = TiledFlipState::default();
        assert!(!state.reset());

        state.frame_committed(1);
        assert!(state.reset());
        assert!(!state.has_pending_frame());

        state.frame_committed(2);
        assert!(state.record_member_event(0, event).is_none());
        assert!(state.reset());
        assert!(!state.has_pending_frame());

        let drops = Arc::new(AtomicUsize::new(0));
        let mut state = TiledFlipState::default();
        state.frame_committed(DropProbe(drops.clone()));
        assert!(state.reset());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tiled_failures_retry_with_bounded_backoff() {
        let now = Instant::now();
        let mut failures = TiledFailureTracker::default();

        for (attempt, delay) in [
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_secs(5),
        ]
        .into_iter()
        .enumerate()
        {
            let retry = failures
                .record("DP-4+DP-5".into(), TiledFailureClass::Retryable, now)
                .unwrap();
            assert_eq!(usize::from(retry.attempt), attempt);
            assert_eq!(retry.delay, delay);
            assert!(failures.blocks("DP-4+DP-5", now));
            assert!(!failures.retry_due("DP-4+DP-5", retry.generation, now));
            assert!(failures.retry_due("DP-4+DP-5", retry.generation, now + delay));
        }

        assert!(failures
            .record("DP-4+DP-5".into(), TiledFailureClass::Retryable, now)
            .is_none());
        assert_eq!(
            failures.policy("DP-4+DP-5"),
            Some(TiledFailurePolicy::UntilEnvironmentChange),
        );
        assert!(failures.blocks("DP-4+DP-5", now + Duration::from_secs(60)));
    }

    #[test]
    fn tiled_failure_generations_make_stale_retries_harmless() {
        let now = Instant::now();
        let mut failures = TiledFailureTracker::default();
        let old = failures
            .record("DP-4+DP-5".into(), TiledFailureClass::Retryable, now)
            .unwrap();
        let new = failures
            .record("DP-4+DP-5".into(), TiledFailureClass::Retryable, now)
            .unwrap();

        assert!(!failures.retry_due("DP-4+DP-5", old.generation, now + Duration::from_secs(10),));
        assert!(failures.retry_due("DP-4+DP-5", new.generation, now + Duration::from_secs(10),));

        failures.remove("DP-4+DP-5");
        assert!(!failures.retry_due("DP-4+DP-5", new.generation, now + Duration::from_secs(10),));
        assert!(!failures.blocks("DP-4+DP-5", now));
    }

    #[test]
    fn persistent_and_session_failures_wait_for_invalidation() {
        let now = Instant::now();
        let mut failures = TiledFailureTracker::default();

        failures.record(
            "persistent".into(),
            TiledFailureClass::UntilEnvironmentChange,
            now,
        );
        failures.record("session".into(), TiledFailureClass::UntilSessionResume, now);
        assert!(failures.blocks("persistent", now + Duration::from_secs(60)));
        assert!(failures.blocks("session", now + Duration::from_secs(60)));

        failures.clear();
        assert!(!failures.blocks("persistent", now));
        assert!(!failures.blocks("session", now));
    }

    fn modeinfo(clock: u32, w: u16, h: u16) -> drm_mode_modeinfo {
        drm_mode_modeinfo {
            clock,
            hdisplay: w,
            hsync_start: w + 8,
            hsync_end: w + 32,
            htotal: w + 80,
            vdisplay: h,
            vsync_start: h + 3,
            vsync_end: h + 10,
            vtotal: h + 28,
            vrefresh: 60,
            flags: drm_ffi::DRM_MODE_FLAG_NHSYNC | drm_ffi::DRM_MODE_FLAG_PVSYNC,
            name: [0; 32],
            type_: drm_ffi::DRM_MODE_TYPE_PREFERRED,
            hskew: 0,
            vscan: 0,
        }
    }

    fn candidate(
        _name: &str,
        identity: &str,
        tile: Option<TileInfo>,
        modes: Vec<DrmMode>,
    ) -> TileCandidate {
        TileCandidate {
            identity: identity.to_string(),
            tile,
            modes,
            plane_fourccs: vec![Fourcc::Xrgb8888, Fourcc::Argb8888],
        }
    }

    fn lg_tile(h_loc: u32) -> Option<TileInfo> {
        Some(TileInfo {
            group_id: 1,
            single_monitor: true,
            num_h_tiles: 2,
            num_v_tiles: 1,
            tile_h_loc: h_loc,
            tile_v_loc: 0,
            tile_w: 2560,
            tile_h: 2880,
        })
    }

    #[test]
    fn parse_tile_info() {
        assert_eq!(TileInfo::parse(b"1:1:2:1:0:0:2560:2880\0"), {
            let mut tile = lg_tile(0).unwrap();
            tile.group_id = 1;
            Some(tile)
        });
        assert_eq!(TileInfo::parse(b"1:1:2:1:1:0:2560:2880"), lg_tile(1),);
        // Wrong field count, bad values, garbage.
        assert_eq!(TileInfo::parse(b"1:1:2:1:0:0:2560\0"), None);
        assert_eq!(TileInfo::parse(b"1:1:2:1:0:0:2560:2880:extra\0"), None);
        assert_eq!(TileInfo::parse(b"1:1:0:1:0:0:2560:2880\0"), None);
        assert_eq!(TileInfo::parse(b"1:1:2:1:2:0:2560:2880\0"), None);
        assert_eq!(TileInfo::parse(b"1:1:2:1:0:0:0:2880\0"), None);
        assert_eq!(TileInfo::parse(b"1:1:2:1:0:0:65536:2880\0"), None);
        assert_eq!(TileInfo::parse(b"garbage\0"), None);
        assert_eq!(TileInfo::parse(b""), None);
    }

    #[test]
    fn fingerprint_ignores_name_and_type() {
        let a = DrmMode::from(modeinfo(533_250, 2560, 2880));
        let mut info_b = modeinfo(533_250, 2560, 2880);
        info_b.type_ = drm_ffi::DRM_MODE_TYPE_DRIVER;
        info_b.name[0] = b'X' as _;
        let b = DrmMode::from(info_b);
        assert_eq!(TimingFingerprint::from(a), TimingFingerprint::from(b));

        let c = DrmMode::from(modeinfo(533_251, 2560, 2880));
        assert_ne!(TimingFingerprint::from(a), TimingFingerprint::from(c));
    }

    #[test]
    fn validate_lg_5k_pair() {
        let modes_a = vec![
            DrmMode::from(modeinfo(533_250, 2560, 2880)),
            DrmMode::from(modeinfo(268_500, 2560, 1440)),
        ];
        let modes_b = modes_a.clone();
        let a = candidate("DP-4", "LG UltraFine 5K 123", lg_tile(0), modes_a);
        let b = candidate("DP-5", "LG UltraFine 5K 123", lg_tile(1), modes_b);

        let pair = validate_tile_pair(&a, &b, true).unwrap();
        assert_eq!(pair.tile_w, 2560);
        assert_eq!(pair.tile_h, 2880);
        assert_eq!(pair.a_x, 0);
        assert_eq!(pair.b_x, 2560);
        assert_eq!(pair.tile_mode.size(), (2560, 2880));

        // Swapped connector order must still yield the left tile at x = 0.
        let pair = validate_tile_pair(&b, &a, true).unwrap();
        assert_eq!(pair.a_x, 2560);
        assert_eq!(pair.b_x, 0);
    }

    #[test]
    fn reject_mismatching_pairs() {
        let modes = || vec![DrmMode::from(modeinfo(533_250, 2560, 2880))];
        let left = || candidate("DP-4", "LG UltraFine 5K 123", lg_tile(0), modes());
        let right = || candidate("DP-5", "LG UltraFine 5K 123", lg_tile(1), modes());

        // Different identity: never cross-pair two physical panels.
        let other_panel = candidate("DP-5", "LG UltraFine 5K 456", lg_tile(1), modes());
        assert_eq!(
            validate_tile_pair(&left(), &other_panel, true),
            Err(PairRejection::IdentityMismatch),
        );

        // Missing topology when required.
        let no_tile = candidate("DP-5", "LG UltraFine 5K 123", None, modes());
        assert_eq!(
            validate_tile_pair(&left(), &no_tile, true),
            Err(PairRejection::MissingTopology),
        );

        // Different TILE group id.
        let mut other_group = lg_tile(1).unwrap();
        other_group.group_id = 2;
        let other_group = candidate("DP-5", "LG UltraFine 5K 123", Some(other_group), modes());
        assert_eq!(
            validate_tile_pair(&left(), &other_group, true),
            Err(PairRejection::GroupIdMismatch),
        );

        // Different timings.
        let other_modes = vec![DrmMode::from(modeinfo(533_333, 2560, 2880))];
        let other_timing = candidate("DP-5", "LG UltraFine 5K 123", lg_tile(1), other_modes);
        assert_eq!(
            validate_tile_pair(&left(), &other_timing, true),
            Err(PairRejection::NoCommonMode),
        );

        // Different format supersets are valid when the tiles retain a common format.
        let mut other_format = right();
        other_format.plane_fourccs = vec![Fourcc::Xrgb8888];
        assert!(validate_tile_pair(&left(), &other_format, true).is_ok());

        // A disjoint format set is not viable.
        other_format.plane_fourccs = vec![Fourcc::Abgr8888];
        assert_eq!(
            validate_tile_pair(&left(), &other_format, true),
            Err(PairRejection::PlaneFormatMismatch),
        );
    }

    #[test]
    fn incomplete_explicit_group_claims_its_present_member() {
        let connected = ["DP-4", "DP-6"];
        let configured = ["DP-4".to_owned(), "DP-5".to_owned()];
        let mut claimed = HashSet::new();
        let (_members, missing) =
            resolve_and_claim_explicit_members(&configured, &connected, &mut claimed);

        assert_eq!(missing, ["DP-5"]);
        assert!(claimed.contains(&0));
        assert!(!claimed.contains(&1));
        assert!(explicit_group_suppresses_standalone(false, 1));
        assert!(explicit_group_suppresses_standalone(true, 0));
        assert!(!explicit_group_suppresses_standalone(false, 0));
    }

    #[test]
    fn either_missing_native_member_suppresses_the_present_member() {
        for configured in [
            ["DP-4".to_owned(), "DP-5".to_owned()],
            ["DP-5".to_owned(), "DP-4".to_owned()],
        ] {
            let connected = ["DP-4"];
            let mut claimed = HashSet::new();
            let (members, missing) =
                resolve_and_claim_explicit_members(&configured, &connected, &mut claimed);
            assert_eq!(members, vec![0]);
            assert_eq!(missing.len(), 1);
            assert!(claimed.contains(&0));
            assert!(explicit_group_suppresses_standalone(false, missing.len()));
        }
    }

    #[test]
    fn taking_an_inconsistent_live_index_prunes_stale_reverse_entries() {
        let primary = crtc::Handle::from(std::num::NonZeroU32::new(10).unwrap());
        let left = crtc::Handle::from(std::num::NonZeroU32::new(11).unwrap());
        let right = crtc::Handle::from(std::num::NonZeroU32::new(12).unwrap());
        let mut state = TiledDeviceState::default();
        state.by_crtc.insert(left, primary);
        state.by_crtc.insert(right, primary);

        assert!(state.take_live_group_by_member(left).is_none());
        assert!(!state.owns_crtc(left));
        assert!(!state.owns_crtc(right));
    }

    #[test]
    fn trace_names_and_output_id_are_reused_across_group_attempts() {
        let mut state = TiledDeviceState::default();
        let (first_id, first_names) = state.output_identity("DP-4+DP-5");
        let (second_id, second_names) = state.output_identity("DP-4+DP-5");

        assert_eq!(first_id, second_id);
        assert!(first_names.vblank_frame == second_names.vblank_frame);
        assert!(first_names.time_since_presentation == second_names.time_since_presentation);
        assert!(first_names.presentation_misprediction == second_names.presentation_misprediction);
        assert!(first_names.sequence_delta == second_names.sequence_delta);
    }

    #[test]
    fn explicit_group_without_topology() {
        let modes = || vec![DrmMode::from(modeinfo(533_250, 2560, 2880))];
        // Explicit config groups may lack TILE topology; layout is synthesized side-by-side
        // in config order and identity is not checked.
        let a = candidate("DP-4", "Panel A 1", None, modes());
        let b = candidate("DP-5", "Panel B 2", None, modes());

        let pair = validate_tile_pair(&a, &b, false).unwrap();
        assert_eq!(pair.a_x, 0);
        assert_eq!(pair.b_x, 2560);
        assert_eq!((pair.tile_w, pair.tile_h), (2560, 2880));
    }

    #[test]
    fn explicit_group_with_one_sided_topology() {
        let modes = || vec![DrmMode::from(modeinfo(533_250, 2560, 2880))];

        // Only the second connector exposes TILE topology, placing it on the left.
        let a = candidate("DP-4", "Panel A 1", None, modes());
        let b = candidate("DP-5", "Panel B 2", lg_tile(0), modes());
        let pair = validate_tile_pair(&a, &b, false).unwrap();
        assert_eq!(pair.a_x, 2560);
        assert_eq!(pair.b_x, 0);

        // And on the right.
        let b = candidate("DP-5", "Panel B 2", lg_tile(1), modes());
        let pair = validate_tile_pair(&a, &b, false).unwrap();
        assert_eq!(pair.a_x, 0);
        assert_eq!(pair.b_x, 2560);

        // The available topology constrains mode selection even when it is only exposed by
        // the second connector.
        let modes = || {
            vec![
                DrmMode::from(modeinfo(268_500, 2560, 1440)),
                DrmMode::from(modeinfo(533_250, 2560, 2880)),
            ]
        };
        let a = candidate("DP-4", "Panel A 1", None, modes());
        let b = candidate("DP-5", "Panel B 2", lg_tile(1), modes());
        let pair = validate_tile_pair(&a, &b, false).unwrap();
        assert_eq!(pair.tile_mode.size(), (2560, 2880));
    }

    #[test]
    fn explicit_group_still_validates_present_topology() {
        let modes = || vec![DrmMode::from(modeinfo(533_250, 2560, 2880))];

        // Config order says DP-5 first, but its live TILE position is right.
        let a = candidate("DP-5", "LG UltraFine 5K 123", lg_tile(1), modes());
        let b = candidate("DP-4", "LG UltraFine 5K 123", lg_tile(0), modes());
        let pair = validate_tile_pair(&a, &b, false).unwrap();
        assert_eq!(pair.a_x, 2560);
        assert_eq!(pair.b_x, 0);

        // Topology that is present but mismatching is rejected even for explicit groups.
        let mut wrong_size = lg_tile(1).unwrap();
        wrong_size.tile_w = 1920;
        let b = candidate("DP-5", "LG UltraFine 5K 123", Some(wrong_size), modes());
        assert_eq!(
            validate_tile_pair(&a, &b, false),
            Err(PairRejection::TileSizeMismatch),
        );
    }
}
