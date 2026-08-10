// SPDX-License-Identifier: GPL-3.0-or-later
//! KMS-free primitives shared by hardware-tiled and general display groups.

use std::array;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::time::Duration;

use anyhow::{bail, ensure, Result};
use smithay::backend::renderer::element::{
    RenderElementPresentationState, RenderElementState, RenderElementStates,
};

/// The first implementation deliberately bounds groups so frame completion can use a compact mask.
pub const MAX_GROUP_MEMBERS: usize = 4;
pub const STRICT_REFRESH_TOLERANCE_MILLIHZ: u32 = 5;
pub const RELAXED_REFRESH_TOLERANCE_MILLIHZ: u32 = 100;

/// Confirmed member retirement for a withdrawn logical output.
///
/// A member bit is set only after its KMS disable succeeds or its DRM device disappears. Keeping
/// this transition KMS-free makes partial-clear recovery exhaustively testable while the host
/// retains every surface whose bit remains clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberRetirement {
    expected_mask: u8,
    retired_mask: u8,
}

impl MemberRetirement {
    pub fn new(member_count: usize) -> Self {
        assert!((2..=MAX_GROUP_MEMBERS).contains(&member_count));
        Self {
            expected_mask: (1_u8 << member_count) - 1,
            retired_mask: 0,
        }
    }

    pub fn is_retired(self, member: usize) -> bool {
        member < MAX_GROUP_MEMBERS && self.retired_mask & (1_u8 << member) != 0
    }

    pub fn confirm(&mut self, member: usize) -> bool {
        let Some(bit) = 1_u8.checked_shl(member as u32) else {
            return false;
        };
        if bit & self.expected_mask == 0 {
            return false;
        }
        self.retired_mask |= bit;
        true
    }

    pub fn pending_mask(self) -> u8 {
        self.expected_mask & !self.retired_mask
    }

    pub fn is_complete(self) -> bool {
        self.pending_mask() == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownCommand {
    WithdrawOutput,
    EnterQuarantine,
    RetryDisable,
    ReleaseResources,
}

/// Pure public-lifetime/resource-ownership reducer for generalized teardown.
///
/// The host executes the initial commands in order, then reports confirmed member retirement.
/// A failed disable never republishes the output or releases resources; it emits only a scoped
/// retry command. This seam makes the public/quarantine invariant fault-testable without DRM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupTeardownLifecycle {
    public: bool,
    retirement: MemberRetirement,
}

impl GroupTeardownLifecycle {
    pub fn begin(member_count: usize) -> (Self, [TeardownCommand; 2]) {
        (
            Self {
                public: false,
                retirement: MemberRetirement::new(member_count),
            },
            [
                TeardownCommand::WithdrawOutput,
                TeardownCommand::EnterQuarantine,
            ],
        )
    }

    pub fn is_public(self) -> bool {
        self.public
    }

    pub fn is_retired(self, member: usize) -> bool {
        self.retirement.is_retired(member)
    }

    pub fn pending_mask(self) -> u8 {
        self.retirement.pending_mask()
    }

    pub fn confirm_retired(&mut self, member: usize) -> Option<TeardownCommand> {
        if !self.retirement.confirm(member) {
            return None;
        }
        self.retirement
            .is_complete()
            .then_some(TeardownCommand::ReleaseResources)
    }

    pub fn disable_failed(self) -> TeardownCommand {
        debug_assert!(!self.public && !self.retirement.is_complete());
        TeardownCommand::RetryDisable
    }

    pub fn is_complete(self) -> bool {
        self.retirement.is_complete()
    }
}

/// Reversible mutations accumulated while preparing a display group.
///
/// The host records created resources and changed pre-existing resources before each mutation.
/// Failed formation consumes the journal and applies both lists in reverse order; successful
/// formation consumes it without producing rollback work.
#[derive(Debug)]
pub struct FormationJournal<Created, Changed> {
    created: Vec<Created>,
    changed: Vec<Changed>,
}

impl<Created, Changed> Default for FormationJournal<Created, Changed> {
    fn default() -> Self {
        Self {
            created: Vec::new(),
            changed: Vec::new(),
        }
    }
}

impl<Created, Changed> FormationJournal<Created, Changed> {
    pub fn record_created(&mut self, entry: Created) {
        self.created.push(entry);
    }

    pub fn record_changed(&mut self, entry: Changed) {
        self.changed.push(entry);
    }

    pub fn commit(self) {}

    pub fn rollback(mut self) -> (Vec<Changed>, Vec<Created>) {
        self.changed.reverse();
        self.created.reverse();
        (self.changed, self.created)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GammaNodeState {
    Old,
    New,
    Unknown,
}

/// Per-node state for an emulated cross-device gamma transaction.
#[derive(Debug, Clone)]
pub struct GammaTransaction<Node> {
    states: HashMap<Node, GammaNodeState>,
}

impl<Node: Copy + Eq + Hash> GammaTransaction<Node> {
    pub fn new(nodes: impl IntoIterator<Item = Node>) -> Self {
        Self {
            states: nodes
                .into_iter()
                .map(|node| (node, GammaNodeState::Old))
                .collect(),
        }
    }

    pub fn mark_new(&mut self, node: Node) {
        *self.states.get_mut(&node).expect("unknown gamma node") = GammaNodeState::New;
    }

    pub fn mark_rollback(&mut self, node: Node, confirmed: bool) {
        *self.states.get_mut(&node).expect("unknown gamma node") = if confirmed {
            GammaNodeState::Old
        } else {
            GammaNodeState::Unknown
        };
    }

    pub fn state(&self, node: Node) -> Option<GammaNodeState> {
        self.states.get(&node).copied()
    }

    pub fn has_unknown(&self) -> bool {
        self.states
            .values()
            .any(|state| *state == GammaNodeState::Unknown)
    }

    pub fn states(&self) -> &HashMap<Node, GammaNodeState> {
        &self.states
    }
}

/// Identity and predicted sequence floor for one queued CRTC submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrtcSubmissionId {
    pub generation: u64,
    pub sequence_floor: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueuedCrtcSubmission {
    id: CrtcSubmissionId,
    drain: bool,
}

/// Classification made before touching Smithay's compositor state for a CRTC event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrtcEventClassification {
    Reject,
    Retire {
        submission: CrtcSubmissionId,
        drain: bool,
        current_generation: bool,
    },
}

/// Persistent per-CRTC event ordering across standalone/group ownership transitions.
///
/// Queued submissions survive generation changes so a delayed event can retire the exact old
/// Smithay frame without ever being mistaken for a newer group frame. Replacing the underlying
/// compositor marks old submissions as no-drain: their kernel events still advance the sequence
/// baseline, but must not mutate the replacement compositor.
#[derive(Debug, Default)]
pub struct CrtcEventTracker {
    generation: u64,
    last_sequence: Option<u32>,
    queued: VecDeque<QueuedCrtcSubmission>,
}

impl CrtcEventTracker {
    pub fn begin_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn enqueue(&mut self) -> CrtcSubmissionId {
        if self.generation == 0 {
            self.begin_generation();
        }
        let sequence_floor = self
            .last_sequence
            .map(|sequence| sequence.wrapping_add(1))
            .unwrap_or(0)
            .wrapping_add(self.queued.len() as u32);
        let id = CrtcSubmissionId {
            generation: self.generation,
            sequence_floor,
        };
        self.queued
            .push_back(QueuedCrtcSubmission { id, drain: true });
        id
    }

