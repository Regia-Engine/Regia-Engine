// SPDX-License-Identifier: MPL-2.0
//! The montage player: plays [`Montage`]s on a rig over its anim graph.
//!
//! Specification: `docs/specs/17-animation.md` § 4b (ADR-1465). [`MontagePlayer`] is a component
//! on the rig entity (next to its [`crate::Animator`]). Gameplay calls [`MontagePlayer::play`],
//! [`MontagePlayer::jump_to_section`], [`MontagePlayer::on_branch`] and friends; the evaluate
//! system then runs [`MontagePlayer::evaluate`] on the graph's output every frame:
//!
//! 1. each playing instance advances by the frame time × its rate, section by section, stopping
//!    exactly at branching points (where armed actions apply) and following section links;
//!    timeline notifies, windows and the segments' clip notifies fire in timeline order;
//! 2. blend weights move (blend in, automatic or interrupting blend out);
//! 3. every instance's tracks are sampled and blended over the graph pose through their slot
//!    masks, in start order, and the main track's root motion is blended with the graph's;
//! 4. an active motion-warping window corrects the frame's root motion toward its target
//!    ([`crate::warp`]);
//! 5. instances that finished blending out end, and queued montages start.
//!
//! Everything is a pure function of the inputs and the frame time: no wall clock, no
//! randomness, so playback is deterministic.

use std::collections::VecDeque;
use std::sync::Arc;

use regia_core::math::{Quat, Vec3, det};
use regia_ecs::ecs::lifecycle::HookContext;
use regia_ecs::ecs::message::Messages;
use regia_ecs::ecs::world::DeferredWorld;
use regia_ecs::prelude::*;

use crate::clip::{ClipStep, Notify};
use crate::eval::ClipLibrary;
use crate::graph::JointMaskDef;
use crate::montage::{ClipSpan, Montage, MontageLayout, SectionNext, TimeWindow, WarpWindow};
use crate::montage_command::{CommandError, MontageCommand, MontageNetState, TimedMontageCommand};
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use crate::skeleton_slots::MontageIssue;
use crate::state_machine::TransitionCurve;
use crate::sync::SyncLeader;
use crate::warp::{
    NO_ROOT_MOTION, RotationKind, WarpAnchor, WarpInput, WarpTarget, compose_root, inside_part, warp_frame,
    wrap_angle, yaw_of, yaw_toward,
};

/// The slot group of a slot the player does not define.
pub const DEFAULT_SLOT_GROUP: &str = "default";

/// How many events [`MontagePlayer::history`] keeps.
pub const EVENT_HISTORY: usize = 64;

/// Recorded commands kept for [`MontagePlayer::recent_commands`] (spec 34 § 9).
pub const COMMAND_HISTORY: usize = 32;

/// Upper bound on section changes and branching points handled in one frame (a looping section
/// shorter than the frame time loops several times; this caps a degenerate file).
const MAX_STEPS_PER_FRAME: usize = 64;

/// Section ends and branch times closer than this (montage seconds) count as reached.
const TIME_EPSILON: f32 = 1e-5;

/// Minimum blend weight at which an instance's segment clips fire their notifies (the same rule
/// the graph applies to blended clips).
const NOTIFY_MIN_WEIGHT: f32 = 0.5 - 1e-4;

/// A slot: a named place montage tracks play into, with the joints it covers.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotDef {
    /// Slot name (montage tracks name it).
    pub name: String,
    /// Slot group: a montage interrupts the playing montage whose main slot shares its group.
    pub group: String,
    /// Joints the slot covers; `None` = the whole body.
    pub mask: Option<JointMaskDef>,
}

impl SlotDef {
    /// A full-body slot.
    pub fn full_body(name: impl Into<String>, group: impl Into<String>) -> Self {
        SlotDef {
            name: name.into(),
            group: group.into(),
            mask: None,
        }
    }

    /// A slot covering `joints` and their descendants.
    pub fn masked(name: impl Into<String>, group: impl Into<String>, joints: &[&str]) -> Self {
        SlotDef {
            name: name.into(),
            group: group.into(),
            mask: Some(JointMaskDef {
                joints: joints.iter().map(|j| (*j).to_string()).collect(),
                include_children: true,
            }),
        }
    }
}

/// Identifies one play of a montage on one player.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct MontageId(pub u64);

/// Options for [`MontagePlayer::play`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlayOptions {
    /// Rate multiplier on top of the montage's own rate (finite, `>= 0`).
    pub rate: f32,
    /// Section to start in (`None` = the first).
    pub section: Option<String>,
    /// Seconds into the start section to begin at (in reverse: seconds back from its end).
    pub start_time: f32,
    /// Play backward: montage time runs from the end (of `section`, or of the montage) toward
    /// 0 (ADR-1870).
    pub reverse: bool,
    /// Time-stretch: set the play rate so the timeline from the start point to the end (0 in
    /// reverse) takes this many wall seconds, rate curve included (finite, `> 0`; ADR-1870).
    /// Replaces `rate`.
    pub duration: Option<f32>,
}

impl Default for PlayOptions {
    fn default() -> Self {
        PlayOptions {
            rate: 1.0,
            section: None,
            start_time: 0.0,
            reverse: false,
            duration: None,
        }
    }
}

/// Why [`MontagePlayer::try_play`] refused a montage (`docs/specs/34-montage-extensions.md` M5).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlayError {
    /// A track plays a slot the player's slot table lacks (`montage_unknown_slot`). Only
    /// checked when the player has a slot table; with none every slot is full body.
    #[error("montage_unknown_slot: no slot named {0:?}")]
    UnknownSlot(String),
    /// The requested start section does not exist (`montage_unknown_section`).
    #[error("montage_unknown_section: no section named {0:?}")]
    UnknownSection(String),
    /// The play rate was negative or not finite (`montage_bad_rate`).
    #[error("montage_bad_rate: the play rate must be finite and >= 0")]
    BadRate,
    /// The start time was negative, not finite or past the montage's end
    /// (`montage_start_past_end`).
    #[error("montage_start_past_end: the start time is outside the montage")]
    StartPastEnd,
    /// The montage has no main track or its main track has no segments (`montage_no_main_track`).
    #[error("montage_no_main_track: the montage has no main track to play")]
    NoMainTrack,
}

/// What happens when playback reaches an armed branching point.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BranchAction {
    /// Jump to the start of the named section, exactly at the branching point's time.
    JumpTo(String),
    /// End the montage there (it blends out, not interrupted).
    Stop,
}

/// Where an instance is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MontagePhase {
    /// Weight rising toward 1.
    BlendingIn,
    /// Full weight.
    Playing,
    /// Reached the end without an automatic blend-out: holds its last pose until stopped.
    Holding,
    /// Weight falling to 0; the instance ends when it gets there.
    BlendingOut,
}

impl MontagePhase {
    /// Lowercase name (agent protocol).
    pub fn as_str(self) -> &'static str {
        match self {
            MontagePhase::BlendingIn => "blending_in",
            MontagePhase::Playing => "playing",
            MontagePhase::Holding => "holding",
            MontagePhase::BlendingOut => "blending_out",
        }
    }
}

/// A snapshot of one playing montage.
#[derive(Clone, Debug, PartialEq)]
pub struct MontageStatus {
    /// Play id.
    pub id: MontageId,
    /// Montage name.
    pub montage: String,
    /// Current section name.
    pub section: String,
    /// Montage time, seconds.
    pub time: f32,
    /// Montage length (0 until the clips resolve on the first evaluation).
    pub length: f32,
    /// Blend weight.
    pub weight: f32,
    /// Effective rate (montage rate × play rate).
    pub rate: f32,
    /// Phase.
    pub phase: MontagePhase,
    /// Whether the blend-out was an interruption (stop or a newer montage).
    pub interrupted: bool,
    /// Whether montage time runs backward (ADR-1870).
    pub reverse: bool,
}

/// What a [`MontageEvent`] reports.
#[derive(Clone, Debug, PartialEq)]
pub enum MontageEventKind {
    /// The montage started (it may still be blending in).
    Started,
    /// Playback moved to another section (link, jump or branch).
    Section {
        /// Section left.
        from: String,
        /// Section entered.
        to: String,
    },
    /// An instant timeline notify.
    Notify {
        /// Notify name.
        name: String,
        /// Optional payload.
        payload: Option<String>,
    },
    /// A notify window opened.
    WindowBegin {
        /// Window name.
        name: String,
        /// Optional payload.
        payload: Option<String>,
    },
    /// A notify window closed (its end, a jump out of it, or the montage ending).
    WindowEnd {
        /// Window name.
        name: String,
    },
    /// Playback reached a branching point (an armed action has just been applied).
    BranchPoint {
        /// Branching point name.
        name: String,
    },
    /// The blend-out started.
    BlendingOut {
        /// `true` when stopped or replaced by a newer montage in the slot group.
        interrupted: bool,
    },
    /// The montage is fully blended out and gone.
    Ended {
        /// `false` = it completed.
        interrupted: bool,
    },
}

impl MontageEventKind {
    /// Lowercase event name (agent protocol).
    pub fn as_str(&self) -> &'static str {
        match self {
            MontageEventKind::Started => "started",
            MontageEventKind::Section { .. } => "section",
            MontageEventKind::Notify { .. } => "notify",
            MontageEventKind::WindowBegin { .. } => "window_begin",
            MontageEventKind::WindowEnd { .. } => "window_end",
            MontageEventKind::BranchPoint { .. } => "branch_point",
            MontageEventKind::BlendingOut { .. } => "blending_out",
            MontageEventKind::Ended { .. } => "ended",
        }
    }
}

/// One montage event as the player records it.
#[derive(Clone, Debug, PartialEq)]
pub struct MontageRecord {
    /// Play id.
    pub id: MontageId,
    /// Montage name.
    pub montage: String,
    /// What happened.
    pub kind: MontageEventKind,
}

/// A montage event, written as an ECS message by the evaluate system.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct MontageEvent {
    /// The rig entity.
    pub entity: Entity,
    /// Play id.
    pub id: MontageId,
    /// Montage name.
    pub montage: String,
    /// What happened.
    pub kind: MontageEventKind,
}

/// The rig's placement for motion warping: its world origin, rotation and scale, including
/// root motion accumulated but not yet applied by the character controller.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigPlacement {
    /// World position.
    pub position: Vec3,
    /// World rotation.
    pub rotation: Quat,
    /// World scale.
    pub scale: Vec3,
}

impl Default for RigPlacement {
    fn default() -> Self {
        RigPlacement {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

impl RigPlacement {
    /// The placement after the rig moves by the root-motion `delta` (rig object space, scaled
    /// by the rig's scale, the inverse of how a warp turns a world offset into rig space).
    pub(crate) fn after(self, delta: (Vec3, Quat)) -> RigPlacement {
        if delta == crate::warp::NO_ROOT_MOTION {
            return self;
        }
        RigPlacement {
            position: self.position + self.rotation * (self.scale * delta.0),
            rotation: (self.rotation * delta.1).normalize(),
            scale: self.scale,
        }
    }
}

/// What [`MontagePlayer::evaluate`] needs from the rig this frame.
#[derive(Clone, Copy)]
pub struct MontageFrame<'a> {
    /// The rig's skeleton.
    pub skeleton: &'a Skeleton,
    /// The animator's clip library (searched after the player's own).
    pub graph_clips: Option<&'a ClipLibrary>,
    /// Frame time, seconds (already scaled by the animator speed; negative counts as 0).
    pub delta: f32,
    /// The rig's placement (for warping).
    pub rig: RigPlacement,
}

#[derive(Clone, Debug)]
struct Instance {
    id: MontageId,
    montage: Arc<Montage>,
    layout: Option<Arc<MontageLayout>>,
    group: String,
    time: f32,
    section: usize,
    links: Vec<SectionNext>,
    rate: f32,
    phase: MontagePhase,
    blend_elapsed: f32,
    blend_time: f32,
    blend_curve: TransitionCurve,
    blend_from: f32,
    weight: f32,
    interrupted: bool,
    started: bool,
    reached_end: bool,
    open_windows: Vec<usize>,
    branch_actions: Vec<(String, BranchAction)>,
    // This frame: montage time at the start, and whether playback stayed contiguous.
    frame_from: f32,
    contiguous: bool,
    // A jump landed on a new time: the next step includes its start, so a notify or window
    // begin sitting exactly on the landing time fires (audit-gameplay zai H1, ADR-1809).
    landed: bool,
    root: (Vec3, Quat),
    /// Warp window whose remaining authored motion is cached, if this instance is inside one.
    rest_window: Option<usize>,
    /// Authored root motion from that window's start to its end.
    rest_total: (Vec3, Quat),
    /// Authored root motion from that window's start through `rest_hi`.
    rest_through: (Vec3, Quat),
    /// Montage time `rest_through` reaches.
    rest_hi: f32,
    /// Cut times reused by this instance's root-motion walks.
    motion_cuts: Vec<f32>,
    /// Clip spans reused by this instance's root-motion walks and notify collection.
    motion_spans: Vec<ClipSpan>,
    reverse: bool,
    // Reverse start waiting for the layout: (section given, seconds back from its end).
    reverse_start: Option<(Option<usize>, f32)>,
    // Time-stretch waiting for the layout: wall seconds to the end.
    stretch: Option<f32>,
    // Timeline hits and clip spans, reused across steps.
    hits: Vec<Hit>,
    spans: Vec<ClipSpan>,
}

impl Instance {
    fn effective_rate(&self) -> f32 {
        self.rate * self.montage.rate
    }

    fn link(&self, section: usize) -> SectionNext {
        self.links
            .get(section)
            .cloned()
            .unwrap_or_else(|| self.montage.section_next(section))
    }

    fn status(&self) -> MontageStatus {
        MontageStatus {
            id: self.id,
            montage: self.montage.name.clone(),
            section: self.montage.section_name(self.section).to_string(),
            time: self.time,
            length: self.layout.as_ref().map_or(0.0, |l| l.length),
            weight: self.weight,
            rate: self.effective_rate(),
            phase: self.phase,
            interrupted: self.interrupted,
            reverse: self.reverse,
        }
    }

    /// Apply a pending reverse start and time-stretch once the layout is known.
    fn resolve_pending(&mut self) {
        let Some(layout) = self.layout.clone() else {
            return;
        };
        if let Some((section, back)) = self.reverse_start.take() {
            let back = back.max(0.0);
            match section {
                Some(index) => {
                    let start = self.montage.section_start(index);
                    self.time = (layout.section_end(index) - back).max(start);
                    self.section = index;
                }
                None => {
                    self.time = (layout.length - back).max(0.0);
                    self.section = reverse_section_at(&self.montage, self.time);
                }
            }
            self.frame_from = self.time;
        }
        if let Some(seconds) = self.stretch.take() {
            let end = if self.reverse { 0.0 } else { layout.length };
            let wall = wall_time(&self.montage, self.time, end);
            if wall > 0.0 && seconds > 0.0 {
                self.rate = wall / seconds;
            }
        }
    }

    fn begin_blend_out(
        &mut self,
        time: f32,
        curve: TransitionCurve,
        interrupted: bool,
        records: &mut Vec<MontageRecord>,
    ) {
        if self.phase == MontagePhase::BlendingOut {
            return;
        }
        self.phase = MontagePhase::BlendingOut;
        self.blend_elapsed = 0.0;
        self.blend_time = crate::num::finite_or(time, 0.0).max(0.0);
        self.blend_curve = curve;
        self.blend_from = self.weight;
        self.interrupted = interrupted;
        records.push(self.record(MontageEventKind::BlendingOut { interrupted }));
    }

    fn record(&self, kind: MontageEventKind) -> MontageRecord {
        MontageRecord {
            id: self.id,
            montage: self.montage.name.clone(),
            kind,
        }
    }

    /// Jump to section `index` (its start; its end in reverse), closing windows the new time
    /// is outside of.
    fn jump(&mut self, index: usize, records: &mut Vec<MontageRecord>) {
        let from = self.montage.section_name(self.section).to_string();
        let left = self.time;
        self.section = index;
        self.time = match (&self.layout, self.reverse) {
            (Some(layout), true) => layout.section_end(index),
            _ => self.montage.section_start(index),
        };
        self.contiguous = false;
        // Landing where playback already is (a jump to the section that starts at the end just
        // reached) crossed that time already: its notifies fired on the way in.
        self.landed = self.time != left;
        let to = self.montage.section_name(index).to_string();
        records.push(self.record(MontageEventKind::Section { from, to }));
        self.close_windows_outside(records);
    }

    fn close_windows_outside(&mut self, records: &mut Vec<MontageRecord>) {
        let time = self.time;
        let montage = self.montage.clone();
        self.open_windows.retain(|&i| {
            let Some(n) = montage.notifies.get(i) else {
                return false;
            };
            let inside = time >= n.time && time < n.time + n.duration;
            if !inside {
                records.push(MontageRecord {
                    id: self.id,
                    montage: montage.name.clone(),
                    kind: MontageEventKind::WindowEnd { name: n.name.clone() },
                });
            }
            inside
        });
    }

    fn close_all_windows(&mut self, records: &mut Vec<MontageRecord>) {
        for i in std::mem::take(&mut self.open_windows) {
            if let Some(n) = self.montage.notifies.get(i) {
                records.push(self.record(MontageEventKind::WindowEnd { name: n.name.clone() }));
            }
        }
    }

    /// Reached the end of the montage's playback path.
    fn reach_end(&mut self, records: &mut Vec<MontageRecord>) {
        self.reached_end = true;
        if self.montage.auto_blend_out {
            let blend = self.montage.blend_out;
            self.begin_blend_out(blend.time, blend.curve, false, records);
        } else if self.phase != MontagePhase::BlendingOut {
            self.phase = MontagePhase::Holding;
        }
    }

    /// Whether the current section's link leads to the montage's end.
    fn section_leads_to_end(&self) -> bool {
        match self.link(self.section) {
            SectionNext::Stop => true,
            SectionNext::Continue => self.section + 1 >= self.montage.section_count(),
            SectionNext::Jump(_) => false,
        }
    }
}

/// Is `t` inside the half-open step `(a, b]` (or `[a, b]` on an instance's first step)?
fn crossed(t: f32, a: f32, b: f32, include_start: bool) -> bool {
    (t > a || (include_start && t >= a)) && t <= b
}

/// Is `t` inside the backward step from `a` down to `b`: `[b, a)` (or `[b, a]` on an
/// instance's first step)?
fn crossed_back(t: f32, a: f32, b: f32, include_start: bool) -> bool {
    (t < a || (include_start && t <= a)) && t >= b
}

/// The section a reverse instance at `time` is in: the last one starting strictly before it
/// (a time on a boundary belongs to the section that ends there).
fn reverse_section_at(montage: &Montage, time: f32) -> usize {
    montage.sections.iter().rposition(|s| s.start < time).unwrap_or(0)
}

/// Wall seconds the straight timeline from montage time `from` to `to` (either direction)
/// takes at play rate 1: the integral of `1 / (curve(t) × montage rate)`, key to key, by midpoint
/// substeps of at most `WALL_SUBSTEP` montage seconds (ADR-1870).
fn wall_time(montage: &Montage, from: f32, to: f32) -> f32 {
    let span = (to - from).abs();
    let base = montage.rate;
    if !(span.is_finite() && base.is_finite() && base > 0.0) || span == 0.0 {
        return 0.0;
    }
    let Some(curve) = &montage.rate_curve else {
        return span / base;
    };
    let (lo, hi) = (from.min(to), from.max(to));
    // Integrate key to key, so a step curve (constant between keys) is exact.
    let mut cuts: Vec<f32> = vec![lo, hi];
    cuts.extend(curve.keys.iter().map(|k| k.0).filter(|&k| k > lo && k < hi));
    cuts.sort_by(f32::total_cmp);
    let mut wall = 0.0;
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let steps = ((b - a) / WALL_SUBSTEP).ceil().max(1.0);
        let h = (b - a) / steps;
        for i in 0..steps as usize {
            let mid = a + (i as f32 + 0.5) * h;
            wall += h / (curve.rate_at(mid.min(b)) * base);
        }
    }
    wall
}

/// The longest montage-time substep of `wall_time`, seconds.
const WALL_SUBSTEP: f32 = 1.0 / 240.0;

/// Montage seconds a reverse instance covers in `dt` wall seconds going back from `t`, with
/// `base` multiplying the curve (midpoint substeps of at most 1/240 wall seconds).
fn rate_back(curve: &crate::montage::RateCurve, t: f32, dt: f32, base: f32) -> f32 {
    if !(dt.is_finite() && dt > 0.0 && base.is_finite() && base > 0.0) {
        return 0.0;
    }
    let steps = (dt / (1.0 / 240.0)).ceil().max(1.0);
    let h = dt / steps;
    let mut now = t;
    for _ in 0..steps as usize {
        let mid = now - 0.5 * h * base * curve.rate_at(now);
        now -= h * base * curve.rate_at(mid);
    }
    t - now
}

/// The inverse of a root-motion delta: the motion that undoes it.
fn invert_root(delta: (Vec3, Quat)) -> (Vec3, Quat) {
    let inverse = delta.1.inverse();
    (-(inverse * delta.0), inverse)
}

/// `a·(1−w) + b·w` for root-motion deltas (rotation by normalised lerp).
fn lerp_root(a: (Vec3, Quat), b: (Vec3, Quat), w: f32) -> (Vec3, Quat) {
    if w <= 0.0 {
        return a;
    }
    if w >= 1.0 {
        return b;
    }
    (a.0.lerp(b.0, w), a.1.lerp(b.1, w).normalize())
}

/// One slot's resolved joint weights, valid for skeletons with `stamp` and for `mask` (the
/// slots are a pub field: an edited mask must resolve again).
#[derive(Clone, Debug)]
struct MaskEntry {
    name: String,
    stamp: u64,
    mask: JointMaskDef,
    weights: Vec<f32>,
}

/// FNV-1a over the joint count, names, parents and full bind transforms (a hot reload that
/// only fixes bind rotations or reparents a joint must refresh the cached bind pose and masks).
fn skeleton_stamp(skeleton: &Skeleton) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mix = |hash: &mut u64, bytes: &[u8]| {
        for &byte in bytes {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(&mut hash, &(skeleton.len() as u64).to_le_bytes());
    for name in &skeleton.joint_names {
        mix(&mut hash, name.as_bytes());
        mix(&mut hash, &[0xff]);
    }
    for bind in &skeleton.bind_pose {
        for value in bind
            .translation
            .to_array()
            .into_iter()
            .chain(bind.rotation.to_array())
            .chain(bind.scale.to_array())
        {
            mix(&mut hash, &value.to_le_bytes());
        }
    }
    for parent in &skeleton.parents {
        mix(&mut hash, &parent.unwrap_or(u32::MAX).to_le_bytes());
    }
    hash
}

/// Write slot `name`'s per-joint weights into `out`. An undefined or unmasked slot is 1
/// everywhere. A masked slot is copied from `cache` when `stamp` still matches.
fn fill_slot_mask(
    slots: &[SlotDef],
    cache: &mut Vec<MaskEntry>,
    stamp: u64,
    name: &str,
    skeleton: &Skeleton,
    out: &mut Vec<f32>,
) {
    let n = skeleton.len();
    let masked = slots
        .iter()
        .find(|s| s.name == name)
        .and_then(|s| s.mask.as_ref());
    let Some(mask) = masked else {
        out.resize(n, 1.0);
        out.fill(1.0);
        return;
    };
    let slot = cache.iter().position(|e| e.stamp == stamp && e.name == name);
    if let Some(entry) = slot.map(|index| &cache[index])
        && entry.mask == *mask
    {
        out.clear();
        out.extend_from_slice(&entry.weights);
        return;
    }
    let mut weights = Vec::new();
    mask.resolve(&skeleton.joint_names)
        .weights_into(&skeleton.parents, n, &mut weights);
    out.clear();
    out.extend_from_slice(&weights);
    let entry = MaskEntry {
        name: name.to_string(),
        stamp,
        mask: mask.clone(),
        weights,
    };
    match slot {
        Some(index) => cache[index] = entry,
        None => cache.push(entry),
    }
}

/// A cached bone anchor: the rig-space offset of `bone` from the origin at the end of warp
/// window `window`, measured with `montage` and `clips`. Both are compared by identity, so a
/// montage replaced under the same name or a swapped clip library is measured again (keyed by
/// name, the old geometry was served forever; audit-gameplay Medium).
#[derive(Clone, Debug)]
struct AnchorEntry {
    montage: Arc<Montage>,
    clips: Arc<ClipLibrary>,
    window: usize,
    bone: String,
    offset: Vec3,
}

/// One montage track's pose for a graph `Slot` node (spec 34 M9).
#[derive(Clone, Debug, Default)]
pub struct SlotLayer {
    /// The track's slot.
    pub slot: String,
    /// The track's sampled pose (root pinned where the clip extracts root motion).
    pub pose: Pose,
    /// Per-joint weights of the slot's mask (1 everywhere for a full-body slot).
    pub mask: Vec<f32>,
    /// The montage instance's blend weight.
    pub weight: f32,
}

/// The montage poses of one frame, per slot ([`MontagePlayer::slot_layers`]), oldest
/// instance first. Reused between frames.
#[derive(Clone, Debug, Default)]
pub struct SlotLayers {
    layers: Vec<SlotLayer>,
    used: usize,
}

impl SlotLayers {
    /// The layers of slot `slot`, oldest first.
    pub fn of<'a>(&'a self, slot: &'a str) -> impl Iterator<Item = &'a SlotLayer> + 'a {
        self.layers[..self.used].iter().filter(move |l| l.slot == slot)
    }

    /// Every layer this frame.
    pub fn all(&self) -> &[SlotLayer] {
        &self.layers[..self.used]
    }

    /// Whether no montage track plays this frame.
    pub fn is_empty(&self) -> bool {
        self.used == 0
    }

    /// Forget this frame's layers (keeps the pose buffers).
    pub fn clear(&mut self) {
        self.used = 0;
    }
}

/// Plays montages on a rig over its anim graph (`docs/specs/17-animation.md` § 4b).
///
/// Despawning the entity that holds a player cuts its montages like
/// [`MontagePlayer::cancel_all`] and writes their interrupted events as [`MontageEvent`]
/// messages (spec 34 M5, ADR-7216).
#[derive(Component, Clone, Debug, Default)]
#[component(on_despawn = cut_montages_on_despawn)]
pub struct MontagePlayer {
    /// The rig's slots (undefined slots are full body in [`DEFAULT_SLOT_GROUP`]).
    pub slots: Vec<SlotDef>,
    /// Clips the montages use (searched before the animator's graph library).
    pub clips: Arc<ClipLibrary>,
    /// Montages playable by name ([`MontagePlayer::play_named`], the agent protocol).
    pub library: Vec<Arc<Montage>>,
    instances: Vec<Instance>,
    queued: Vec<(MontageId, Arc<Montage>, PlayOptions)>,
    warp_targets: Vec<(String, WarpTarget)>,
    // Bone anchor offsets at the window end (see `AnchorEntry`).
    anchor_cache: Vec<AnchorEntry>,
    next_id: u64,
    pending: Vec<MontageRecord>,
    history: VecDeque<MontageRecord>,
    scratch: Pose,
    sync_leaders: Vec<(String, SyncLeader)>,
    /// Bind pose of the skeleton `bind_stamp` names.
    bind_pose: Pose,
    bind_stamp: u64,
    /// Resolved slot masks, rebuilt when `bind_stamp` changes.
    mask_cache: Vec<MaskEntry>,
    /// Mask row reused by [`MontagePlayer::finish_frame`].
    mask_row: Vec<f32>,
    /// Animation seconds the evaluate system has advanced this player by (spec 34 § 9).
    clock: f64,
    /// Whether method calls are recorded as commands.
    recording: bool,
    /// Commands recorded since the last [`MontagePlayer::take_commands`].
    recorded: Vec<TimedMontageCommand>,
    /// Received commands waiting for their clock, in arrival order by time.
    inbox: Vec<TimedMontageCommand>,
    /// The last [`COMMAND_HISTORY`] commands taken (agent inspection).
    recent_commands: VecDeque<TimedMontageCommand>,
    /// Load warnings of the library montages (spec 34 M1).
    load_issues: Vec<MontageIssue>,
    /// Network bookkeeping of the command stream (ADR-9370).
    net: MontageNetState,
}

/// The `on_despawn` hook of [`MontagePlayer`]: an owner going away cuts its montages (spec 34
/// M5). The events go out as [`MontageEvent`] messages at once, so a waiting caller gets its
/// terminal event the frame the rig despawns.
fn cut_montages_on_despawn(mut world: DeferredWorld<'_>, context: HookContext) {
    let entity = context.entity;
    let records = {
        let Some(mut player) = world.get_mut::<MontagePlayer>(entity) else {
            return;
        };
        if player.instances.is_empty() && player.queued.is_empty() && player.pending.is_empty() {
            return;
        }
        player.cancel_all_unrecorded();
        player.take_events()
    };
    if let Some(mut messages) = world.get_resource_mut::<Messages<MontageEvent>>() {
        for record in records {
            messages.write(MontageEvent {
                entity,
                id: record.id,
                montage: record.montage,
                kind: record.kind,
            });
        }
    }
}

impl MontagePlayer {
    /// A player with these slots and clips.
    pub fn new(slots: Vec<SlotDef>, clips: ClipLibrary) -> Self {
        MontagePlayer {
            slots,
            clips: Arc::new(clips),
            ..Default::default()
        }
    }