    pub fn cancel(&mut self, submission: CrtcSubmissionId) -> bool {
        let Some(position) = self
            .queued
            .iter()
            .position(|queued| queued.id == submission)
        else {
            return false;
        };
        self.queued.remove(position);
        true
    }

    pub fn replace_compositor(&mut self) -> u64 {
        for queued in &mut self.queued {
            queued.drain = false;
        }
        self.begin_generation()
    }

    pub fn classify(&self, sequence: u32) -> CrtcEventClassification {
        let Some(queued) = self.queued.front().copied() else {
            return CrtcEventClassification::Reject;
        };
        if sequence.wrapping_sub(queued.id.sequence_floor) >= (1 << 31) {
            return CrtcEventClassification::Reject;
        }
        CrtcEventClassification::Retire {
            submission: queued.id,
            drain: queued.drain,
            current_generation: queued.id.generation == self.generation,
        }
    }

    pub fn retired(&mut self, submission: CrtcSubmissionId, sequence: u32) -> bool {
        if !matches!(self.queued.front(), Some(queued) if queued.id == submission) {
            return false;
        }
        self.queued.pop_front();
        self.last_sequence = Some(sequence);
        true
    }

    pub fn last_sequence(&self) -> Option<u32> {
        self.last_sequence
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl MemberRect {
    fn right(self) -> Option<u32> {
        self.x.checked_add(self.width)
    }

    fn bottom(self) -> Option<u32> {
        self.y.checked_add(self.height)
    }

    fn overlaps(self, other: Self) -> bool {
        self.x < other.right().unwrap_or(u32::MAX)
            && other.x < self.right().unwrap_or(u32::MAX)
            && self.y < other.bottom().unwrap_or(u32::MAX)
            && other.y < self.bottom().unwrap_or(u32::MAX)
    }
}

/// A member's post-transform physical size and optional physical-pixel position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberGeometry {
    pub size: MemberSize,
    pub position: Option<(u32, u32)>,
}

/// Validate and normalize a rectangular 2-4 member group.
///
/// If every position is omitted, members are laid out in one horizontal row in declaration order.
/// If any position is present, every position is required and the rectangles must cover exactly one
/// bounding rectangle whose origin is `(0, 0)`, without gaps or overlap. All units are
/// post-transform physical pixels; logical-space rounding is intentionally excluded from group
/// geometry.
pub fn group_layout(members: &[MemberGeometry]) -> Result<(MemberSize, Vec<MemberRect>)> {
    ensure!(
        (2..=MAX_GROUP_MEMBERS).contains(&members.len()),
        "display-group requires between 2 and {MAX_GROUP_MEMBERS} members"
    );
    ensure!(
        members
            .iter()
            .all(|member| member.size.width > 0 && member.size.height > 0),
        "display-group member sizes must be non-zero"
    );

    let positioned = members
        .iter()
        .filter(|member| member.position.is_some())
        .count();
    ensure!(
        positioned == 0 || positioned == members.len(),
        "display-group positions must be supplied for every member or omitted for every member"
    );

    let rects = if positioned == 0 {
        ensure!(
            members
                .iter()
                .all(|member| member.size.height == members[0].size.height),
            "display-group automatic row layout requires equal member heights; supply explicit positions for a different rectangular arrangement"
        );
        let mut x = 0_u32;
        let mut rects = Vec::with_capacity(members.len());
        for member in members {
            rects.push(MemberRect {
                x,
                y: 0,
                width: member.size.width,
                height: member.size.height,
            });
            x = x
                .checked_add(member.size.width)
                .ok_or_else(|| anyhow::anyhow!("display-group width overflows u32"))?;
        }
        rects
    } else {
        members
            .iter()
            .map(|member| {
                let (x, y) = member.position.unwrap();
                MemberRect {
                    x,
                    y,
                    width: member.size.width,
                    height: member.size.height,
                }
            })
            .collect::<Vec<_>>()
    };

    let width = rects
        .iter()
        .map(|rect| rect.right())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| anyhow::anyhow!("display-group width overflows u32"))?
        .into_iter()
        .max()
        .unwrap();
    let height = rects
        .iter()
        .map(|rect| rect.bottom())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| anyhow::anyhow!("display-group height overflows u32"))?
        .into_iter()
        .max()
        .unwrap();
    ensure!(
        u16::try_from(width).is_ok() && u16::try_from(height).is_ok(),
        "display-group dimensions exceed the output protocol limit"
    );

    ensure!(
        rects.iter().any(|rect| rect.x == 0) && rects.iter().any(|rect| rect.y == 0),
        "display-group bounding box must start at (0, 0)"
    );