    /// Add (or replace by name) a montage in the library.
    ///
    /// A track whose slot the player's slot table lacks is a load warning (spec 34 M1): it is
    /// logged with code `montage_unknown_slot`, kept in [`MontagePlayer::load_issues`], and the
    /// track still plays full body in group [`DEFAULT_SLOT_GROUP`] through
    /// [`MontagePlayer::play`] ([`MontagePlayer::try_play`] refuses it). With no slot table every
    /// slot is full body and nothing is reported.
    pub fn add_montage(&mut self, montage: Montage) -> Arc<Montage> {
        let montage = Arc::new(montage);
        self.library.retain(|m| m.name != montage.name);
        self.anchor_cache.retain(|e| e.montage.name != montage.name);
        self.load_issues
            .retain(|i| !i.detail.starts_with(&issue_prefix(&montage.name)));
        if !self.slots.is_empty() {
            for (track, t) in montage.tracks.iter().enumerate() {
                if self.slots.iter().any(|s| s.name == t.slot) {
                    continue;
                }
                let issue = MontageIssue {
                    code: "montage_unknown_slot",
                    track,
                    detail: format!(
                        "{}track {track} plays unknown slot {:?} (full body in group {DEFAULT_SLOT_GROUP:?})",
                        issue_prefix(&montage.name),
                        t.slot
                    ),
                };
                tracing::warn!(target: "regia_animation", code = issue.code, "{}", issue.detail);
                self.load_issues.push(issue);
            }
        }
        self.library.push(montage.clone());
        montage
    }

    /// The load warnings of the library montages (spec 34 M1): one `montage_unknown_slot` per
    /// track naming a slot the player's table lacks. Replacing a montage replaces its warnings.
    #[must_use]
    pub fn load_issues(&self) -> &[MontageIssue] {
        &self.load_issues
    }

    /// A library montage by name.
    pub fn montage(&self, name: &str) -> Option<&Arc<Montage>> {
        self.library.iter().find(|m| m.name == name)
    }

    /// The slot group of `slot`.
    pub fn slot_group(&self, slot: &str) -> &str {
        self.slots
            .iter()
            .find(|s| s.name == slot)
            .map_or(DEFAULT_SLOT_GROUP, |s| s.group.as_str())
    }

    fn group_of(&self, montage: &Montage) -> String {
        montage
            .tracks
            .first()
            .map_or(DEFAULT_SLOT_GROUP, |t| self.slot_group(&t.slot))
            .to_string()
    }

    fn allocate_id(&mut self) -> MontageId {
        self.next_id += 1;
        MontageId(self.next_id)
    }

    /// Play `montage` now. A playing montage whose main slot is in the same slot group is
    /// interrupted: it blends out over this montage's blend-in time.
    pub fn play(&mut self, montage: Arc<Montage>, options: PlayOptions) -> MontageId {
        self.record(|| MontageCommand::Play {
            montage: montage.name.clone(),
            options: options.clone(),
            queued: false,
        });
        let id = self.allocate_id();
        self.start(id, montage, options);
        id
    }

    /// Like [`MontagePlayer::play`], but checks the request first and never starts anything on
    /// failure. `play` stays as it was: it returns an id even when the montage later fails to
    /// resolve its clips.
    ///
    /// # Errors
    ///
    /// [`PlayError`] naming the first problem found.
    pub fn try_play(&mut self, montage: Arc<Montage>, options: PlayOptions) -> Result<MontageId, PlayError> {
        if montage.tracks.first().is_none_or(|t| t.segments.is_empty()) {
            return Err(PlayError::NoMainTrack);
        }
        if !self.slots.is_empty()
            && let Some(track) = montage
                .tracks
                .iter()
                .find(|t| !self.slots.iter().any(|s| s.name == t.slot))
        {
            return Err(PlayError::UnknownSlot(track.slot.clone()));
        }
        if let Some(section) = &options.section
            && montage.section_index(section).is_none()
        {
            return Err(PlayError::UnknownSection(section.clone()));
        }
        if !(options.rate.is_finite() && options.rate >= 0.0) {
            return Err(PlayError::BadRate);
        }
        if options.duration.is_some_and(|d| !(d.is_finite() && d > 0.0)) {
            return Err(PlayError::BadRate);
        }
        if !(options.start_time.is_finite() && options.start_time >= 0.0) {
            return Err(PlayError::StartPastEnd);
        }
        // The length is known once the clips resolve; a montage whose clips are not loaded yet
        // skips this check (it fails on its first evaluation instead).
        let clips = &self.clips;
        if let Ok(layout) = montage.layout(|name| clips.get(name).cloned()) {
            let section = options
                .section
                .as_deref()
                .and_then(|s| montage.section_index(s))
                .unwrap_or(0);
            let past = match (options.reverse, options.section.is_some()) {
                (false, _) => montage.section_start(section) + options.start_time > layout.length,
                (true, false) => options.start_time > layout.length,
                (true, true) => {
                    options.start_time > layout.section_end(section) - montage.section_start(section)
                }
            };
            if past {
                return Err(PlayError::StartPastEnd);
            }
        }
        Ok(self.play(montage, options))
    }

    /// Cut everything at once, for a rig that is going away: every playing montage emits
    /// `BlendingOut { interrupted: true }` (unless it was already blending out) and
    /// `Ended { interrupted: true }`, every queued montage emits one `Ended { interrupted: true }`
    /// without a `Started`, and nothing plays afterwards. Take the events with
    /// [`MontagePlayer::take_events`].
    pub fn cancel_all(&mut self) {
        self.record(|| MontageCommand::CancelAll);
        self.cancel_all_unrecorded();
    }

    fn cancel_all_unrecorded(&mut self) {
        let mut records = Vec::new();
        for mut instance in std::mem::take(&mut self.instances) {
            instance.begin_blend_out(0.0, TransitionCurve::Linear, true, &mut records);
            instance.close_all_windows(&mut records);
            records.push(instance.record(MontageEventKind::Ended { interrupted: true }));
        }
        self.drop_queued(None, &mut records);
        self.pending.extend(records);
    }