    for (idx, rect) in rects.iter().copied().enumerate() {
        if rects[idx + 1..]
            .iter()
            .copied()
            .any(|other| rect.overlaps(other))
        {
            bail!("display-group member rectangles overlap");
        }
    }

    let covered = rects.iter().try_fold(0_u64, |area, rect| {
        area.checked_add(u64::from(rect.width) * u64::from(rect.height))
    });
    let bounding = u64::from(width) * u64::from(height);
    ensure!(
        covered == Some(bounding),
        "display-group member rectangles must cover their bounding rectangle without gaps"
    );

    Ok((MemberSize { width, height }, rects))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshSync {
    #[default]
    Strict,
    Relaxed,
}

impl RefreshSync {
    pub fn tolerance_millihz(self) -> u32 {
        match self {
            Self::Strict => STRICT_REFRESH_TOLERANCE_MILLIHZ,
            Self::Relaxed => RELAXED_REFRESH_TOLERANCE_MILLIHZ,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshMode {
    pub millihz: u32,
    pub preferred: bool,
}

/// Pick one compatible refresh for every member, preferring common preferred modes and then the
/// highest pacing rate. This search runs before strict formation fails, so independently selected
/// preferred modes do not hide a shared refresh.
pub fn choose_common_refresh(
    modes: &[Vec<RefreshMode>],
    policy: RefreshSync,
) -> Option<Vec<RefreshMode>> {
    if !(2..=MAX_GROUP_MEMBERS).contains(&modes.len()) || modes.iter().any(Vec::is_empty) {
        return None;
    }

    let tolerance = policy.tolerance_millihz();
    let mut candidates = modes
        .iter()
        .flatten()
        .map(|mode| mode.millihz)
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates.dedup();

    candidates
        .into_iter()
        .filter_map(|target| {
            let selected = modes
                .iter()
                .map(|member_modes| {
                    member_modes
                        .iter()
                        .copied()
                        .filter(|mode| mode.millihz.abs_diff(target) <= tolerance)
                        .min_by_key(|mode| {
                            (
                                mode.millihz.abs_diff(target),
                                !mode.preferred,
                                std::cmp::Reverse(mode.millihz),
                            )
                        })
                })
                .collect::<Option<Vec<_>>>()?;
            let spread = selected.iter().map(|mode| mode.millihz).max().unwrap()
                - selected.iter().map(|mode| mode.millihz).min().unwrap();
            if spread > tolerance {
                return None;
            }
            let preferred = selected.iter().filter(|mode| mode.preferred).count();
            let pacing = selected.iter().map(|mode| mode.millihz).min().unwrap();
            Some(((std::cmp::Reverse(spread), preferred, pacing), selected))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, selected)| selected)
}

/// Refresh interval reported for the logical output. Groups pace at the slowest selected member,
/// even when a faster member is the only one submitted for a particular frame.
pub fn pacing_refresh_interval(selected: &[RefreshMode]) -> Option<Duration> {
    let millihz = selected.iter().map(|mode| mode.millihz).min()?;
    (millihz != 0).then(|| Duration::from_secs_f64(1000.0 / f64::from(millihz)))
}

/// Merge member-local element states for one logical output.
///
/// Visible area is summed in physical pixels. An element is reported as zero-copy only when every
/// visible fragment across every member was scanned out directly; one rendered fragment makes the
/// logical element rendered. Members where the element is absent or skipped are neutral.
pub fn merge_render_element_states(merged: &mut RenderElementStates, member: &RenderElementStates) {
    for (id, next) in &member.states {
        let Some(current) = merged.states.get_mut(id) else {
            merged.states.insert(id.clone(), *next);
            continue;
        };

        current.visible_area = current.visible_area.saturating_add(next.visible_area);
        current.needs_capture |= next.needs_capture;
        current.presentation_state = merge_presentation_state(*current, *next);
    }
}

fn merge_presentation_state(
    current: RenderElementState,
    next: RenderElementState,
) -> RenderElementPresentationState {
    use RenderElementPresentationState::{Rendering, Skipped, ZeroCopy};

    match (current.presentation_state, next.presentation_state) {
        (Skipped, state) => state,
        (state, Skipped) => state,
        (ZeroCopy, ZeroCopy) => ZeroCopy,
        (Rendering { reason }, Rendering { reason: next }) => Rendering {
            reason: reason.or(next),
        },
        (Rendering { reason }, ZeroCopy) | (ZeroCopy, Rendering { reason }) => Rendering { reason },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberEvent {
    pub sequence: u32,
    pub time: Duration,
}

/// Fold member events in member-index order. Only the designated primary member's sequence is used;
/// DRM sequences from different nodes are unrelated. Presentation time is the latest completion.
pub fn aggregate_member_events(
    events: &[Option<MemberEvent>; MAX_GROUP_MEMBERS],
    primary: usize,
) -> Option<MemberEvent> {
    let sequence = events.get(primary)?.as_ref()?.sequence;
    let time = events.iter().flatten().map(|event| event.time).max()?;
    Some(MemberEvent { sequence, time })
}

/// One logical frame in flight. Only members included in `pending_mask` are waited on.
///
/// A single frame intentionally couples group pacing: a slow member stalls the next logical frame.
/// This keeps members from running ahead independently and makes ownership tractable. Watchdog and
/// misprediction thresholds must therefore treat normal EVDI USB jitter (often 10-20 ms) as
/// latency, not a missing-event failure.
#[derive(Debug)]
pub struct PendingGroupFrame<F> {
    frame: F,
    pending_mask: u8,
    events: [Option<MemberEvent>; MAX_GROUP_MEMBERS],
    sequence_floors: [u32; MAX_GROUP_MEMBERS],
}

impl<F> PendingGroupFrame<F> {
    pub fn new(frame: F, submitted_mask: u8, sequence_floors: [u32; MAX_GROUP_MEMBERS]) -> Self {
        let valid_mask = (1_u8 << MAX_GROUP_MEMBERS) - 1;
        assert_ne!(
            submitted_mask, 0,
            "a group frame must submit at least one member"
        );
        assert_eq!(
            submitted_mask & !valid_mask,
            0,
            "member mask exceeds group bound"
        );
        Self {
            frame,
            pending_mask: submitted_mask,
            events: array::from_fn(|_| None),
            sequence_floors,
        }
    }

    pub fn pending_mask(&self) -> u8 {
        self.pending_mask
    }

    /// Record one event. Duplicate, unsubmitted, and pre-submit stale events are ignored.
    pub fn record(&mut self, member: usize, event: MemberEvent) -> bool {
        if !self.accepts(member, event.sequence) {
            return false;
        }
        let bit = 1_u8 << member;
        self.events[member] = Some(event);
        self.pending_mask &= !bit;
        true
    }

    pub fn accepts(&self, member: usize, sequence: u32) -> bool {
        let Some(bit) = 1_u8.checked_shl(member as u32) else {
            return false;
        };
        if member >= MAX_GROUP_MEMBERS
            || self.pending_mask & bit == 0
            || sequence.wrapping_sub(self.sequence_floors[member]) >= (1 << 31)
        {
            return false;
        }
        true
    }

    pub fn finish(self, primary: usize) -> Result<(F, MemberEvent)> {
        ensure!(self.pending_mask == 0, "group frame is still pending");
        let event = aggregate_member_events(&self.events, primary)
            .ok_or_else(|| anyhow::anyhow!("primary member did not participate in this frame"))?;
        Ok((self.frame, event))
    }

    /// Complete a cross-device frame using the logical group sequence. DRM sequence counters from
    /// different nodes are unrelated, so only the latest completion timestamp is aggregated.
    pub fn finish_logical(self, sequence: u32) -> Result<(F, MemberEvent)> {
        ensure!(self.pending_mask == 0, "group frame is still pending");
        let time = self
            .events
            .iter()
            .flatten()
            .map(|event| event.time)
            .max()
            .ok_or_else(|| anyhow::anyhow!("group frame has no member events"))?;
        Ok((self.frame, MemberEvent { sequence, time }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalFrameOutcome {
    Complete,
    PartialFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalFrameDisposition {
    Present { sequence: u32 },
    DiscardPartial,
}

#[derive(Debug)]
pub struct LogicalFrameCompletion<F> {
    pub frame: F,
    pub event: MemberEvent,
    pub disposition: LogicalFrameDisposition,
}

#[derive(Debug)]
pub enum LogicalFrameEvent<F> {
    Rejected,
    Waiting,
    Completed(LogicalFrameCompletion<F>),
}

#[derive(Debug)]
struct LogicalPendingFrame<F> {
    pending: PendingGroupFrame<F>,
    outcome: LogicalFrameOutcome,
}

/// Pure generalized-frame lifecycle used by the host KMS adapter.
///
/// It is the sole owner of the successful logical sequence: partial cross-device submissions wait
/// for every submitted member so buffers remain retained, but can only produce `DiscardPartial`.
/// This keeps truthful presentation behavior deterministic and fault-injectable without DRM.
#[derive(Debug)]
pub struct LogicalFrameLifecycle<F> {
    pending: Option<LogicalPendingFrame<F>>,
    successful_sequence: u32,
}

impl<F> Default for LogicalFrameLifecycle<F> {
    fn default() -> Self {
        Self {
            pending: None,
            successful_sequence: 0,
        }
    }
}

impl<F> LogicalFrameLifecycle<F> {
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn submit(
        &mut self,
        frame: F,
        submitted_mask: u8,
        sequence_floors: [u32; MAX_GROUP_MEMBERS],
        outcome: LogicalFrameOutcome,
    ) {
        assert!(self.pending.is_none(), "a logical frame is already pending");
        self.pending = Some(LogicalPendingFrame {
            pending: PendingGroupFrame::new(frame, submitted_mask, sequence_floors),
            outcome,
        });
    }

    pub fn record(&mut self, member: usize, event: MemberEvent) -> LogicalFrameEvent<F> {
        let Some(pending) = self.pending.as_mut() else {
            return LogicalFrameEvent::Rejected;
        };
        if !pending.pending.record(member, event) {
            return LogicalFrameEvent::Rejected;
        }
        if pending.pending.pending_mask() != 0 {
            return LogicalFrameEvent::Waiting;
        }

        let pending = self.pending.take().unwrap();
        let next = self.successful_sequence.wrapping_add(1);
        let (frame, event) = pending
            .pending
            .finish_logical(next)
            .expect("a zero pending mask must contain at least one event");
        let disposition = match pending.outcome {
            LogicalFrameOutcome::Complete => {
                self.successful_sequence = next;
                LogicalFrameDisposition::Present { sequence: next }
            }
            LogicalFrameOutcome::PartialFailure => LogicalFrameDisposition::DiscardPartial,
        };
        LogicalFrameEvent::Completed(LogicalFrameCompletion {
            frame,
            event,
            disposition,
        })
    }

    pub fn take_pending(&mut self) -> Option<F> {
        self.pending.take().map(|pending| pending.pending.frame)
    }

    pub fn successful_sequence(&self) -> u32 {
        self.successful_sequence
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSubmission {
    pub submitted_mask: u8,
    pub partial_failure: bool,
}

/// The logical result of attempting to submit one display-group frame.
///
/// `PartialFailure` is deliberately distinct from `Complete`: members that were submitted still
/// need their completion events to retire Smithay-owned buffers, but the compositor must not report
/// that physically split frame as one successful logical presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSubmissionOutcome {
    NoSubmission,
    Complete { submitted_mask: u8 },
    PartialFailure { submitted_mask: u8 },
}

impl GroupSubmission {
    pub fn outcome(self) -> GroupSubmissionOutcome {
        match (self.submitted_mask, self.partial_failure) {
            (0, _) => GroupSubmissionOutcome::NoSubmission,
            (submitted_mask, false) => GroupSubmissionOutcome::Complete { submitted_mask },
            (submitted_mask, true) => GroupSubmissionOutcome::PartialFailure { submitted_mask },
        }
    }
}

/// Submit every prepared member in index order and record partial failure explicitly.
///
/// Rendering/KMS integration supplies the callback; keeping the orchestration pure provides a
/// recording seam for failure-order tests without requiring a live DRM device.
pub fn submit_prepared_members(
    member_count: usize,
    mut prepared: impl FnMut(usize) -> bool,
    mut submit: impl FnMut(usize) -> bool,
) -> GroupSubmission {
    debug_assert!(member_count <= MAX_GROUP_MEMBERS);
    let mut submitted_mask = 0_u8;
    let mut partial_failure = false;
    for member in 0..member_count {
        if !prepared(member) {
            continue;
        }
        if submit(member) {
            submitted_mask |= 1 << member;
        } else {
            partial_failure = true;
        }
    }
    GroupSubmission {
        submitted_mask,
        partial_failure,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use smithay::backend::renderer::element::{Id, RenderingReason};

    use super::*;

    proptest! {
        #[test]
        fn horizontal_rectangular_tilings_are_accepted(
            widths in prop::collection::vec(1_u32..4_000, 2..=MAX_GROUP_MEMBERS),
            height in 1_u32..4_000,
        ) {
            let mut x = 0_u32;
            let members = widths
                .iter()
                .map(|&width| {
                    let member = MemberGeometry {
                        size: MemberSize { width, height },
                        position: Some((x, 0)),
                    };
                    x += width;
                    member
                })
                .collect::<Vec<_>>();
            let (size, rects) = group_layout(&members).unwrap();
            prop_assert_eq!(size, MemberSize { width: x, height });
            prop_assert_eq!(rects.len(), widths.len());
            for pair in rects.windows(2) {
                prop_assert_eq!(pair[0].right(), Some(pair[1].x));
            }
        }

        #[test]
        fn vertical_rectangular_tilings_are_accepted(
            heights in prop::collection::vec(1_u32..4_000, 2..=MAX_GROUP_MEMBERS),
            width in 1_u32..4_000,
        ) {
            let mut y = 0_u32;
            let members = heights
                .iter()
                .map(|&height| {
                    let member = MemberGeometry {
                        size: MemberSize { width, height },
                        position: Some((0, y)),
                    };
                    y += height;
                    member
                })
                .collect::<Vec<_>>();
            let (size, rects) = group_layout(&members).unwrap();
            prop_assert_eq!(size, MemberSize { width, height: y });
            prop_assert_eq!(rects.len(), heights.len());
            for pair in rects.windows(2) {
                prop_assert_eq!(pair[0].bottom(), Some(pair[1].y));
            }
        }

        #[test]
        fn four_member_grid_tilings_are_accepted(
            left in 1_u32..2_000,
            right in 1_u32..2_000,
            top in 1_u32..2_000,
            bottom in 1_u32..2_000,
        ) {
            let members = [
                MemberGeometry { size: MemberSize { width: left, height: top }, position: Some((0, 0)) },
                MemberGeometry { size: MemberSize { width: right, height: top }, position: Some((left, 0)) },
                MemberGeometry { size: MemberSize { width: left, height: bottom }, position: Some((0, top)) },
                MemberGeometry { size: MemberSize { width: right, height: bottom }, position: Some((left, top)) },
            ];
            let (size, _) = group_layout(&members).unwrap();
            prop_assert_eq!(size, MemberSize { width: left + right, height: top + bottom });
        }
    }

    #[test]
    fn retirement_tracks_every_partial_clear_order() {
        for member_count in 2..=MAX_GROUP_MEMBERS {
            let full_mask = (1_u8 << member_count) - 1;
            for initially_failed in 0..=full_mask {
                let mut retirement = MemberRetirement::new(member_count);
                for member in 0..member_count {
                    if initially_failed & (1 << member) == 0 {
                        assert!(retirement.confirm(member));
                    }
                }
                assert_eq!(retirement.pending_mask(), initially_failed);
                for member in (0..member_count).rev() {
                    if initially_failed & (1 << member) != 0 {
                        assert!(retirement.confirm(member));
                    }
                }
                assert!(retirement.is_complete());
            }
        }
    }

    #[test]
    fn teardown_reducer_withdraws_once_and_never_releases_partial_ownership() {
        for member_count in 2..=MAX_GROUP_MEMBERS {
            let full_mask = (1_u8 << member_count) - 1;
            for failed_mask in 0..=full_mask {
                let (mut lifecycle, commands) = GroupTeardownLifecycle::begin(member_count);
                assert_eq!(
                    commands,
                    [
                        TeardownCommand::WithdrawOutput,
                        TeardownCommand::EnterQuarantine
                    ]
                );
                assert!(!lifecycle.is_public());
                let mut release_count = 0;
                for member in 0..member_count {
                    if failed_mask & (1 << member) == 0
                        && lifecycle.confirm_retired(member)
                            == Some(TeardownCommand::ReleaseResources)
                    {
                        release_count += 1;
                    }
                }
                assert_eq!(lifecycle.pending_mask(), failed_mask);
                if failed_mask != 0 {
                    assert_eq!(lifecycle.disable_failed(), TeardownCommand::RetryDisable);
                    assert_eq!(release_count, 0);
                }
                for member in (0..member_count).rev() {
                    if failed_mask & (1 << member) != 0
                        && lifecycle.confirm_retired(member)
                            == Some(TeardownCommand::ReleaseResources)
                    {
                        release_count += 1;
                    }
                }
                assert!(lifecycle.is_complete());
                assert_eq!(release_count, 1);
                assert!(!lifecycle.is_public());
            }
        }
    }

    #[test]
    fn formation_journal_rolls_back_every_fault_point_in_reverse() {
        for failed_after in 0..4 {
            let mut journal = FormationJournal::default();
            for member in 0..failed_after {
                journal.record_changed(format!("changed-{member}"));
                journal.record_created(format!("created-{member}"));
            }
            let (changed, created) = journal.rollback();
            let expected = (0..failed_after).rev().collect::<Vec<_>>();
            assert_eq!(
                changed,
                expected
                    .iter()
                    .map(|member| format!("changed-{member}"))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                created,
                expected
                    .iter()
                    .map(|member| format!("created-{member}"))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn gamma_transaction_retains_unknown_nodes_after_rollback_failures() {
        for node_count in 2..=MAX_GROUP_MEMBERS {
            for failed_commit in 1..node_count {
                for rollback_failure_mask in 0_u8..(1 << failed_commit) {
                    let mut transaction = GammaTransaction::new(0..node_count);
                    for node in 0..failed_commit {
                        transaction.mark_new(node);
                    }
                    for node in 0..failed_commit {
                        transaction.mark_rollback(node, rollback_failure_mask & (1 << node) == 0);
                    }
                    assert_eq!(transaction.has_unknown(), rollback_failure_mask != 0);
                    for node in 0..failed_commit {
                        let expected = if rollback_failure_mask & (1 << node) == 0 {
                            GammaNodeState::Old
                        } else {
                            GammaNodeState::Unknown
                        };
                        assert_eq!(transaction.state(node), Some(expected));
                    }
                    for node in failed_commit..node_count {
                        assert_eq!(transaction.state(node), Some(GammaNodeState::Old));
                    }
                }
            }
        }
    }

    #[test]
    fn portrait_pair_auto_layout_is_exact() {
        let members = [
            MemberGeometry {
                size: MemberSize {
                    width: 2160,
                    height: 3840,
                },
                position: None,
            },
            MemberGeometry {
                size: MemberSize {
                    width: 2160,
                    height: 3840,
                },
                position: None,
            },
        ];
        let (size, rects) = group_layout(&members).unwrap();
        assert_eq!(
            size,
            MemberSize {
                width: 4320,
                height: 3840
            }
        );
        assert_eq!(rects[1].x, 2160);
        assert_eq!(
            (f64::from(size.width) / 1.25, f64::from(size.height) / 1.25),
            (3456.0, 3072.0)
        );
    }

    #[test]
    fn explicit_layout_rejects_gaps_overlap_and_shifted_origin() {
        let member = |x| MemberGeometry {
            size: MemberSize {
                width: 100,
                height: 100,
            },
            position: Some((x, 0)),
        };
        assert!(group_layout(&[member(0), member(101)]).is_err());
        assert!(group_layout(&[member(0), member(99)]).is_err());
        assert!(group_layout(&[member(1), member(101)]).is_err());
        assert!(group_layout(&[
            member(0),
            MemberGeometry {
                position: None,
                ..member(100)
            },
        ])
        .is_err());
    }

    #[test]
    fn automatic_layout_partitions_physical_pixels_exactly() {
        let members = [
            MemberGeometry {
                size: MemberSize {
                    width: 2160,
                    height: 3840,
                },
                position: None,
            },
            MemberGeometry {
                size: MemberSize {
                    width: 2160,
                    height: 3840,
                },
                position: None,
            },
        ];
        let (_, rects) = group_layout(&members).unwrap();
        assert_eq!(rects[0].right(), Some(rects[1].x));
        assert_eq!(rects[0].y, rects[1].y);
        assert_eq!(rects[0].height, rects[1].height);
    }

    #[test]
    fn automatic_layout_reports_unequal_heights_directly() {
        let err = group_layout(&[
            MemberGeometry {
                size: MemberSize {
                    width: 100,
                    height: 100,
                },
                position: None,
            },
            MemberGeometry {
                size: MemberSize {
                    width: 100,
                    height: 90,
                },
                position: None,
            },
        ])
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("automatic row layout requires equal member heights"));
    }

    #[test]
    fn strict_refresh_searches_beyond_independent_preferred_modes() {
        let selected = choose_common_refresh(
            &[
                vec![
                    RefreshMode {
                        millihz: 60_000,
                        preferred: true,
                    },
                    RefreshMode {
                        millihz: 59_997,
                        preferred: false,
                    },
                ],
                vec![
                    RefreshMode {
                        millihz: 59_940,
                        preferred: true,
                    },
                    RefreshMode {
                        millihz: 59_997,
                        preferred: false,
                    },
                ],
            ],
            RefreshSync::Strict,
        )
        .unwrap();
        assert_eq!(
            selected.iter().map(|mode| mode.millihz).collect::<Vec<_>>(),
            vec![59_997, 59_997]
        );
        assert!(choose_common_refresh(
            &[
                vec![RefreshMode {
                    millihz: 60_000,
                    preferred: true
                }],
                vec![RefreshMode {
                    millihz: 59_940,
                    preferred: true
                }],
            ],
            RefreshSync::Strict,
        )
        .is_none());
        assert!(choose_common_refresh(
            &[
                vec![RefreshMode {
                    millihz: 60_000,
                    preferred: true
                }],
                vec![RefreshMode {
                    millihz: 59_940,
                    preferred: true
                }],
            ],
            RefreshSync::Relaxed,
        )
        .is_some());
    }

    #[test]
    fn frame_mask_ignores_duplicates_and_stale_events() {
        let mut frame = PendingGroupFrame::new("frame", 0b101, [10, 0, 20, 0]);
        assert!(!frame.record(
            0,
            MemberEvent {
                sequence: 9,
                time: Duration::ZERO
            }
        ));
        assert!(frame.record(
            2,
            MemberEvent {
                sequence: 20,
                time: Duration::from_millis(12)
            }
        ));
        assert!(!frame.record(
            2,
            MemberEvent {
                sequence: 21,
                time: Duration::from_millis(13)
            }
        ));
        assert!(frame.record(
            0,
            MemberEvent {
                sequence: 10,
                time: Duration::from_millis(10)
            }
        ));
        let (value, event) = frame.finish(0).unwrap();
        assert_eq!(value, "frame");
        assert_eq!(event.sequence, 10);
        assert_eq!(event.time, Duration::from_millis(12));

        let wrapping = PendingGroupFrame::new((), 0b1, [u32::MAX, 0, 0, 0]);
        assert!(wrapping.accepts(0, 0));
        assert!(!wrapping.accepts(0, u32::MAX - 1));
    }

    #[test]
    fn crtc_tracker_retires_old_generations_without_authenticating_new_frames() {
        let mut tracker = CrtcEventTracker::default();
        let standalone = tracker.enqueue();
        assert_eq!(standalone.generation, 1);
        tracker.begin_generation();
        let grouped = tracker.enqueue();
        assert_eq!(grouped.generation, 2);
        assert_eq!(
            grouped.sequence_floor,
            standalone.sequence_floor.wrapping_add(1)
        );

        assert_eq!(
            tracker.classify(standalone.sequence_floor),
            CrtcEventClassification::Retire {
                submission: standalone,
                drain: true,
                current_generation: false,
            }
        );
        assert!(tracker.retired(standalone, standalone.sequence_floor));
        assert_eq!(
            tracker.classify(grouped.sequence_floor),
            CrtcEventClassification::Retire {
                submission: grouped,
                drain: true,
                current_generation: true,
            }
        );
        assert!(tracker.retired(grouped, grouped.sequence_floor));
    }

    #[test]
    fn crtc_tracker_rejects_pre_floor_and_handles_wraparound() {
        let mut tracker = CrtcEventTracker::default();
        let first = tracker.enqueue();
        assert!(tracker.retired(first, u32::MAX));
        let wrapped = tracker.enqueue();
        assert_eq!(wrapped.sequence_floor, 0);
        assert_eq!(
            tracker.classify(u32::MAX - 1),
            CrtcEventClassification::Reject
        );
        assert!(matches!(
            tracker.classify(0),
            CrtcEventClassification::Retire { submission, .. } if submission == wrapped
        ));
    }

    #[test]
    fn replacing_compositor_never_drains_old_submission_into_new_owner() {
        let mut tracker = CrtcEventTracker::default();
        let old = tracker.enqueue();
        tracker.replace_compositor();
        let new = tracker.enqueue();
        assert_eq!(
            tracker.classify(old.sequence_floor),
            CrtcEventClassification::Retire {
                submission: old,
                drain: false,
                current_generation: false,
            }
        );
        assert!(tracker.retired(old, old.sequence_floor));
        assert!(matches!(
            tracker.classify(new.sequence_floor),
            CrtcEventClassification::Retire {
                submission,
                drain: true,
                current_generation: true,
            } if submission == new
        ));
    }

    #[test]
    fn rejected_event_without_pending_submission_never_establishes_a_floor() {
        let mut tracker = CrtcEventTracker::default();
        assert_eq!(
            tracker.classify(4_000_000_000),
            CrtcEventClassification::Reject
        );
        assert_eq!(tracker.last_sequence(), None);
        let submission = tracker.enqueue();
        assert_eq!(submission.sequence_floor, 0);
        assert_eq!(tracker.last_sequence(), None);
    }

    #[test]
    fn stale_event_between_current_member_events_cannot_complete_the_frame() {
        let mut lifecycle = LogicalFrameLifecycle::default();
        lifecycle.submit(
            "current",
            0b11,
            [100, 200, 0, 0],
            LogicalFrameOutcome::Complete,
        );
        assert!(matches!(
            lifecycle.record(
                0,
                MemberEvent {
                    sequence: 100,
                    time: Duration::from_millis(1),
                }
            ),
            LogicalFrameEvent::Waiting
        ));
        assert!(matches!(
            lifecycle.record(
                0,
                MemberEvent {
                    sequence: 99,
                    time: Duration::from_millis(2),
                }
            ),
            LogicalFrameEvent::Rejected
        ));
        assert!(lifecycle.has_pending());
        assert!(matches!(
            lifecycle.record(
                1,
                MemberEvent {
                    sequence: 200,
                    time: Duration::from_millis(3),
                }
            ),
            LogicalFrameEvent::Completed(LogicalFrameCompletion {
                disposition: LogicalFrameDisposition::Present { sequence: 1 },
                ..
            })
        ));
    }

    #[test]
    fn abandoning_after_one_member_event_never_presents() {
        let mut lifecycle = LogicalFrameLifecycle::default();
        lifecycle.submit(
            "hot-unplugged",
            0b111,
            [0; MAX_GROUP_MEMBERS],
            LogicalFrameOutcome::Complete,
        );
        assert!(matches!(
            lifecycle.record(
                1,
                MemberEvent {
                    sequence: 1,
                    time: Duration::from_millis(1),
                }
            ),
            LogicalFrameEvent::Waiting
        ));
        assert_eq!(lifecycle.take_pending(), Some("hot-unplugged"));
        assert!(!lifecycle.has_pending());
        assert_eq!(lifecycle.successful_sequence(), 0);
    }

    #[test]
    fn every_complete_member_event_order_presents_exactly_once() {
        fn permutations(values: &mut [usize], start: usize, out: &mut Vec<Vec<usize>>) {
            if start == values.len() {
                out.push(values.to_vec());
                return;
            }
            for idx in start..values.len() {
                values.swap(start, idx);
                permutations(values, start + 1, out);
                values.swap(start, idx);
            }
        }

        for member_count in 2..=MAX_GROUP_MEMBERS {
            let mut members = (0..member_count).collect::<Vec<_>>();
            let mut orders = Vec::new();
            permutations(&mut members, 0, &mut orders);
            for order in orders {
                let mut lifecycle = LogicalFrameLifecycle::default();
                lifecycle.submit(
                    (),
                    (1_u8 << member_count) - 1,
                    [0; MAX_GROUP_MEMBERS],
                    LogicalFrameOutcome::Complete,
                );
                let mut completions = 0;
                for member in order {
                    let result = lifecycle.record(
                        member,
                        MemberEvent {
                            sequence: member as u32,
                            time: Duration::from_millis(member as u64),
                        },
                    );
                    completions += matches!(result, LogicalFrameEvent::Completed(_)) as usize;
                }
                assert_eq!(completions, 1);
                assert_eq!(lifecycle.successful_sequence(), 1);
            }
        }
    }

    #[test]
    fn submission_seam_records_partial_failure_without_waiting_for_unsent_members() {
        let prepared = [true, true, false, true];
        let mut attempted = Vec::new();
        let result = submit_prepared_members(
            prepared.len(),
            |member| prepared[member],
            |member| {
                attempted.push(member);
                member != 1
            },
        );

        assert_eq!(attempted, vec![0, 1, 3]);
        assert_eq!(result.submitted_mask, 0b1001);
        assert!(result.partial_failure);
        assert_eq!(
            result.outcome(),
            GroupSubmissionOutcome::PartialFailure {
                submitted_mask: 0b1001
            }
        );
    }

    #[test]
    fn submission_outcome_is_explicit_for_two_to_four_members_and_every_failure() {
        for member_count in 2..=MAX_GROUP_MEMBERS {
            let complete = submit_prepared_members(member_count, |_| true, |_| true);
            assert_eq!(
                complete.outcome(),
                GroupSubmissionOutcome::Complete {
                    submitted_mask: (1 << member_count) - 1
                }
            );

            for failed in 0..member_count {
                let submission =
                    submit_prepared_members(member_count, |_| true, |member| member != failed);
                let expected_mask = ((1_u8 << member_count) - 1) & !(1 << failed);
                let expected = if expected_mask == 0 {
                    GroupSubmissionOutcome::NoSubmission
                } else {
                    GroupSubmissionOutcome::PartialFailure {
                        submitted_mask: expected_mask,
                    }
                };
                assert_eq!(
                    submission.outcome(),
                    expected,
                    "member_count={member_count}, failed={failed}"
                );
            }
        }

        let none = submit_prepared_members(4, |_| false, |_| unreachable!());
        assert_eq!(none.outcome(), GroupSubmissionOutcome::NoSubmission);
    }

    #[test]
    fn logical_lifecycle_never_presents_partial_submissions() {
        for member_count in 2..=MAX_GROUP_MEMBERS {
            let full_mask = (1_u8 << member_count) - 1;
            for failed in 0..member_count {
                let submitted = full_mask & !(1 << failed);
                let mut lifecycle = LogicalFrameLifecycle::default();
                lifecycle.submit(
                    "partial",
                    submitted,
                    [0; MAX_GROUP_MEMBERS],
                    LogicalFrameOutcome::PartialFailure,
                );
                for member in 0..member_count {
                    if submitted & (1 << member) == 0 {
                        continue;
                    }
                    let event = lifecycle.record(
                        member,
                        MemberEvent {
                            sequence: member as u32 + 1,
                            time: Duration::from_millis(member as u64 + 1),
                        },
                    );
                    if lifecycle.has_pending() {
                        assert!(matches!(event, LogicalFrameEvent::Waiting));
                    } else {
                        assert!(matches!(
                            event,
                            LogicalFrameEvent::Completed(LogicalFrameCompletion {
                                frame: "partial",
                                disposition: LogicalFrameDisposition::DiscardPartial,
                                ..
                            })
                        ));
                    }
                }
                assert_eq!(lifecycle.successful_sequence(), 0);
            }
        }
    }

    #[test]
    fn logical_lifecycle_advances_only_complete_frames_and_rejects_duplicates() {
        let mut lifecycle = LogicalFrameLifecycle::default();
        lifecycle.submit(
            "complete",
            0b11,
            [10, 20, 0, 0],
            LogicalFrameOutcome::Complete,
        );
        assert!(matches!(
            lifecycle.record(
                0,
                MemberEvent {
                    sequence: 10,
                    time: Duration::from_millis(10),
                },
            ),
            LogicalFrameEvent::Waiting
        ));
        assert!(matches!(
            lifecycle.record(
                0,
                MemberEvent {
                    sequence: 11,
                    time: Duration::from_millis(11),
                },
            ),
            LogicalFrameEvent::Rejected
        ));
        let completion = lifecycle.record(
            1,
            MemberEvent {
                sequence: 20,
                time: Duration::from_millis(20),
            },
        );
        assert!(matches!(
            completion,
            LogicalFrameEvent::Completed(LogicalFrameCompletion {
                frame: "complete",
                event: MemberEvent {
                    sequence: 1,
                    time,
                },
                disposition: LogicalFrameDisposition::Present { sequence: 1 },
            }) if time == Duration::from_millis(20)
        ));
        assert_eq!(lifecycle.successful_sequence(), 1);
    }

    #[test]
    fn reported_refresh_uses_slowest_member() {
        let refresh = pacing_refresh_interval(&[
            RefreshMode {
                millihz: 60_000,
                preferred: true,
            },
            RefreshMode {
                millihz: 59_940,
                preferred: true,
            },
        ])
        .unwrap();
        assert_eq!(refresh, Duration::from_secs_f64(1000.0 / 59_940.0));
    }

    #[test]
    fn element_state_merge_requires_every_visible_fragment_to_be_zero_copy() {
        let id = Id::new();
        let mut merged = RenderElementStates::default();
        let mut left = RenderElementStates::default();
        left.states.insert(
            id.clone(),
            RenderElementState {
                visible_area: 100,
                presentation_state: RenderElementPresentationState::ZeroCopy,
                needs_capture: false,
            },
        );
        let mut right = RenderElementStates::default();
        right.states.insert(
            id.clone(),
            RenderElementState {
                visible_area: 20,
                presentation_state: RenderElementPresentationState::Rendering {
                    reason: Some(RenderingReason::ScanoutFailed),
                },
                needs_capture: true,
            },
        );

        merge_render_element_states(&mut merged, &left);
        merge_render_element_states(&mut merged, &right);
        let state = merged.states[&id];
        assert_eq!(state.visible_area, 120);
        assert!(state.needs_capture);
        assert_eq!(
            state.presentation_state,
            RenderElementPresentationState::Rendering {
                reason: Some(RenderingReason::ScanoutFailed),
            }
        );
    }
}