    /// Drop queued montages (all, or the one named) and record the terminal event each owes: a
    /// queued montage never started, so it gets one `Ended { interrupted: true }` and no
    /// `Started`.
    fn drop_queued(&mut self, id: Option<MontageId>, records: &mut Vec<MontageRecord>) {
        let (dropped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queued)
            .into_iter()
            .partition(|(q, _, _)| id.is_none_or(|id| id == *q));
        self.queued = kept;
        for (q, montage, _) in dropped {
            records.push(MontageRecord {
                id: q,
                montage: montage.name.clone(),
                kind: MontageEventKind::Ended { interrupted: true },
            });
        }
    }

    /// Play a library montage by name; `None` when the library has no such montage.
    pub fn play_named(&mut self, name: &str, options: PlayOptions) -> Option<MontageId> {
        let montage = self.montage(name)?.clone();
        Some(self.play(montage, options))
    }

    /// Play `montage` once the slot group's current montage starts its natural blend-out (or
    /// now, when nothing plays in the group).
    pub fn queue(&mut self, montage: Arc<Montage>, options: PlayOptions) -> MontageId {
        self.record(|| MontageCommand::Play {
            montage: montage.name.clone(),
            options: options.clone(),
            queued: true,
        });
        let id = self.allocate_id();
        let group = self.group_of(&montage);
        if self.group_busy(&group) {
            self.queued.push((id, montage, options));
        } else {
            self.start(id, montage, options);
        }
        id
    }

    fn group_busy(&self, group: &str) -> bool {
        self.instances
            .iter()
            .any(|i| i.group == group && i.phase != MontagePhase::BlendingOut)
    }

    fn start(&mut self, id: MontageId, montage: Arc<Montage>, options: PlayOptions) {
        let group = self.group_of(&montage);
        let blend_in = montage.blend_in;
        let mut records = Vec::new();
        for other in &mut self.instances {
            if other.group == group && other.phase != MontagePhase::BlendingOut {
                other.begin_blend_out(blend_in.time, blend_in.curve, true, &mut records);
            }
        }
        let section = options
            .section
            .as_deref()
            .and_then(|s| montage.section_index(s))
            .unwrap_or(0);
        let offset = crate::num::finite_or(options.start_time, 0.0).max(0.0);
        // A reverse start is measured from a section's end, known once the clips resolve.
        let start = if options.reverse {
            montage.section_start(section)
        } else {
            montage.section_start(section) + offset
        };
        let reverse_start = options.reverse.then(|| {
            let given = options.section.as_deref().and_then(|s| montage.section_index(s));
            (given, offset)
        });
        let stretch = options.duration.filter(|d| d.is_finite() && *d > 0.0);
        let rate = crate::num::finite_or(options.rate, 0.0).max(0.0);
        let immediate = blend_in.time.is_nan() || blend_in.time <= 0.0;
        let instance = Instance {
            id,
            montage: montage.clone(),
            layout: None,
            group,
            time: start,
            section: montage.section_at(start).max(section),
            links: (0..montage.section_count())
                .map(|i| montage.section_next(i))
                .collect(),
            rate,
            phase: if immediate {
                MontagePhase::Playing
            } else {
                MontagePhase::BlendingIn
            },
            blend_elapsed: 0.0,
            blend_time: blend_in.time,
            blend_curve: blend_in.curve,
            blend_from: 0.0,
            weight: if immediate { 1.0 } else { 0.0 },
            interrupted: false,
            started: false,
            reached_end: false,
            open_windows: Vec::new(),
            branch_actions: Vec::new(),
            frame_from: start,
            contiguous: true,
            landed: false,
            root: (Vec3::ZERO, Quat::IDENTITY),
            rest_window: None,
            rest_total: NO_ROOT_MOTION,
            rest_through: NO_ROOT_MOTION,
            rest_hi: 0.0,
            motion_cuts: Vec::new(),
            motion_spans: Vec::new(),
            reverse: options.reverse,
            reverse_start,
            stretch,
            hits: Vec::new(),
            spans: Vec::new(),
        };
        records.push(instance.record(MontageEventKind::Started));
        self.instances.push(instance);
        self.pending.extend(records);
    }

    /// The instance `id` names, or (for `None`) the newest one not blending out, else the newest.
    fn target_mut(&mut self, id: Option<MontageId>) -> Option<&mut Instance> {
        match id {
            Some(id) => self.instances.iter_mut().find(|i| i.id == id),
            None => {
                let index = self
                    .instances
                    .iter()
                    .rposition(|i| i.phase != MontagePhase::BlendingOut)
                    .or_else(|| self.instances.len().checked_sub(1))?;
                self.instances.get_mut(index)
            }
        }
    }

    /// Stop montage `id` (or every montage, for `None`): it blends out over `blend` seconds (the
    /// montage's blend-out time for `None`), interrupted. Queued montages of a full stop (or the
    /// one named) are dropped, each with one `Ended { interrupted: true }` and no `Started`.
    /// Returns how many instances started blending out.
    pub fn stop(&mut self, id: Option<MontageId>, blend: Option<f32>) -> usize {
        self.record(|| MontageCommand::Stop { id, blend });
        let mut records = Vec::new();
        let mut count = 0;
        for instance in &mut self.instances {
            if id.is_some_and(|id| id != instance.id) || instance.phase == MontagePhase::BlendingOut {
                continue;
            }
            let settings = instance.montage.blend_out;
            instance.begin_blend_out(blend.unwrap_or(settings.time), settings.curve, true, &mut records);
            count += 1;
        }
        self.drop_queued(id, &mut records);
        self.pending.extend(records);
        count
    }

    /// Jump montage `id` (or the current one) to the start of `section`. Returns `false` when
    /// there is no such montage or section.
    pub fn jump_to_section(&mut self, id: Option<MontageId>, section: &str) -> bool {
        self.record(|| MontageCommand::JumpToSection {
            id,
            section: section.to_owned(),
        });
        let mut records = Vec::new();
        let done = match self.target_mut(id) {
            Some(instance) => match instance.montage.section_index(section) {
                Some(index) => {
                    instance.jump(index, &mut records);
                    instance.reached_end = false;
                    if instance.phase == MontagePhase::Holding {
                        instance.phase = MontagePhase::Playing;
                    }
                    true
                }
                None => false,
            },
            None => false,
        };
        self.pending.extend(records);
        done
    }

    /// Change where section `from` goes at its end, for montage `id` (or the current one).
    /// Returns `false` when there is no such montage or section (or the jump target is unknown).
    pub fn set_next_section(&mut self, id: Option<MontageId>, from: &str, next: SectionNext) -> bool {
        self.record(|| MontageCommand::SetNextSection {
            id,
            from: from.to_owned(),
            next: next.clone(),
        });
        let Some(instance) = self.target_mut(id) else {
            return false;
        };
        let Some(index) = instance.montage.section_index(from) else {
            return false;
        };
        if let SectionNext::Jump(to) = &next
            && instance.montage.section_index(to).is_none()
        {
            return false;
        }
        if let Some(link) = instance.links.get_mut(index) {
            *link = next;
            true
        } else {
            false
        }
    }

    /// Arm `action` for the next time montage `id` (or the current one) reaches branching
    /// point `branch`; it applies at the branching point's exact time, once. Returns `false`
    /// when there is no such montage or branching point.
    pub fn on_branch(&mut self, id: Option<MontageId>, branch: &str, action: BranchAction) -> bool {
        self.record(|| MontageCommand::OnBranch {
            id,
            branch: branch.to_owned(),
            action: action.clone(),
        });
        let Some(instance) = self.target_mut(id) else {
            return false;
        };
        let known = instance
            .montage
            .notifies
            .iter()
            .any(|n| n.branch && n.name == branch);
        let valid = match &action {
            BranchAction::JumpTo(section) => instance.montage.section_index(section).is_some(),
            BranchAction::Stop => true,
        };
        if !known || !valid {
            return false;
        }
        instance.branch_actions.retain(|(name, _)| name != branch);
        instance.branch_actions.push((branch.to_string(), action));
        true
    }

    /// Set the play rate of montage `id` (or the current one); non-finite or negative is 0.
    pub fn set_rate(&mut self, id: Option<MontageId>, rate: f32) -> bool {
        self.record(|| MontageCommand::SetRate { id, rate });
        match self.target_mut(id) {
            Some(instance) => {
                instance.rate = crate::num::finite_or(rate, 0.0).max(0.0);
                true
            }
            None => false,
        }
    }

    /// Play montage `id` (or the current one) backward (`true`) or forward from where it is
    /// (ADR-1870). A montage holding at its end plays again in the new direction. Returns
    /// `false` when there is no such montage.
    pub fn set_reverse(&mut self, id: Option<MontageId>, reverse: bool) -> bool {
        self.record(|| MontageCommand::SetReverse { id, reverse });
        let Some(instance) = self.target_mut(id) else {
            return false;
        };
        if instance.reverse != reverse {
            instance.reverse = reverse;
            if !reverse {
                // A reverse start not placed yet (clips unresolved) starts forward where it is.
                instance.reverse_start = None;
            }
            if instance.reached_end && instance.phase != MontagePhase::BlendingOut {
                instance.reached_end = false;
                instance.phase = MontagePhase::Playing;
            }
            instance.section = if reverse {
                reverse_section_at(&instance.montage, instance.time)
            } else {
                instance.montage.section_at(instance.time)
            };
        }
        true
    }

    /// Time-stretch montage `id` (or the current one): set its play rate so the timeline from
    /// its current time to the end (0 in reverse) takes `seconds` of wall time, rate curve
    /// included (ADR-1870). Returns `false` when there is no such montage or `seconds` is not
    /// finite and positive.
    pub fn stretch_to(&mut self, id: Option<MontageId>, seconds: f32) -> bool {
        self.record(|| MontageCommand::StretchTo { id, seconds });
        if !(seconds.is_finite() && seconds > 0.0) {
            return false;
        }
        let Some(instance) = self.target_mut(id) else {
            return false;
        };
        instance.stretch = Some(seconds);
        if instance.reverse_start.is_none() {
            instance.resolve_pending();
        }
        true
    }

    /// Set (or move) warp target `name`.
    pub fn set_warp_target(&mut self, name: impl Into<String>, target: WarpTarget) {
        let name = name.into();
        self.record(|| MontageCommand::SetWarpTarget {
            name: name.clone(),
            target,
        });
        match self.warp_targets.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = target,
            None => self.warp_targets.push((name, target)),
        }
    }

    /// Remove warp target `name`; returns whether it was set.
    pub fn clear_warp_target(&mut self, name: &str) -> bool {
        self.record(|| MontageCommand::ClearWarpTarget {
            name: name.to_owned(),
        });
        let before = self.warp_targets.len();
        self.warp_targets.retain(|(n, _)| n != name);
        before != self.warp_targets.len()
    }

    /// Warp target `name`, if set.
    pub fn warp_target(&self, name: &str) -> Option<WarpTarget> {
        self.warp_targets.iter().find(|(n, _)| n == name).map(|(_, t)| *t)
    }

    /// Every warp target set.
    pub fn warp_targets(&self) -> &[(String, WarpTarget)] {
        &self.warp_targets
    }

    /// Re-expresses the world-space warp targets after an origin rebase (spec 78 § 12.2).
    pub fn shift_warp_targets(&mut self, shift: Vec3) {
        self.record(|| MontageCommand::ShiftWarpTargets { shift });
        for (_, target) in &mut self.warp_targets {
            target.position -= shift;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Commands (spec 34 § 9, ADR-7216).

    /// Record `command` (built only when recording is on) at the current clock.
    fn record(&mut self, command: impl FnOnce() -> MontageCommand) {
        if self.recording {
            let at = self.clock;
            self.recorded.push(TimedMontageCommand {
                at,
                command: command(),
            });
        }
    }

    /// Turn command recording on or off (spec 34 § 9). While on, every method call that
    /// changes playback is kept as a [`TimedMontageCommand`] until
    /// [`MontagePlayer::take_commands`] (the plugin writes them as `MontageCommandRecorded`).
    pub fn set_recording(&mut self, on: bool) {
        self.recording = on;
    }

    /// Whether command recording is on.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// Drain the commands recorded since the last call, oldest first. The last
    /// [`COMMAND_HISTORY`] stay readable through [`MontagePlayer::recent_commands`].
    pub fn take_commands(&mut self) -> Vec<TimedMontageCommand> {
        let taken = std::mem::take(&mut self.recorded);
        for command in &taken {
            if self.recent_commands.len() >= COMMAND_HISTORY {
                self.recent_commands.pop_front();
            }
            self.recent_commands.push_back(command.clone());
        }
        taken
    }

    /// The last recorded commands taken, oldest first (at most [`COMMAND_HISTORY`]).
    pub fn recent_commands(&self) -> impl Iterator<Item = &TimedMontageCommand> {
        self.recent_commands.iter()
    }

    /// How many received commands wait for their clock.
    #[must_use]
    pub fn waiting_commands(&self) -> usize {
        self.inbox.len()
    }

    /// Whether commands wait in [`MontagePlayer::take_commands`].
    #[must_use]
    pub fn has_recorded_commands(&self) -> bool {
        !self.recorded.is_empty()
    }

    /// The network bookkeeping of this player's command stream (ADR-9370): commands sent and
    /// received over the network, duplicates and late ones dropped, and the stamps that order
    /// them. A player that is not networked keeps zeros.
    #[must_use]
    pub fn net_state(&self) -> &MontageNetState {
        &self.net
    }

    /// Mutable [`MontagePlayer::net_state`] (the network layer stamps and checks commands).
    pub fn net_state_mut(&mut self) -> &mut MontageNetState {
        &mut self.net
    }

    /// Seconds of animation time the evaluate system has advanced this player by; recorded
    /// commands carry it, received commands wait for it.
    #[must_use]
    pub fn clock(&self) -> f64 {
        self.clock
    }

    /// Advance the command clock by one evaluation's animation seconds (the evaluate system
    /// calls this once per evaluation of the rig, playing or not).
    pub fn advance_clock(&mut self, seconds: f32) {
        self.clock += f64::from(crate::num::finite_or(seconds, 0.0));
    }

    /// Apply one command through the method that made it (so a recording player records it
    /// again: a relay keeps its own stream).
    ///
    /// # Errors
    ///
    /// [`CommandError::UnknownMontage`] when a `Play` names a montage the library lacks;
    /// nothing changes then.
    pub fn apply_command(&mut self, command: &MontageCommand) -> Result<(), CommandError> {
        match command {
            MontageCommand::Play {
                montage,
                options,
                queued,
            } => {
                let Some(found) = self.montage(montage).cloned() else {
                    return Err(CommandError::UnknownMontage(montage.clone()));
                };
                if *queued {
                    self.queue(found, options.clone());
                } else {
                    self.play(found, options.clone());
                }
            }
            MontageCommand::Stop { id, blend } => {
                self.stop(*id, *blend);
            }
            MontageCommand::CancelAll => self.cancel_all(),
            MontageCommand::JumpToSection { id, section } => {
                self.jump_to_section(*id, section);
            }
            MontageCommand::SetNextSection { id, from, next } => {
                self.set_next_section(*id, from, next.clone());
            }
            MontageCommand::OnBranch { id, branch, action } => {
                self.on_branch(*id, branch, action.clone());
            }
            MontageCommand::SetRate { id, rate } => {
                self.set_rate(*id, *rate);
            }
            MontageCommand::SetReverse { id, reverse } => {
                self.set_reverse(*id, *reverse);
            }
            MontageCommand::StretchTo { id, seconds } => {
                self.stretch_to(*id, *seconds);
            }
            MontageCommand::SetWarpTarget { name, target } => self.set_warp_target(name.clone(), *target),
            MontageCommand::ClearWarpTarget { name } => {
                self.clear_warp_target(name);
            }
            MontageCommand::ShiftWarpTargets { shift } => self.shift_warp_targets(*shift),
        }
        Ok(())
    }

    /// Queue a received command until the clock reaches its time (stable by time: commands of
    /// one time apply in arrival order).
    pub fn receive_command(&mut self, command: TimedMontageCommand) {
        let at = self.inbox.partition_point(|c| c.at <= command.at);
        self.inbox.insert(at, command);
    }

    /// Whether received commands wait for their time.
    #[must_use]
    pub fn has_inbox(&self) -> bool {
        !self.inbox.is_empty()
    }

    /// Apply every received command whose time has come, in order; returns the refusals.
    pub fn apply_due_commands(&mut self) -> Vec<CommandError> {
        let due = self.inbox.partition_point(|c| c.at <= self.clock);
        let mut errors = Vec::new();
        for command in self.inbox.drain(..due).collect::<Vec<_>>() {
            if let Err(error) = self.apply_command(&command.command) {
                errors.push(error);
            }
        }
        errors
    }

    /// The named float curve on the highest-weight instance that has it, this frame.
    ///
    /// Walks the live instances without allocating. `None` when no instance with weight above
    /// zero has resolved its clips and carries `name`. A montage curve overrides a graph
    /// parameter of the same name for [`crate::foot_ik::FootPlacement`].
    #[must_use]
    pub fn named_curve(&self, name: &str) -> Option<f32> {
        let mut best_weight = -1.0_f32;
        let mut best = None;
        for instance in &self.instances {
            if instance.weight <= 0.0 {
                continue;
            }
            let Some(layout) = instance.layout.as_ref() else {
                continue;
            };
            let Some(value) = layout.sample_curve(name, instance.time) else {
                continue;
            };
            if instance.weight > best_weight {
                best_weight = instance.weight;
                best = Some(value);
            }
        }
        best
    }

    /// Status of every playing (or blending-out) montage, oldest first.
    pub fn instances(&self) -> Vec<MontageStatus> {
        self.instances.iter().map(Instance::status).collect()
    }

    /// Status of montage `id`.
    pub fn status(&self, id: MontageId) -> Option<MontageStatus> {
        self.instances.iter().find(|i| i.id == id).map(Instance::status)
    }

    /// Whether anything plays or is queued.
    pub fn is_active(&self) -> bool {
        !self.instances.is_empty() || !self.queued.is_empty()
    }

    /// Ids of queued montages, in queue order.
    pub fn queued(&self) -> Vec<MontageId> {
        self.queued.iter().map(|(id, _, _)| *id).collect()
    }

    /// The last [`EVENT_HISTORY`] events, oldest first.
    pub fn history(&self) -> impl Iterator<Item = &MontageRecord> {
        self.history.iter()
    }

    /// Sync groups that blending-out montages lead after the last evaluation (their
    /// `sync_group`, the main track's clip and clip time); the evaluate system hands them to the
    /// animator's sync state.
    pub fn sync_leaders(&self) -> &[(String, SyncLeader)] {
        &self.sync_leaders
    }

    /// Drain the events recorded since the last call (the evaluate system writes them as
    /// [`MontageEvent`] messages).
    pub fn take_events(&mut self) -> Vec<MontageRecord> {
        let events = std::mem::take(&mut self.pending);
        for event in &events {
            if self.history.len() >= EVENT_HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(event.clone());
        }
        events
    }

    /// Run one frame over the graph's output: advance, blend into `pose`, combine and warp
    /// `root` (rig object space), and append the segments' clip notifies to `notifies`.
    ///
    /// The same as [`MontagePlayer::begin_frame`] then [`MontagePlayer::finish_frame`] with no
    /// slot consumed by the graph.
    #[tracing::instrument(skip_all, name = "montage_evaluate")]
    pub fn evaluate(
        &mut self,
        frame: &MontageFrame<'_>,
        pose: &mut Pose,
        root: &mut (Vec3, Quat),
        notifies: &mut Vec<Notify>,
    ) {
        if self.begin_frame(frame, notifies) {
            self.finish_frame(frame, pose, root, &[]);
        }
    }

    /// First half of a frame (spec 34 M9, ADR-1870): resolve clips and advance every montage
    /// (sections, notifies, root motion). Returns `false` when nothing plays (queued montages
    /// that can start have started; there is nothing to finish this frame).
    #[tracing::instrument(skip_all, name = "montage_begin_frame")]
    pub fn begin_frame(&mut self, frame: &MontageFrame<'_>, notifies: &mut Vec<Notify>) -> bool {
        self.sync_leaders.clear();
        if self.instances.is_empty() {
            self.start_queued();
            self.prune_anchor_cache();
            return false;
        }
        let dt = crate::num::finite_or(frame.delta, 0.0).max(0.0);
        let mut records = Vec::new();
        // 1. Resolve clips on first use.
        for instance in &mut self.instances {
            if instance.layout.is_some() {
                continue;
            }
            let clips = &self.clips;
            let lookup = |name: &str| {
                clips
                    .get(name)
                    .or_else(|| frame.graph_clips.and_then(|g| g.get(name)))
                    .cloned()
            };
            match instance.montage.layout(lookup) {
                Ok(layout) => {
                    instance.layout = Some(Arc::new(layout));
                    instance.resolve_pending();
                }
                Err(error) => {
                    tracing::warn!(target: "regia_animation", "montage {:?}: {error}", instance.montage.name);
                    // Ends at once, as an interruption.
                    instance.weight = 0.0;
                    instance.begin_blend_out(0.0, TransitionCurve::Linear, true, &mut records);
                }
            }
        }
        // 2. Advance every instance.
        for instance in &mut self.instances {
            advance(instance, dt, &mut records, notifies);
        }
        self.pending.extend(records);
        true
    }

    /// Refresh the cached bind pose when the skeleton's names, parents or bind pose change.
    fn ensure_bind(&mut self, skeleton: &Skeleton) -> u64 {
        let stamp = skeleton_stamp(skeleton);
        if stamp != self.bind_stamp || self.bind_pose.len() != skeleton.len() {
            self.bind_pose = skeleton.bind_pose_pose();
            self.bind_stamp = stamp;
            self.mask_cache.clear();
        }
        stamp
    }

    /// The montage pose of every playing track, per slot, oldest instance first: what a graph
    /// `Slot` node blends over its source (spec 34 M9). Call after
    /// [`MontagePlayer::begin_frame`]; `out` is refilled, reusing its poses.
    ///
    /// Takes `&mut self` so the bind pose and slot masks stay cached on the player.
    #[tracing::instrument(skip_all, name = "montage_slot_layers")]
    pub fn slot_layers(&mut self, frame: &MontageFrame<'_>, out: &mut SlotLayers) {
        out.used = 0;
        let stamp = self.ensure_bind(frame.skeleton);
        let MontagePlayer {
            instances,
            slots,
            bind_pose,
            mask_cache,
            ..
        } = self;
        for instance in instances.iter() {
            let Some(layout) = instance.layout.as_ref() else {
                continue;
            };
            if instance.weight <= 0.0 {
                continue;
            }
            for (track_index, track) in layout.tracks.iter().enumerate() {
                if out.used == out.layers.len() {
                    out.layers.push(SlotLayer::default());
                }
                let layer = &mut out.layers[out.used];
                if !layout.sample(
                    track_index,
                    instance.time,
                    bind_pose,
                    instance.montage.root_motion,
                    &mut layer.pose,
                ) {
                    continue;
                }
                layer.slot.clone_from(&track.slot);
                fill_slot_mask(
                    slots,
                    mask_cache,
                    stamp,
                    &track.slot,
                    frame.skeleton,
                    &mut layer.mask,
                );
                layer.weight = instance.weight;
                out.used += 1;
            }
        }
    }

    /// Second half of a frame: blend every track whose slot is not in `consumed` (slots a graph
    /// `Slot` node already blended) into `pose`, combine and warp `root`, end finished montages
    /// and start queued ones. Root motion is combined for every track, consumed or not.
    #[tracing::instrument(skip_all, name = "montage_finish_frame")]
    pub fn finish_frame(
        &mut self,
        frame: &MontageFrame<'_>,
        pose: &mut Pose,
        root: &mut (Vec3, Quat),
        consumed: &[String],
    ) {
        let dt = crate::num::finite_or(frame.delta, 0.0).max(0.0);
        let mut records = Vec::new();
        // 3. Blend poses and root motion, oldest first.
        let root_joint = frame.skeleton.parents.iter().position(Option::is_none);
        let stamp = self.ensure_bind(frame.skeleton);
        {
            let MontagePlayer {
                instances,
                slots,
                scratch,
                bind_pose,
                mask_cache,
                mask_row,
                ..
            } = self;
            for instance in instances.iter() {
                let Some(layout) = instance.layout.as_ref() else {
                    continue;
                };
                if instance.weight <= 0.0 {
                    continue;
                }
                for (track_index, track) in layout.tracks.iter().enumerate() {
                    fill_slot_mask(slots, mask_cache, stamp, &track.slot, frame.skeleton, mask_row);
                    let in_graph = consumed.contains(&track.slot);
                    if !in_graph {
                        if !layout.sample(
                            track_index,
                            instance.time,
                            bind_pose,
                            instance.montage.root_motion,
                            scratch,
                        ) {
                            continue;
                        }
                        pose.masked_blend_from(scratch, mask_row, instance.weight);
                    }
                    if track_index == 0 && instance.montage.root_motion {
                        let share = root_joint.and_then(|r| mask_row.get(r)).copied().unwrap_or(0.0);
                        *root = lerp_root(*root, instance.root, instance.weight * share);
                    }
                }
            }
        }
        // The warp follows the newest weighted instance whose window covers this frame and has a
        // target. A newer montage whose targeted window is still ahead (or already past) must not
        // take the warp from an older one inside its window (audit-gameplay H3, ADR-1809).
        let warp_source = self
            .instances
            .iter()
            .enumerate()
            .rev()
            // Reverse playback is not warped (spec 34 M8).
            .filter(|(_, i)| i.layout.is_some() && i.weight > 0.0 && !i.reverse)
            .find(|(_, instance)| self.active_warp(instance).is_some())
            .map(|(index, _)| index);
        // A montage blending out leads its sync group, newest last (so it wins).
        for instance in &self.instances {
            let (Some(group), Some(layout)) = (&instance.montage.sync_group, &instance.layout) else {
                continue;
            };
            if instance.reverse || instance.phase != MontagePhase::BlendingOut || instance.weight <= 0.0 {
                continue;
            }
            if let Some((clip, time, rate)) = layout.clip_at(0, instance.time) {
                let leader = SyncLeader {
                    clip: clip.name.clone(),
                    time,
                    speed: rate * instance.effective_rate(),
                    looping: true,
                    duration: clip.duration,
                };
                self.sync_leaders.retain(|(g, _)| g != group);
                self.sync_leaders.push((group.clone(), leader));
            }
        }
        // 4. Motion warping on the final root motion.
        if let Some(index) = warp_source
            && dt > 0.0
        {
            let anchor = self.anchor_offset(index, frame);
            *root = self.warp_index(index, frame, *root, anchor);
        }
        // 5. End finished instances, start queued ones.
        let mut ended = Vec::new();
        self.instances.retain_mut(|instance| {
            let done =
                instance.phase == MontagePhase::BlendingOut && instance.blend_elapsed >= instance.blend_time;
            if done {
                instance.close_all_windows(&mut ended);
                let interrupted = instance.interrupted;
                ended.push(instance.record(MontageEventKind::Ended { interrupted }));
            }
            !done
        });
        records.extend(ended);
        self.pending.extend(records);
        self.start_queued();
        self.prune_anchor_cache();
    }

    /// Drops cached anchors whose montage is no longer playing, queued or in the library, or
    /// that were measured with a clip library the player no longer uses (audit v033b: every
    /// distinct montage ever played kept an entry, and with it an `Arc` of its montage and of
    /// that frame's clip library).
    fn prune_anchor_cache(&mut self) {
        if self.anchor_cache.is_empty() {
            return;
        }
        let (instances, queued, library, clips) = (&self.instances, &self.queued, &self.library, &self.clips);
        self.anchor_cache.retain(|entry| {
            Arc::ptr_eq(&entry.clips, clips)
                && (instances.iter().any(|i| Arc::ptr_eq(&i.montage, &entry.montage))
                    || queued.iter().any(|(_, m, _)| Arc::ptr_eq(m, &entry.montage))
                    || library.iter().any(|m| Arc::ptr_eq(m, &entry.montage)))
        });
    }

    fn start_queued(&mut self) {
        let mut index = 0;
        while index < self.queued.len() {
            let group = self.group_of(&self.queued[index].1);
            if self.group_busy(&group) {
                index += 1;
                continue;
            }
            let (id, montage, options) = self.queued.remove(index);
            self.start(id, montage, options);
        }
    }

    /// The active warp window of `instance` this frame: its index and its target.
    fn active_warp(&self, instance: &Instance) -> Option<(usize, WarpTarget)> {
        let (a, b) = (instance.frame_from, instance.time);
        instance
            .montage
            .warps
            .iter()
            .enumerate()
            // The step and the window share some time. A zero-length window (load-rejected,
            // but a host can build one) never warps.
            .filter(|(_, w)| b.min(w.end) > a.max(w.start))
            .find_map(|(i, w)| self.warp_target(&w.target).map(|t| (i, t)))
    }

    /// The rig-space offset from the origin to the active window's anchor at the window end.
    /// A bone anchor samples the main track once per (montage, window, bone) and caches it.
    fn anchor_offset(&mut self, index: usize, frame: &MontageFrame<'_>) -> Vec3 {
        let Some(instance) = self.instances.get(index) else {
            return Vec3::ZERO;
        };
        let Some((window_index, _)) = self.active_warp(instance) else {
            return Vec3::ZERO;
        };
        let window = &instance.montage.warps[window_index];
        let bone_name = match &window.anchor {
            WarpAnchor::Root => return Vec3::ZERO,
            WarpAnchor::Offset(v) => return Vec3::from_array(*v),
            WarpAnchor::Bone(name) => name.as_str(),
        };
        let montage_arc = Arc::clone(&instance.montage);
        if let Some(cached) = self.anchor_cache.iter().find(|e| {
            Arc::ptr_eq(&e.montage, &montage_arc)
                && Arc::ptr_eq(&e.clips, &self.clips)
                && e.window == window_index
                && e.bone == bone_name
        }) {
            return cached.offset;
        }
        let bone = bone_name.to_string();
        let montage = montage_arc.name.clone();
        let Some(layout) = instance.layout.clone() else {
            return Vec3::ZERO;
        };
        let (end, pin_root) = (window.end, instance.montage.root_motion);
        let offset = bone_offset(frame.skeleton, &layout, end, pin_root, &bone).unwrap_or_else(|| {
            tracing::warn!(
                target: "regia_animation",
                "montage_unknown_anchor_bone: montage {montage:?} window {window_index} names bone {bone:?}, which the skeleton lacks; anchoring on the root"
            );
            Vec3::ZERO
        });
        // One entry per (name, window, bone): a stale measurement of an older version goes.
        self.anchor_cache
            .retain(|e| !(e.montage.name == montage && e.window == window_index && e.bone == bone));
        self.anchor_cache.push(AnchorEntry {
            montage: montage_arc,
            clips: Arc::clone(&self.clips),
            window: window_index,
            bone,
            offset,
        });
        offset
    }

    /// The warped root motion of this frame for instance `index`'s active warp window; `anchor`
    /// is the rig-space offset of the window's anchor from the origin at the window end.
    ///
    /// The remaining authored motion of the window is computed once, when the window is
    /// entered, and then updated by subtracting this step. Later frames of the same window
    /// do not walk the layout again.
    fn warp_index(
        &mut self,
        index: usize,
        frame: &MontageFrame<'_>,
        root: (Vec3, Quat),
        anchor: Vec3,
    ) -> (Vec3, Quat) {
        let Some(prepared) = self.prepare_warp(index) else {
            return root;
        };
        let off = prepared.montage.root_motion_off.as_slice();
        let window = &prepared.montage.warps[prepared.window_index];
        let (mut cuts, mut spans) = {
            let Some(instance) = self.instances.get_mut(index) else {
                return root;
            };
            (
                std::mem::take(&mut instance.motion_cuts),
                std::mem::take(&mut instance.motion_spans),
            )
        };
        let before = if prepared.a < prepared.start {
            prepared
                .layout
                .root_motion_between_reuse(prepared.a, prepared.start, off, &mut cuts, &mut spans)
        } else {
            NO_ROOT_MOTION
        };
        let after = if prepared.b > prepared.end {
            prepared
                .layout
                .root_motion_between_reuse(prepared.end, prepared.b, off, &mut cuts, &mut spans)
        } else {
            NO_ROOT_MOTION
        };
        let split = prepared.a < prepared.start || prepared.b > prepared.end;
        let authored_inside = if split {
            inside_part(prepared.authored, before, after)
        } else {
            prepared.authored
        };
        let rest = {
            let Some(instance) = self.instances.get_mut(index) else {
                return root;
            };
            instance.motion_cuts = cuts;
            instance.motion_spans = spans;
            rest_of_window(
                instance,
                &RestStep {
                    layout: &prepared.layout,
                    off,
                    window_index: prepared.window_index,
                    start: prepared.start,
                    end: prepared.end,
                    lo: prepared.lo,
                    hi: prepared.hi,
                    authored_inside,
                },
            )
        };
        let piece_root = if split {
            inside_part(root, before, after)
        } else {
            root
        };
        let warped = warp_piece(&WarpPiece {
            window,
            target: prepared.target,
            rig: frame.rig.after(before),
            root: piece_root,
            anchor,
            lo: prepared.lo,
            hi: prepared.hi,
            rest,
        });
        if split {
            compose_root(compose_root(before, warped), after)
        } else {
            warped
        }
    }

    /// Copies what `warp_index` needs so the rest cache can update the instance.
    ///
    /// The montage and layout are reference-counted clones, so the returned value does not borrow
    /// the player.
    fn prepare_warp(&self, index: usize) -> Option<PreparedWarp> {
        let instance = self.instances.get(index)?;
        let layout = instance.layout.clone()?;
        if !instance.contiguous {
            return None;
        }
        let (a, b) = (instance.frame_from, instance.time);
        let (window_index, target) = self.active_warp(instance)?;
        let window = &instance.montage.warps[window_index];
        let (lo, hi) = (a.max(window.start), b.min(window.end));
        if hi - lo <= 0.0 {
            return None;
        }
        Some(PreparedWarp {
            montage: Arc::clone(&instance.montage),
            layout,
            window_index,
            target,
            authored: instance.root,
            a,
            b,
            start: window.start,
            end: window.end,
            lo,
            hi,
        })
    }
}

/// Owned inputs of one warp step. The montage `Arc` keeps the window and the off-ranges alive
/// without borrowing the player.
struct PreparedWarp {
    montage: Arc<Montage>,
    layout: Arc<MontageLayout>,
    window_index: usize,
    target: WarpTarget,
    authored: (Vec3, Quat),
    a: f32,
    b: f32,
    start: f32,
    end: f32,
    lo: f32,
    hi: f32,
}

/// One warp step's authored inputs for [`rest_of_window`].
struct RestStep<'a> {
    layout: &'a MontageLayout,
    off: &'a [TimeWindow],
    window_index: usize,
    start: f32,
    end: f32,
    lo: f32,
    hi: f32,
    authored_inside: (Vec3, Quat),
}

/// Remaining authored motion from `hi` to the window end.
///
/// The first step of a window walks `[start, end]` once. Each later step that continues from
/// the cached time composes this step's authored motion onto that total, which is the same
/// integral as walking `[hi, end]` again.
fn rest_of_window(instance: &mut Instance, step: &RestStep<'_>) -> (Vec3, Quat) {
    let continues =
        instance.rest_window == Some(step.window_index) && (instance.rest_hi - step.lo).abs() <= TIME_EPSILON;
    if !continues {
        crate::hot_count::count_root_motion_rest();
        instance.rest_window = Some(step.window_index);
        let mut cuts = std::mem::take(&mut instance.motion_cuts);
        let mut spans = std::mem::take(&mut instance.motion_spans);
        instance.rest_total = step
            .layout
            .root_motion_between_reuse(step.start, step.end, step.off, &mut cuts, &mut spans);
        instance.rest_through = if step.lo > step.start + TIME_EPSILON {
            step.layout
                .root_motion_between_reuse(step.start, step.lo, step.off, &mut cuts, &mut spans)
        } else {
            NO_ROOT_MOTION
        };
        instance.motion_cuts = cuts;
        instance.motion_spans = spans;
    }
    let through = compose_root(instance.rest_through, step.authored_inside);
    let rest = compose_root(invert_root(through), instance.rest_total);
    instance.rest_through = through;
    instance.rest_hi = step.hi;
    rest
}

/// The part of one frame's root motion that lies inside a warp window, and what warping it
/// needs (see `MontagePlayer::warp`).
struct WarpPiece<'a> {
    window: &'a WarpWindow,
    target: WarpTarget,
    /// The rig when the piece begins.
    rig: RigPlacement,
    /// The piece's root motion.
    root: (Vec3, Quat),
    /// Rig-space offset of the window's anchor from the origin at the window end.
    anchor: Vec3,
    /// Montage time the piece starts at (inside the window).
    lo: f32,
    /// Montage time the piece ends at (inside the window).
    hi: f32,
    /// Authored root motion from `hi` to the window end.
    rest: (Vec3, Quat),
}

/// The warped root motion of one [`WarpPiece`].
fn warp_piece(piece: &WarpPiece<'_>) -> (Vec3, Quat) {
    let &WarpPiece {
        window,
        target,
        rig,
        root,
        anchor,
        lo,
        hi,
        rest,
    } = piece;
    let frame_time = hi - lo;
    let rest_time = (window.end - hi).max(0.0);
    let mut offset = target.position - rig.position;
    let scale = Vec3::new(
        if rig.scale.x.abs() > 1e-6 {
            rig.scale.x
        } else {
            1.0
        },
        if rig.scale.y.abs() > 1e-6 {
            rig.scale.y
        } else {
            1.0
        },
        if rig.scale.z.abs() > 1e-6 {
            rig.scale.z
        } else {
            1.0
        },
    );
    let rig_yaw = yaw_of(rig.rotation);
    let desired_yaw = |offset: Vec3| -> Option<f32> {
        let facing = target.facing.map(yaw_of);
        let desired = match window.rotation_kind {
            RotationKind::Auto => facing.or_else(|| yaw_toward(offset)),
            RotationKind::MatchTarget => facing,
            RotationKind::FaceTarget => yaw_toward(offset),
        };
        desired.map(|d| wrap_angle(d - rig_yaw))
    };
    // The anchor, not the origin, lands on the target: aim the origin at the target minus
    // the anchor's offset in the rig's final orientation. The orientation depends on the
    // facing, which depends on the aim, so two passes (error bound in spec 34 § 4).
    if anchor != Vec3::ZERO {
        let authored_yaw = yaw_of(root.1 * rest.1);
        for _ in 0..2 {
            let turn = if window.rotation {
                desired_yaw(offset).unwrap_or(authored_yaw)
            } else {
                authored_yaw
            };
            let final_rotation = rig.rotation * det::quat_from_rotation_y(turn);
            offset = target.position - rig.position - final_rotation * (scale * anchor);
        }
    }
    let local = rig.rotation.inverse() * offset;
    let to_target = local / scale;
    let yaw_to_target = if window.rotation {
        desired_yaw(offset)
    } else {
        None
    };
    let (rotation_frame_time, rotation_rest_time) = if window.rotation_fraction >= 1.0 {
        (frame_time, rest_time)
    } else {
        let rotation_end = window.start + (window.end - window.start) * window.rotation_fraction;
        ((rotation_end.min(hi) - lo).max(0.0), (rotation_end - hi).max(0.0))
    };
    warp_frame(&WarpInput {
        frame: root,
        rest,
        to_target,
        yaw_to_target,
        frame_time,
        rest_time,
        horizontal: window.translation,
        vertical: window.vertical,
        mode: window.mode,
        rotation_frame_time,
        rotation_rest_time,
    })
}

/// The position of joint `bone` relative to the rig origin in the main track's pose at
/// montage time `time`; `None` when the skeleton has no such joint.
fn bone_offset(
    skeleton: &Skeleton,
    layout: &MontageLayout,
    time: f32,
    pin_root: bool,
    bone: &str,
) -> Option<Vec3> {
    let joint = skeleton.joint_index(bone)? as usize;
    let bind = skeleton.bind_pose_pose();
    let mut pose = skeleton.bind_pose_pose();
    layout.sample(0, time, &bind, pin_root, &mut pose);
    skeleton
        .globals(&pose, None)
        .get(joint)
        .map(|m| m.transform_point3(Vec3::ZERO))
}

/// Advance one instance by `dt` seconds: blend weight, then montage time with its sections,
/// branching points, notifies and root motion.
fn advance(instance: &mut Instance, dt: f32, records: &mut Vec<MontageRecord>, notifies: &mut Vec<Notify>) {
    // Blend weight (real time, independent of the play rate).
    match instance.phase {
        MontagePhase::BlendingIn => {
            instance.blend_elapsed += dt;
            let blend = instance.montage.blend_in;
            instance.weight = blend.progress(instance.blend_elapsed);
            if instance.blend_elapsed >= instance.blend_time {
                instance.weight = 1.0;
                instance.phase = MontagePhase::Playing;
            }
        }
        MontagePhase::BlendingOut => {
            instance.blend_elapsed += dt;
            let t = if instance.blend_time > 0.0 {
                instance.blend_elapsed / instance.blend_time
            } else {
                1.0
            };
            instance.weight = instance.blend_from * (1.0 - instance.blend_curve.alpha(t));
        }
        MontagePhase::Playing | MontagePhase::Holding => {}
    }
    instance.frame_from = instance.time;
    instance.contiguous = true;
    instance.root = (Vec3::ZERO, Quat::IDENTITY);
    let Some(layout) = instance.layout.clone() else {
        return;
    };
    if instance.reached_end {
        return;
    }
    if instance.reverse {
        advance_reverse(instance, &layout, dt, records, notifies);
        return;
    }
    let montage = instance.montage.clone();
    let mut remaining = match &montage.rate_curve {
        Some(curve) => curve.advance(instance.time, dt, instance.effective_rate()),
        None => dt * instance.effective_rate(),
    };
    let mut first = !instance.started;
    instance.started = true;
    for _ in 0..MAX_STEPS_PER_FRAME {
        if std::mem::take(&mut instance.landed) {
            first = true;
        }
        let section_end = layout.section_end(instance.section);
        let goal = (instance.time + remaining).min(section_end);
        let from = instance.time;
        // The earliest branching point this step reaches.
        let branch = montage
            .notifies
            .iter()
            .enumerate()
            .filter(|(_, n)| n.branch && crossed(n.time, from, goal, first))
            .min_by(|a, b| a.1.time.total_cmp(&b.1.time));
        let piece_end = branch.map_or(goal, |(_, n)| n.time);
        play_piece(instance, &layout, from, piece_end, first, records, notifies);
        remaining = (remaining - (piece_end - from)).max(0.0);
        instance.time = piece_end;
        first = false;
        // Start the automatic blend-out `lead` seconds before an end the path leads to.
        if instance.montage.auto_blend_out && instance.section_leads_to_end() {
            let blend = instance.montage.blend_out;
            let lead = instance.montage.blend_out_lead.unwrap_or(blend.time);
            if instance.time >= section_end - lead - TIME_EPSILON
                && instance.phase != MontagePhase::BlendingOut
            {
                instance.begin_blend_out(blend.time, blend.curve, false, records);
            }
        }
        if let Some((_, notify)) = branch {
            records.push(instance.record(MontageEventKind::BranchPoint {
                name: notify.name.clone(),
            }));
            let armed = instance
                .branch_actions
                .iter()
                .position(|(name, _)| *name == notify.name)
                .map(|i| instance.branch_actions.remove(i).1);
            match armed {
                Some(BranchAction::JumpTo(section)) => {
                    if let Some(index) = montage.section_index(&section) {
                        instance.jump(index, records);
                    }
                }
                Some(BranchAction::Stop) => {
                    instance.reach_end(records);
                    return;
                }
                None => {}
            }
            // Continue after the branching point: it is not crossed again (the next step is
            // `(time, ...]`).
            continue;
        }
        if instance.time < section_end - TIME_EPSILON {
            break; // the frame's time is used up inside the section
        }
        match instance.link(instance.section) {
            SectionNext::Continue if instance.section + 1 < montage.section_count() => {
                let from = montage.section_name(instance.section).to_string();
                instance.section += 1;
                let to = montage.section_name(instance.section).to_string();
                records.push(instance.record(MontageEventKind::Section { from, to }));
            }
            SectionNext::Continue | SectionNext::Stop => {
                instance.time = instance.time.min(layout.length);
                instance.reach_end(records);
                return;
            }
            SectionNext::Jump(target) => {
                let Some(index) = montage.section_index(&target) else {
                    instance.reach_end(records);
                    return;
                };
                let length = layout.section_end(index) - montage.section_start(index);
                if length <= TIME_EPSILON {
                    instance.reach_end(records);
                    return;
                }
                instance.jump(index, records);
            }
        }
        if remaining <= 0.0 {
            break;
        }
    }
}

/// Advance a reverse instance by `dt` seconds (ADR-1870): montage time runs toward 0, sections
/// are crossed backward in timeline order (their forward links are not taken), branching
/// points stop the step exactly, and the montage ends at 0.
fn advance_reverse(
    instance: &mut Instance,
    layout: &MontageLayout,
    dt: f32,
    records: &mut Vec<MontageRecord>,
    notifies: &mut Vec<Notify>,
) {
    let montage = instance.montage.clone();
    let mut remaining = match &montage.rate_curve {
        Some(curve) => rate_back(curve, instance.time, dt, instance.effective_rate()),
        None => dt * instance.effective_rate(),
    };
    let mut first = !instance.started;
    instance.started = true;
    for _ in 0..MAX_STEPS_PER_FRAME {
        let section_start = montage.section_start(instance.section);
        let goal = (instance.time - remaining).max(section_start);
        let from = instance.time;
        // The latest branching point this backward step reaches.
        let branch = montage
            .notifies
            .iter()
            .filter(|n| n.branch && crossed_back(n.time, from, goal, first))
            .max_by(|a, b| a.time.total_cmp(&b.time));
        let piece_end = branch.map_or(goal, |n| n.time);
        play_piece_reverse(instance, layout, from, piece_end, first, records, notifies);
        remaining = (remaining - (from - piece_end)).max(0.0);
        instance.time = piece_end;
        first = false;
        // The automatic blend-out starts `lead` seconds before time 0.
        if montage.auto_blend_out && instance.section == 0 {
            let blend = montage.blend_out;
            let lead = montage.blend_out_lead.unwrap_or(blend.time);
            if instance.time <= lead + TIME_EPSILON && instance.phase != MontagePhase::BlendingOut {
                instance.begin_blend_out(blend.time, blend.curve, false, records);
            }
        }
        if let Some(notify) = branch {
            records.push(instance.record(MontageEventKind::BranchPoint {
                name: notify.name.clone(),
            }));
            let armed = instance
                .branch_actions
                .iter()
                .position(|(name, _)| *name == notify.name)
                .map(|i| instance.branch_actions.remove(i).1);
            match armed {
                Some(BranchAction::JumpTo(section)) => {
                    if let Some(index) = montage.section_index(&section) {
                        instance.jump(index, records);
                    }
                }
                Some(BranchAction::Stop) => {
                    instance.reach_end(records);
                    return;
                }
                None => {}
            }
            continue;
        }
        if instance.time > section_start + TIME_EPSILON {
            break;
        }
        if instance.section == 0 {
            instance.time = 0.0;
            instance.reach_end(records);
            return;
        }
        let from_name = montage.section_name(instance.section).to_string();
        instance.section -= 1;
        let to = montage.section_name(instance.section).to_string();
        records.push(instance.record(MontageEventKind::Section { from: from_name, to }));
        if remaining <= 0.0 {
            break;
        }
    }
}

/// `play_piece` for a backward step from `a` down to `b`: notifies and windows crossed in
/// reverse time order (a window opens at its end edge and closes at its start), the segments'
/// clip notifies as reverse clip steps, and the inverse of the forward root motion.
fn play_piece_reverse(
    instance: &mut Instance,
    layout: &MontageLayout,
    a: f32,
    b: f32,
    first: bool,
    records: &mut Vec<MontageRecord>,
    notifies: &mut Vec<Notify>,
) {
    if b > a {
        return;
    }
    let montage = instance.montage.clone();
    let mut hits = std::mem::take(&mut instance.hits);
    hits.clear();
    for (index, notify) in montage.notifies.iter().enumerate() {
        if notify.branch {
            continue;
        }
        if notify.duration <= 0.0 {
            if crossed_back(notify.time, a, b, first) {
                hits.push(Hit {
                    time: notify.time,
                    kind: MontageEventKind::Notify {
                        name: notify.name.clone(),
                        payload: notify.payload.clone(),
                    },
                    window: None,
                });
            }
            continue;
        }
        let end = notify.time + notify.duration;
        if crossed_back(end, a, b, first) && !instance.open_windows.contains(&index) {
            hits.push(Hit {
                time: end,
                kind: MontageEventKind::WindowBegin {
                    name: notify.name.clone(),
                    payload: notify.payload.clone(),
                },
                window: Some((index, true)),
            });
        }
        if crossed_back(notify.time, a, b, false)
            && (instance.open_windows.contains(&index) || crossed_back(end, a, b, first))
        {
            hits.push(Hit {
                time: notify.time,
                kind: MontageEventKind::WindowEnd {
                    name: notify.name.clone(),
                },
                window: Some((index, false)),
            });
        }
    }
    // Latest first; on a tie a window's begin (its end edge) comes before its end.
    hits.sort_by(|x, y| y.time.total_cmp(&x.time));
    for Hit { kind, window, .. } in hits.drain(..) {
        match window {
            Some((index, true)) => instance.open_windows.push(index),
            Some((index, false)) => instance.open_windows.retain(|&i| i != index),
            None => {}
        }
        records.push(instance.record(kind));
    }
    instance.hits = hits;
    if b >= a {
        return;
    }
    let mut cuts = std::mem::take(&mut instance.motion_cuts);
    let mut spans = std::mem::take(&mut instance.motion_spans);
    if instance.weight >= NOTIFY_MIN_WEIGHT {
        let mut spans = std::mem::take(&mut instance.spans);
        for track in 0..layout.tracks.len() {
            spans.clear();
            layout.spans(track, b, a, &mut spans);
            let last = spans.len().saturating_sub(1);
            for (i, span) in spans.iter().enumerate().rev() {
                let step = ClipStep {
                    prev: span.to,
                    time: span.from,
                    loops: 0,
                };
                span.clip.notifies_in_step(&step, first && i == last, notifies);
            }
        }
        instance.spans = spans;
    }
    if montage.root_motion {
        let delta = invert_root(layout.root_motion_between_reuse(
            b,
            a,
            &montage.root_motion_off,
            &mut cuts,
            &mut spans,
        ));
        let acc = instance.root;
        instance.root = (acc.0 + acc.1 * delta.0, (acc.1 * delta.1).normalize());
    }
    instance.motion_cuts = cuts;
    instance.motion_spans = spans;
}

/// One timeline event a step crosses: its time, the event, and the window it opens (`true`) or
/// closes (`false`).
#[derive(Clone, Debug)]
struct Hit {
    time: f32,
    kind: MontageEventKind,
    window: Option<(usize, bool)>,
}

/// Fire what the step from `a` to `b` crosses (montage notifies and windows, the segments'
/// clip notifies) and add the main track's root motion.
fn play_piece(
    instance: &mut Instance,
    layout: &MontageLayout,
    a: f32,
    b: f32,
    first: bool,
    records: &mut Vec<MontageRecord>,
    notifies: &mut Vec<Notify>,
) {
    if b < a {
        return;
    }
    let montage = instance.montage.clone();
    // Timeline notifies and window edges, in time order (declaration order on ties).
    let mut hits: Vec<Hit> = Vec::new();
    for (index, notify) in montage.notifies.iter().enumerate() {
        if notify.branch {
            continue;
        }
        if notify.duration <= 0.0 {
            if crossed(notify.time, a, b, first) {
                hits.push(Hit {
                    time: notify.time,
                    kind: MontageEventKind::Notify {
                        name: notify.name.clone(),
                        payload: notify.payload.clone(),
                    },
                    window: None,
                });
            }
            continue;
        }
        let end = notify.time + notify.duration;
        if crossed(notify.time, a, b, first) && !instance.open_windows.contains(&index) {
            hits.push(Hit {
                time: notify.time,
                kind: MontageEventKind::WindowBegin {
                    name: notify.name.clone(),
                    payload: notify.payload.clone(),
                },
                window: Some((index, true)),
            });
        }
        if crossed(end, a, b, false)
            && (instance.open_windows.contains(&index) || crossed(notify.time, a, b, first))
        {
            hits.push(Hit {
                time: end,
                kind: MontageEventKind::WindowEnd {
                    name: notify.name.clone(),
                },
                window: Some((index, false)),
            });
        }
    }
    hits.sort_by(|x, y| x.time.total_cmp(&y.time));
    for Hit { kind, window, .. } in hits.drain(..) {
        match window {
            Some((index, true)) => instance.open_windows.push(index),
            Some((index, false)) => instance.open_windows.retain(|&i| i != index),
            None => {}
        }
        records.push(instance.record(kind));
    }
    instance.hits = hits;
    if b <= a {
        return;
    }
    // The segments' own clip notifies, while the montage dominates the pose.
    let mut cuts = std::mem::take(&mut instance.motion_cuts);
    let mut spans = std::mem::take(&mut instance.motion_spans);
    if instance.weight >= NOTIFY_MIN_WEIGHT {
        let mut spans = std::mem::take(&mut instance.spans);
        for track in 0..layout.tracks.len() {
            spans.clear();
            layout.spans(track, a, b, &mut spans);
            for (i, span) in spans.iter().enumerate() {
                let step = ClipStep {
                    prev: span.from,
                    time: span.to,
                    loops: 0,
                };
                span.clip
                    .notifies_in_step(&step, span.entered || (first && i == 0), notifies);
            }
        }
        instance.spans = spans;
    }
    if montage.root_motion {
        let delta = layout.root_motion_between_reuse(a, b, &montage.root_motion_off, &mut cuts, &mut spans);
        let acc = instance.root;
        instance.root = (acc.0 + acc.1 * delta.0, (acc.1 * delta.1).normalize());
    }
    instance.motion_cuts = cuts;
    instance.motion_spans = spans;
}

#[cfg(test)]
mod rebase_tests {
    use super::*;

    #[test]
    fn a_rebase_moves_the_warp_targets_with_everything_else() {
        let mut player = MontagePlayer::default();
        player.set_warp_target("ledge", WarpTarget::at(Vec3::new(5000.0, 2.0, 0.0)));
        player.shift_warp_targets(Vec3::new(4096.0, 0.0, 0.0));
        let target = player.warp_target("ledge").expect("target");
        assert_eq!(target.position, Vec3::new(904.0, 2.0, 0.0));
    }
}

#[cfg(test)]
mod scratch_tests {
    use super::*;
    use crate::clip::{AnimationClip, JointTrack, TrackCurve};
    use crate::pose::translated;
    use crate::skeleton::LocalBind;

    #[test]
    fn playing_montage_pose_is_stable_across_reused_masks() {
        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "arm".into()],
            vec![None, Some(0)],
            vec![LocalBind::default(), translated(Vec3::new(0.0, 1.0, 0.0))],
        )
        .expect("skeleton");
        let mut clip = AnimationClip::empty("swing", 1.0);
        clip.tracks.push(JointTrack {
            joint: 1,
            translations: TrackCurve::Static(Vec3::new(3.0, 1.0, 0.0)),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let mut library = ClipLibrary::new();
        library.insert(clip);
        let mut montage = Montage::single("strike", "upper", "swing");
        montage.blend_in.time = 0.0;
        montage.blend_out.time = 0.0;
        montage.root_motion = false;
        let mut player = MontagePlayer::new(vec![SlotDef::masked("upper", "full", &["arm"])], library);
        player.play(Arc::new(montage), PlayOptions::default());
        let frame = MontageFrame {
            skeleton: &skeleton,
            graph_clips: None,
            delta: 1.0 / 60.0,
            rig: RigPlacement::default(),
        };
        let mut pose = Pose::from_identity(skeleton.len());
        let mut root = (Vec3::ZERO, Quat::IDENTITY);
        let mut notifies = Vec::new();
        let mut caps = None;
        for step in 0..8 {
            player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
            assert_eq!(pose.translations[0], Vec3::ZERO);
            assert!(pose.translations[1].abs_diff_eq(Vec3::new(3.0, 1.0, 0.0), 1e-5));
            assert_eq!(player.mask_row.len(), skeleton.len());
            assert_eq!(player.mask_row[0], 0.0);
            assert_eq!(player.mask_row[1], 1.0);
            assert_eq!(player.mask_cache.len(), 1);
            assert!(player.bind_pose.translations.capacity() >= skeleton.len());
            assert!(player.mask_row.capacity() >= skeleton.len());
            if step == 1 {
                caps = Some((
                    player.bind_pose.translations.capacity(),
                    player.mask_row.capacity(),
                    player.instances[0].hits.capacity(),
                    player.instances[0].spans.capacity(),
                ));
            }
        }
        assert_eq!(
            (
                player.bind_pose.translations.capacity(),
                player.mask_row.capacity(),
                player.instances[0].hits.capacity(),
                player.instances[0].spans.capacity(),
            ),
            caps.expect("step 2")
        );

        // Audit (corr-animation): `slots` is a pub field; an edited mask was served from the
        // cache keyed by skeleton stamp and slot name only.
        player.slots[0].mask = Some(JointMaskDef {
            joints: vec!["root".into()],
            include_children: false,
        });
        player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
        assert_eq!(player.mask_row, vec![1.0, 0.0]);
        assert_eq!(player.mask_cache.len(), 1);

        // Audit (corr-animation): the bind-pose stamp left out rotations, so a skeleton fixing
        // only a bind rotation kept the stale cached bind pose.
        let turned = Quat::from_rotation_z(0.5);
        let mut rebound = skeleton.clone();
        rebound.bind_pose[0].rotation = turned;
        let frame = MontageFrame {
            skeleton: &rebound,
            graph_clips: None,
            delta: 1.0 / 60.0,
            rig: RigPlacement::default(),
        };
        player.evaluate(&frame, &mut pose, &mut root, &mut notifies);
        assert!(player.bind_pose.rotations[0].abs_diff_eq(turned, 1e-6));
    }
}

/// The `detail` prefix of a montage's load issues (they are replaced with the montage).
fn issue_prefix(name: &str) -> String {
    format!("montage {name:?} ")
}
