// SPDX-License-Identifier: MPL-2.0
//! Offline pose search for motion matching.
//!
//! A [`FeatureDatabase`] stores one feature row per sampled frame of a library of
//! [`AnimationClip`]s. Each row holds root-relative joint positions and velocities, then
//! future root trajectory samples (offset in the current root frame, and relative yaw as
//! sine and cosine). Rows are normalised per channel (mean and population standard
//! deviation) and scaled by weights. Nearest neighbour is squared Euclidean distance.
//! [`SearchMethod::BruteForce`] and [`SearchMethod::KdTree`] return the same frame,
//! including ties, which keep the lowest frame index.
//!
//! Trajectory offsets are metres in the frame of the root's position and rotation (its scale is
//! left out, so a centimetre rig needs no conversion).
//!
//! [`SearchState`] plays the chosen clip. It will not jump to another frame of that clip
//! unless the candidate is strictly cheaper than continuing by more than
//! [`SearchState::replay_threshold`]. A different clip is selected immediately, and so is any
//! frame once a non-looping clip has played to its end. The
//! resulting [`PoseSelection`] is the `(clip, time)` pair [`PoseSelection::begin_blend`]
//! hands to the existing [`Inertializer`].
//!
//! Sources, re-derived here (no engine source): Simon Clavet, "Motion Matching and The
//! Road to Next-Gen Animation", GDC 2016; Daniel Holden, Oussama Kanoun, Maksym Perepichka
//! and Tiberiu Popa, "Learned Motion Matching", SIGGRAPH 2020; Jon Louis Bentley,
//! "Multidimensional Binary Search Trees Used for Associative Searching", CACM 18(9), 1975.
//! Character forward is local −Z, matching [`crate::ik`].

use std::f32::consts::{PI, TAU};

use regia_core::math::{Quat, Vec3};

use crate::clip::{AnimationClip, RootMotionMode};
use crate::inertialize::{DecayMethod, Inertializer, Switch};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Largest number of frames a [`FeatureDatabase`] will store.
pub const MAX_DATABASE_FRAMES: usize = 100_000;

/// Smallest accepted sampling interval, in seconds.
pub const MIN_SAMPLE_INTERVAL: f32 = 1.0e-4;

/// Channels whose population standard deviation is at or below this are treated as constant.
///
/// The divisor used for those channels is 1, so a database row normalises to 0 and a query
/// keeps its centred value `(query - mean)`.
pub const DEVIATION_EPSILON: f32 = 1.0e-8;

const NONE: u32 = u32::MAX;
const TIME_EPS: f32 = 1.0e-4;

/// Failure while building or querying a pose-search database.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PoseSearchError {
    /// [`FeatureDatabase::build`] or [`FeatureDatabase::from_raw`] was given no clips.
    #[error("pose search needs at least one clip")]
    NoClips,
    /// The skeleton has no joints, so there is no root to measure from.
    #[error("skeleton has no joints")]
    EmptySkeleton,
    /// Sampling interval was missing, non-finite, or smaller than [`MIN_SAMPLE_INTERVAL`].
    #[error(
        "sample interval {interval} must be finite and at least {minimum}",
        minimum = MIN_SAMPLE_INTERVAL
    )]
    BadSampleInterval {
        /// The rejected interval, in seconds.
        interval: f32,
    },
    /// Velocity window was non-finite or negative. Zero means "use the sample interval".
    #[error("velocity window {window} must be finite and >= 0")]
    BadVelocityWindow {
        /// The rejected window, in seconds.
        window: f32,
    },
    /// [`PoseSearchConfig::root_joint`] is outside the skeleton.
    #[error("root joint {joint} is outside the skeleton ({joint_count} joints)")]
    RootOutOfRange {
        /// The rejected joint id.
        joint: u32,
        /// Number of joints in the skeleton.
        joint_count: usize,
    },
    /// A feature joint id is outside the skeleton.
    #[error("feature joint {joint} is outside the skeleton ({joint_count} joints)")]
    JointOutOfRange {
        /// The rejected joint id.
        joint: u32,
        /// Number of joints in the skeleton.
        joint_count: usize,
    },
    /// The same joint was listed twice in [`PoseSearchConfig::feature_joints`].
    #[error("feature joint {joint} is listed more than once")]
    DuplicateJoint {
        /// The duplicated joint id.
        joint: u32,
    },
    /// A trajectory horizon was negative or non-finite.
    #[error("trajectory horizon {index} ({seconds} s) must be finite and >= 0")]
    BadHorizon {
        /// Index in [`PoseSearchConfig::trajectory_horizons`].
        index: usize,
        /// The rejected horizon, in seconds.
        seconds: f32,
    },
    /// The layout has neither feature joints nor trajectory horizons.
    #[error("pose search has no feature channels")]
    NoFeatures,
    /// A group weight was negative or non-finite.
    #[error("weight {name} must be finite and >= 0")]
    BadWeight {
        /// Which group weight failed (`position`, `velocity`, `trajectory_position`, `trajectory_facing`).
        name: &'static str,
    },
    /// One entry of a per-channel weight vector was negative or non-finite.
    #[error("channel weight {channel} must be finite and >= 0")]
    BadChannelWeight {
        /// Index into the weight vector.
        channel: usize,
    },
    /// A per-channel weight vector did not match the feature width.
    #[error("weight vector has {got} channels, the feature has {expected}")]
    WeightLengthMismatch {
        /// Length that was supplied.
        got: usize,
        /// Length the layout requires.
        expected: usize,
    },
    /// A clip duration was negative or non-finite.
    #[error("clip {index} has duration {duration}, which must be finite and >= 0")]
    BadClipDuration {
        /// Index into the clip list passed to the builder.
        index: usize,
        /// The rejected duration, in seconds.
        duration: f32,
    },
    /// The sample grid would store more than [`MAX_DATABASE_FRAMES`] rows.
    #[error("pose search would store {count} frames, above the limit of {limit}")]
    TooManyFrames {
        /// Planned frame count, saturated at the limit plus one when the ratio overflows.
        count: usize,
        /// [`MAX_DATABASE_FRAMES`].
        limit: usize,
    },
    /// Every candidate frame was dropped, or a raw build was given no rows.
    #[error("pose search produced no frames")]
    NoFrames,
    /// A sampled or weighted feature was not finite.
    #[error("feature of clip {clip} at {time} s is not finite")]
    NonFiniteFeature {
        /// Clip index in the database.
        clip: u32,
        /// Sample time, in seconds.
        time: f32,
    },
    /// The root joint's transform could not be inverted.
    #[error("root joint {joint} at {time} s is not invertible")]
    DegenerateRoot {
        /// Root joint id.
        joint: u32,
        /// Sample time, in seconds. Zero when the pose did not come from a clip.
        time: f32,
    },
    /// A query vector's length does not match [`FeatureDatabase::dimensions`].
    #[error("query has {got} channels, the database has {expected}")]
    FeatureLengthMismatch {
        /// Length that was supplied.
        got: usize,
        /// [`FeatureDatabase::dimensions`].
        expected: usize,
    },
    /// A query vector or a live trajectory sample was not finite.
    #[error("pose search query is not finite")]
    NonFiniteQuery,
    /// [`SearchState::new`] was given a negative or non-finite threshold.
    #[error("replay threshold {threshold} must be finite and >= 0")]
    BadThreshold {
        /// The rejected threshold, in weighted squared distance.
        threshold: f32,
    },
    /// A time delta was negative or not finite.
    ///
    /// [`SearchState::advance`] accepts a negative delta (it wraps or clamps). [`PoseSelection::begin_blend`]
    /// does not.
    #[error("time delta {delta} must be finite and >= 0")]
    BadDelta {
        /// The rejected delta, in seconds.
        delta: f32,
    },
    /// A clip index does not exist in the database or in the blend's clip slice.
    #[error("clip index {clip} is outside the library ({count} clips)")]
    ClipOutOfRange {
        /// The rejected index.
        clip: u32,
        /// How many clips the caller passed, or the database holds.
        count: usize,
    },
    /// [`InertialBlend::duration`] was negative or non-finite.
    #[error("blend duration {duration} must be finite and >= 0")]
    BadBlendDuration {
        /// The rejected duration, in seconds.
        duration: f32,
    },
    /// A [`RawFrame`] feature length did not match the first frame.
    #[error("raw frame {index} has {got} channels, expected {expected}")]
    RawFeatureMismatch {
        /// Index into the raw frame slice.
        index: usize,
        /// Length of that frame.
        got: usize,
        /// Length of the first frame.
        expected: usize,
    },
    /// A [`RawFrame::clip`] does not exist in the clip slice.
    #[error("raw frame {index} references clip {clip}, but only {count} clips were given")]
    RawClipOutOfRange {
        /// Index into the raw frame slice.
        index: usize,
        /// The rejected clip index.
        clip: u32,
        /// How many clips were given.
        count: usize,
    },
    /// A [`RawFrame::time`] was negative, non-finite, or past its clip's duration.
    #[error("raw frame {index} has time {time}, which must be finite, >= 0 and within the clip")]
    BadFrameTime {
        /// Index into the raw frame slice.
        index: usize,
        /// The rejected time, in seconds.
        time: f32,
    },
    /// [`FeatureDatabase::feature_at`] or [`FeatureDatabase::query_from_pose`] was called on a
    /// database built with [`FeatureDatabase::from_raw`], which has no skeleton layout.
    #[error("this database was built from raw features and has no pose layout")]
    NoPoseLayout,
    /// The live trajectory slice length does not match the database horizons.
    #[error("future trajectory has {got} samples, the database expects {expected}")]
    TrajectoryMismatch {
        /// Length that was supplied.
        got: usize,
        /// Number of horizons stored in the database.
        expected: usize,
    },
    /// A pose or skeleton joint count does not match the database layout.
    #[error("pose or skeleton has {got} joints, expected {expected}")]
    PoseMismatch {
        /// Count that was supplied.
        got: usize,
        /// Count required by the layout or the skeleton.
        expected: usize,
    },
    /// The clip sampled for a blend produced a non-finite pose.
    #[error("selected pose is not finite")]
    NonFinitePose,
    /// A playback or sample time was negative or non-finite.
    #[error("time {time} must be finite and >= 0")]
    BadTime {
        /// The rejected time, in seconds.
        time: f32,
    },
    /// [`FeatureDatabase::cost`] was asked for a frame the database does not have.
    #[error("frame {index} is outside the database")]
    FrameOutOfRange {
        /// The rejected frame index.
        index: u32,
    },
}

/// How far ahead, and in which joints, a [`FeatureDatabase`] samples clips.
///
/// Feature layout, in order:
///
/// - for each feature joint: position xyz in the root joint's frame, then velocity xyz
/// - for each trajectory horizon: future root offset xyz in the current root frame, then
///   `sin(yaw)` and `cos(yaw)` of the future root yaw relative to the current root
///
/// An empty [`PoseSearchConfig::feature_joints`] list means every joint except the root.
/// Joint positions are relative to the root joint of the sampled pose (the clip is not
/// pinned in place). Looping clips repeat the root's end-to-start translation and yaw the
/// same way [`AnimationClip::root_motion_delta_step`] repeats a lap.
#[derive(Clone, Debug)]
pub struct PoseSearchConfig {
    /// Seconds between database frames. Finite and at least [`MIN_SAMPLE_INTERVAL`].
    pub sample_interval: f32,
    /// Joints stored in the feature, in this order. Empty selects every non-root joint.
    pub feature_joints: Vec<u32>,
    /// Joint that defines the feature frame. Usually 0.
    pub root_joint: u32,
    /// Future offsets, in seconds, sampled from each frame. Each must be finite and >= 0.
    pub trajectory_horizons: Vec<f32>,
    /// Seconds between the two poses used for joint velocity. `0` uses [`Self::sample_interval`].
    pub velocity_window: f32,
    /// Weight applied to each joint-position channel after normalisation.
    pub position_weight: f32,
    /// Weight applied to each joint-velocity channel after normalisation.
    pub velocity_weight: f32,
    /// Weight applied to each future-offset channel after normalisation.
    pub trajectory_position_weight: f32,
    /// Weight applied to each future-facing channel (`sin` and `cos`) after normalisation.
    pub trajectory_facing_weight: f32,
    /// When non-empty, replaces the four group weights. Length must equal the feature width.
    pub channel_weights: Vec<f32>,
}

impl Default for PoseSearchConfig {
    fn default() -> Self {
        Self {
            sample_interval: 1.0 / 30.0,
            feature_joints: Vec::new(),
            root_joint: 0,
            trajectory_horizons: vec![0.3, 0.6, 0.9],
            velocity_window: 0.0,
            position_weight: 1.0,
            velocity_weight: 1.0,
            trajectory_position_weight: 1.0,
            trajectory_facing_weight: 1.0,
            channel_weights: Vec::new(),
        }
    }
}

/// One clip contributed to [`FeatureDatabase::build`].
#[derive(Clone, Copy, Debug)]
pub struct ClipSource<'a> {
    /// Clip sampled into the database. Joint tracks are read as authored (root motion is not pinned).
    pub clip: &'a AnimationClip,
    /// When true, future samples and playback wrap, and each lap adds the root's end-to-start delta.
    pub looping: bool,
}

/// Clip record stored beside the feature rows.
#[derive(Clone, Debug, PartialEq)]
pub struct DatabaseClip {
    /// Clip name copied from [`AnimationClip::name`] or supplied with the raw frames.
    pub name: String,
    /// Duration in seconds.
    pub duration: f32,
    /// Whether playback and future samples wrap.
    pub looping: bool,
}

/// One precomputed feature row for [`FeatureDatabase::from_raw`].
#[derive(Clone, Debug)]
pub struct RawFrame {
    /// Index into the clip slice passed to [`FeatureDatabase::from_raw`].
    pub clip: u32,
    /// Playback time this row stands for, in seconds. Finite and >= 0.
    pub time: f32,
    /// Un-normalised feature. Every frame in the call must use the same length.
    pub feature: Vec<f32>,
}

/// Where one database row came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameRef {
    /// Index into [`FeatureDatabase::clip`].
    pub clip: u32,
    /// Sample time in that clip, in seconds.
    pub time: f32,
}

/// Nearest database row, before the replay rule in [`SearchState`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Neighbor {
    /// Frame index in the database.
    pub frame: u32,
    /// Clip index of that frame.
    pub clip: u32,
    /// Sample time of that frame, in seconds.
    pub time: f32,
    /// Weighted squared distance to the query.
    pub cost: f32,
}

/// Which nearest-neighbour implementation to run.
///
/// Both visit the weighted, normalised rows and break ties toward the lowest frame index.
/// The tree is Bentley's KD-tree: median split, axis cycled by depth, and the far child is
/// visited when the splitting-plane distance squared is less than or equal to the best cost
/// so a tie on the other side of the plane is not pruned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchMethod {
    /// Scan every row.
    #[default]
    BruteForce,
    /// Exact KD-tree search.
    KdTree,
}

/// A future root sample already expressed for a query.
///
/// `offset` is the desired root position in the current root frame, metres: the frame is the
/// root's global position and rotation only, never its scale, so a centimetre rig (root scale
/// 0.01) still takes 1.0 for one metre. The database's own trajectory channels use the same
/// frame. (The joint-position channels are measured through the root's full transform, scale
/// included, on both the build and the query side, so hosts never convert them.)
/// `yaw` is the desired root yaw minus the current root yaw, radians.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrajectorySample {
    /// Future root position in the current root frame, metres.
    pub offset: Vec3,
    /// Future yaw relative to the current root, radians.
    pub yaw: f32,
}

/// The `(clip, time)` a search selected, and whether to start an inertialized blend.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoseSelection {
    /// Clip index in the database.
    pub clip: u32,
    /// Time to sample, in seconds. A jump uses the matched frame's time; continuing uses
    /// the playback clock, which may sit between database frames.
    pub time: f32,
    /// Weighted squared distance of the frame that justified this decision. This is a
    /// last-query value: [`SearchState::advance`] does not recompute it (that needs a query).
    pub cost: f32,
    /// When true, [`PoseSelection::begin_blend`] starts an [`Inertializer`].
    /// Continuing playback leaves this false.
    pub blend: bool,
    /// Database frame used for [`Self::cost`]. On a jump this is the matched frame; while
    /// continuing it is the database frame closest in time to [`Self::time`].
    pub frame: u32,
    /// Whether the clip wraps. Copied from the database so the blend can step backward in time.
    pub looping: bool,
}

/// What [`PoseSelection::begin_blend`] needs in order to sample the clip and start a blend.
#[derive(Clone, Copy, Debug)]
pub struct InertialBlend<'a> {
    /// Skeleton the clips were authored for.
    pub skeleton: &'a Skeleton,
    /// Clip assets, indexed the same way as the database.
    pub clips: &'a [AnimationClip],
    /// Pose currently showing. The inertializer decays from this onto the selected clip.
    pub source: &'a Pose,
    /// Previous source pose, for the source velocity. `None` treats the source as still.
    pub source_prev: Option<&'a Pose>,
    /// Decay used by [`Inertializer::start`].
    pub decay: DecayMethod,
    /// Blend length in seconds. Finite and >= 0. Zero records no active offset.
    pub duration: f32,
    /// Frame delta in seconds. Finite and >= 0. The target's previous pose is sampled `dt` earlier.
    pub dt: f32,
    /// Root-motion pinning override, mirroring the evaluator's `RootMotion` node. `None` derives
    /// the mode from the selected clip's own root-motion track (no track means no pinning). When
    /// the effective mode is not [`RootMotionMode::None`], the sampled target pose (and its
    /// previous sample) is pinned in place exactly like the runtime evaluator, so progression is
    /// extracted by the caller through [`AnimationClip::root_motion_delta_step`] (or
    /// [`AnimationClip::root_motion_delta`]) instead of accumulating on the visible root.
    pub root_motion: Option<RootMotionMode>,
}

/// Playback cursor for pose search.
///
/// The first successful search always adopts the nearest frame and sets [`PoseSelection::blend`].
/// Later searches adopt a frame from a different clip the same way. A frame in the current clip
/// replaces the playback clock only when `continue_cost - candidate_cost` is strictly greater
/// than [`Self::replay_threshold`]. Equality keeps playing, so a threshold of 0 still ignores a
/// candidate that is merely tied with continuing.
///
/// A non-looping clip whose playback clock has reached its clamped end has nothing left to
/// continue: the next search adopts its best frame (with a blend) even within the same clip and
/// regardless of the threshold. Only a match at the very time already showing (a frame stored
/// at the clip end) keeps playing without a blend.
#[derive(Clone, Debug)]
pub struct SearchState {
    current: Option<PoseSelection>,
    replay_threshold: f32,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            current: None,
            replay_threshold: 0.0,
        }
    }
}

impl SearchState {
    /// Cursor with no selection and the given replay threshold.
    ///
    /// # Errors
    ///
    /// [`PoseSearchError::BadThreshold`] when `replay_threshold` is negative or non-finite.
    pub fn new(replay_threshold: f32) -> Result<Self, PoseSearchError> {
        if !replay_threshold.is_finite() || replay_threshold < 0.0 {
            return Err(PoseSearchError::BadThreshold {
                threshold: replay_threshold,
            });
        }
        Ok(Self {
            current: None,
            replay_threshold,
        })
    }

    /// Threshold passed to [`SearchState::new`], in weighted squared distance.
    #[must_use]
    pub fn replay_threshold(&self) -> f32 {
        self.replay_threshold
    }

    /// Last selection, if [`SearchState::search`] has succeeded.
    #[must_use]
    pub fn current(&self) -> Option<PoseSelection> {
        self.current
    }

    /// Drop the current selection. The next search is treated as the first.
    pub fn clear(&mut self) {
        self.current = None;
    }

    /// Advance the playback clock by `dt` seconds without searching.
    ///
    /// Looping clips wrap. Other clips clamp to `[0, duration]`. A negative delta is allowed and
    /// uses the same rules. A cursor with no selection is left unchanged. The blend flag is
    /// cleared: advancing is continuous playback. [`PoseSelection::frame`] is recomputed as the
    /// stored frame nearest the new time; [`PoseSelection::cost`] keeps its last-query value
    /// (it cannot be recomputed without a query).
    ///
    /// # Errors
    ///
    /// [`PoseSearchError::BadDelta`] when `dt` is not finite, or [`PoseSearchError::ClipOutOfRange`]
    /// when the stored clip index is not in `database`.
    #[tracing::instrument(
        target = "regia_animation",
        level = "trace",
        skip_all,
        name = "pose_search.advance"
    )]
    pub fn advance(&mut self, database: &FeatureDatabase, dt: f32) -> Result<(), PoseSearchError> {
        if !dt.is_finite() {
            return Err(PoseSearchError::BadDelta { delta: dt });
        }
        let Some(current) = self.current else {
            return Ok(());
        };
        let clip = database
            .clip(current.clip)
            .ok_or_else(|| PoseSearchError::ClipOutOfRange {
                clip: current.clip,
                count: database.clip_count(),
            })?;
        let time = step_time(current.time, dt, clip.duration, clip.looping);
        let frame = database.nearest_in_time(current.clip, time)?;
        self.current = Some(PoseSelection {
            time,
            frame,
            blend: false,
            ..current
        });
        Ok(())
    }

    /// Pick a `(clip, time)` for `query` and remember it.
    ///
    /// `query` is a raw feature in the database layout ([`FeatureDatabase::normalize`] is
    /// applied inside). On failure the cursor is left as it was.
    ///
    /// # Errors
    ///
    /// The query errors of [`FeatureDatabase::nearest`], or [`PoseSearchError::ClipOutOfRange`]
    /// when the current clip has disappeared from `database`.
    #[tracing::instrument(
        target = "regia_animation",
        level = "trace",
        skip_all,
        name = "pose_search.search"
    )]
    pub fn search(
        &mut self,
        database: &FeatureDatabase,
        query: &[f32],
        method: SearchMethod,
    ) -> Result<PoseSelection, PoseSearchError> {
        let selection = self.decide(database, query, method)?;
        self.current = Some(selection);
        Ok(selection)
    }

    fn decide(
        &self,
        database: &FeatureDatabase,
        query: &[f32],
        method: SearchMethod,
    ) -> Result<PoseSelection, PoseSearchError> {
        let hit = database.nearest(query, method)?;
        let Some(current) = self.current else {
            return selection_from_hit(database, &hit, true);
        };
        if hit.clip != current.clip {
            return selection_from_hit(database, &hit, true);
        }
        let clip = database
            .clip(current.clip)
            .ok_or_else(|| PoseSearchError::ClipOutOfRange {
                clip: current.clip,
                count: database.clip_count(),
            })?;
        // A non-looping clip that has reached its clamped end cannot continue (spec 52, the
        // matcher's end rule). Its nearest stored frame is the last one that still covers the
        // horizon, which would keep winning "continue" and freeze the end pose (anim 21). Adopt
        // the match unless it is the very time already showing (a frame stored at the end).
        if !clip.looping
            && current.time >= clip.duration - TIME_EPS
            && (hit.time - current.time).abs() > TIME_EPS
        {
            return selection_from_hit(database, &hit, true);
        }
        let cont = database.nearest_in_time(current.clip, current.time)?;
        let continue_cost = database.cost(cont, query)?;
        let improvement = continue_cost - hit.cost;
        if hit.frame != cont && improvement > self.replay_threshold {
            return selection_from_hit(database, &hit, true);
        }
        Ok(PoseSelection {
            clip: current.clip,
            time: current.time,
            cost: continue_cost,
            blend: false,
            frame: cont,
            looping: current.looping,
        })
    }
}

impl PoseSelection {
    /// Sample this selection into `target` and, when [`PoseSelection::blend`] is set, start
    /// `inertializer` so the visible pose leaves [`InertialBlend::source`].
    ///
    /// The target starts from the bind pose, then the clip overwrites the joints it animates.
    /// When the effective root-motion mode (see [`InertialBlend::root_motion`]) is not
    /// [`RootMotionMode::None`], the sampled pose is pinned in place like the runtime evaluator,
    /// so the visible root never drifts with the clip's authored root motion; the caller extracts
    /// progression with [`AnimationClip::root_motion_delta_step`]. When `blend` is false the
    /// inertializer is not touched. On error neither `target` nor `inertializer` is modified.
    ///
    /// # Errors
    ///
    /// See [`PoseSearchError`] variants `EmptySkeleton`, `ClipOutOfRange`, `BadTime`,
    /// `BadDelta`, `BadBlendDuration`, `PoseMismatch`, and `NonFinitePose`.
    pub fn begin_blend(
        &self,
        blend: &InertialBlend<'_>,
        inertializer: &mut Inertializer,
        target: &mut Pose,
    ) -> Result<(), PoseSearchError> {
        validate_blend(self, blend)?;
        let clip = &blend.clips[self.clip as usize];
        let mut sampled = blend.skeleton.bind_pose_pose();
        clip.sample_pose(self.time, &mut sampled);
        pin_for_blend(clip, blend.root_motion, &mut sampled);
        if !pose_finite(&sampled) {
            return Err(PoseSearchError::NonFinitePose);
        }
        if self.blend {
            let mut prev = blend.skeleton.bind_pose_pose();
            let prev_time = previous_sample_time(self.time, blend.dt, clip.duration, self.looping);
            clip.sample_pose(prev_time, &mut prev);
            pin_for_blend(clip, blend.root_motion, &mut prev);
            if !pose_finite(&prev) {
                return Err(PoseSearchError::NonFinitePose);
            }
            inertializer.start(
                blend.decay,
                blend.duration,
                Switch {
                    source: blend.source,
                    source_prev: blend.source_prev,
                    target: &sampled,
                    target_prev: Some(&prev),
                    dt: blend.dt,
                },
            );
        }
        target.copy_from(&sampled);
        Ok(())
    }
}

/// Offline feature matrix, its normalisation, and both nearest-neighbour indexes.
#[derive(Clone, Debug)]
pub struct FeatureDatabase {
    clips: Vec<DatabaseClip>,
    frames: Vec<FrameLoc>,
    raw: Vec<f32>,
    rows: Vec<f32>,
    dims: usize,
    mean: Vec<f32>,
    deviation: Vec<f32>,
    weights: Vec<f32>,
    nodes: Vec<KdNode>,
    root: u32,
    layout: Option<PoseLayout>,
}

#[derive(Clone, Debug)]
struct FrameLoc {
    clip: u32,
    time: f32,
}

#[derive(Clone, Debug)]
struct PoseLayout {
    joints: Vec<u32>,
    root: usize,
    horizons: Vec<f32>,
    velocity_window: f32,
    joint_count: usize,
}

#[derive(Clone, Debug)]
struct KdNode {
    frame: u32,
    left: u32,
    right: u32,
    axis: usize,
}

struct Best {
    frame: u32,
    cost: f32,
}

impl FeatureDatabase {
    /// Sample `clips` on `skeleton` and build both search structures.
    ///
    /// Non-looping clips drop a frame that cannot cover the longest trajectory horizon.
    /// Looping clips keep every sample and extend the root past the end by whole laps.
    ///
    /// # Errors
    ///
    /// Configuration, skeleton, clip, and sampling failures. See [`PoseSearchError`].
    #[tracing::instrument(target = "regia_animation", skip_all, name = "pose_search.build")]
    pub fn build(
        skeleton: &Skeleton,
        clips: &[ClipSource<'_>],
        config: &PoseSearchConfig,
    ) -> Result<Self, PoseSearchError> {
        if clips.is_empty() {
            return Err(PoseSearchError::NoClips);
        }
        if skeleton.is_empty() {
            return Err(PoseSearchError::EmptySkeleton);
        }
        let resolved = resolve_config(skeleton, config)?;
        let mut total = 0usize;
        for (index, source) in clips.iter().enumerate() {
            check_duration(index, source.clip.duration)?;
            let planned = planned_samples(source.clip.duration, resolved.interval)?;
            total = total.saturating_add(planned);
            if total > MAX_DATABASE_FRAMES {
                return Err(PoseSearchError::TooManyFrames {
                    count: total,
                    limit: MAX_DATABASE_FRAMES,
                });
            }
        }
        let weights = expand_weights(config, resolved.joints.len(), resolved.horizons.len())?;
        let bind = skeleton.bind_pose_pose();
        let mut pose = bind.clone();
        let mut pose_b = bind.clone();
        let mut frames = Vec::new();
        let mut raw = Vec::new();
        for (index, source) in clips.iter().enumerate() {
            let mut poses = PosePair {
                now: &mut pose,
                other: &mut pose_b,
            };
            let mut sink = RowSink {
                frames: &mut frames,
                raw: &mut raw,
            };
            let ctx = SampleCtx {
                skeleton,
                clip: source.clip,
                bind: &bind,
                root: resolved.root,
                joints: &resolved.joints,
                clip_index: index as u32,
            };
            bake_clip(&ctx, source, &resolved, &mut poses, &mut sink)?;
        }
        if frames.is_empty() {
            return Err(PoseSearchError::NoFrames);
        }
        let stored = clips
            .iter()
            .map(|source| DatabaseClip {
                name: source.clip.name.clone(),
                duration: source.clip.duration,
                looping: source.looping,
            })
            .collect();
        let layout = PoseLayout {
            joints: resolved.joints,
            root: resolved.root,
            horizons: resolved.horizons,
            velocity_window: resolved.velocity_window,
            joint_count: skeleton.len(),
        };
        finish(stored, frames, raw, weights, Some(layout))
    }

    /// Build from feature rows that were extracted elsewhere.
    ///
    /// `weights` empty means every channel weighs 1. Normalisation and both indexes match
    /// [`FeatureDatabase::build`]. The result has no pose layout, so
    /// [`FeatureDatabase::feature_at`] and [`FeatureDatabase::query_from_pose`] fail with
    /// [`PoseSearchError::NoPoseLayout`].
    ///
    /// # Errors
    ///
    /// See [`PoseSearchError`] variants starting at `NoClips`, `NoFrames`, `NoFeatures`,
    /// `BadClipDuration`, `RawFeatureMismatch`, `RawClipOutOfRange`, `BadFrameTime`,
    /// `NonFiniteFeature`, `WeightLengthMismatch`, `BadChannelWeight`, and `TooManyFrames`.
    #[tracing::instrument(target = "regia_animation", skip_all, name = "pose_search.from_raw")]
    pub fn from_raw(
        clips: &[DatabaseClip],
        frames: &[RawFrame],
        weights: &[f32],
    ) -> Result<Self, PoseSearchError> {
        if clips.is_empty() {
            return Err(PoseSearchError::NoClips);
        }
        if frames.is_empty() {
            return Err(PoseSearchError::NoFrames);
        }
        if frames.len() > MAX_DATABASE_FRAMES {
            return Err(PoseSearchError::TooManyFrames {
                count: frames.len(),
                limit: MAX_DATABASE_FRAMES,
            });
        }
        for (index, clip) in clips.iter().enumerate() {
            check_duration(index, clip.duration)?;
        }
        let dims = frames[0].feature.len();
        if dims == 0 {
            return Err(PoseSearchError::NoFeatures);
        }
        let mut raw = Vec::with_capacity(frames.len() * dims);
        let mut stored = Vec::with_capacity(frames.len());
        for (index, frame) in frames.iter().enumerate() {
            if frame.clip as usize >= clips.len() {
                return Err(PoseSearchError::RawClipOutOfRange {
                    index,
                    clip: frame.clip,
                    count: clips.len(),
                });
            }
            if !frame.time.is_finite() || frame.time < 0.0 {
                return Err(PoseSearchError::BadFrameTime {
                    index,
                    time: frame.time,
                });
            }
            if frame.time > clips[frame.clip as usize].duration + TIME_EPS {
                return Err(PoseSearchError::BadFrameTime {
                    index,
                    time: frame.time,
                });
            }
            if frame.feature.len() != dims {
                return Err(PoseSearchError::RawFeatureMismatch {
                    index,
                    got: frame.feature.len(),
                    expected: dims,
                });
            }
            if frame.feature.iter().any(|value| !value.is_finite()) {
                return Err(PoseSearchError::NonFiniteFeature {
                    clip: frame.clip,
                    time: frame.time,
                });
            }
            raw.extend_from_slice(&frame.feature);
            stored.push(FrameLoc {
                clip: frame.clip,
                time: frame.time,
            });
        }
        let weights = resolve_channel_weights(weights, dims)?;
        finish(clips.to_vec(), stored, raw, weights, None)
    }

    /// Number of stored frames.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Number of channels in each feature row.
    #[must_use]
    pub fn dimensions(&self) -> usize {
        self.dims
    }

    /// Number of clips.
    #[must_use]
    pub fn clip_count(&self) -> usize {
        self.clips.len()
    }

    /// Clip metadata for `index`, if it exists.
    #[must_use]
    pub fn clip(&self, index: u32) -> Option<&DatabaseClip> {
        self.clips.get(index as usize)
    }

    /// Source of frame `index`, if it exists.
    #[must_use]
    pub fn frame(&self, index: u32) -> Option<FrameRef> {
        self.frames.get(index as usize).map(|frame| FrameRef {
            clip: frame.clip,
            time: frame.time,
        })
    }

    /// Per-channel mean of the raw rows.
    #[must_use]
    pub fn mean(&self) -> &[f32] {
        &self.mean
    }

    /// Per-channel population standard deviation of the raw rows, before [`DEVIATION_EPSILON`].
    #[must_use]
    pub fn deviation(&self) -> &[f32] {
        &self.deviation
    }

    /// Per-channel weights applied after normalisation.
    #[must_use]
    pub fn weights(&self) -> &[f32] {
        &self.weights
    }

    /// Raw feature of frame `index`, before normalisation.
    #[must_use]
    pub fn raw_row(&self, index: u32) -> Option<&[f32]> {
        row(&self.raw, self.dims, index)
    }

    /// Weighted, normalised feature of frame `index`. This is the space both searches use.
    #[must_use]
    pub fn weighted_row(&self, index: u32) -> Option<&[f32]> {
        row(&self.rows, self.dims, index)
    }

    /// Normalise `raw` with this database's mean, deviation and weights.
    ///
    /// # Errors
    ///
    /// [`PoseSearchError::FeatureLengthMismatch`] or [`PoseSearchError::NonFiniteQuery`].
    pub fn normalize(&self, raw: &[f32]) -> Result<Vec<f32>, PoseSearchError> {
        if raw.len() != self.dims {
            return Err(PoseSearchError::FeatureLengthMismatch {
                got: raw.len(),
                expected: self.dims,
            });
        }
        if raw.iter().any(|value| !value.is_finite()) {
            return Err(PoseSearchError::NonFiniteQuery);
        }
        let mut out = vec![0.0; self.dims];
        apply_norm(raw, &self.mean, &self.deviation, &self.weights, &mut out);
        if out.iter().any(|value| !value.is_finite()) {
            return Err(PoseSearchError::NonFiniteQuery);
        }
        Ok(out)
    }

    /// Weighted squared distance from `query` (raw) to one stored frame.
    ///
    /// # Errors
    ///
    /// [`PoseSearchError::FrameOutOfRange`], plus the errors of [`FeatureDatabase::normalize`].
    pub fn cost(&self, frame: u32, query: &[f32]) -> Result<f32, PoseSearchError> {
        let Some(stored) = self.weighted_row(frame) else {
            return Err(PoseSearchError::FrameOutOfRange { index: frame });
        };
        let weighted = self.normalize(query)?;
        Ok(distance_sq(stored, &weighted))
    }

    /// Nearest frame to a raw query.
    ///
    /// # Errors
    ///
    /// [`PoseSearchError::NoFrames`] when the database is empty, plus the errors of
    /// [`FeatureDatabase::normalize`].
    #[tracing::instrument(
        target = "regia_animation",
        level = "trace",
        skip_all,
        name = "pose_search.nearest"
    )]
    pub fn nearest(&self, query: &[f32], method: SearchMethod) -> Result<Neighbor, PoseSearchError> {
        if self.frames.is_empty() {
            return Err(PoseSearchError::NoFrames);
        }
        let weighted = self.normalize(query)?;
        let found = match method {
            SearchMethod::BruteForce => brute_nearest(&self.rows, self.dims, &weighted),
            SearchMethod::KdTree => kd_nearest(&self.nodes, self.root, &self.rows, self.dims, &weighted),
        };
        let Some((frame, cost)) = found else {
            return Err(PoseSearchError::NoFrames);
        };
        let loc = self
            .frames
            .get(frame as usize)
            .ok_or(PoseSearchError::FrameOutOfRange { index: frame })?;
        Ok(Neighbor {
            frame,
            clip: loc.clip,
            time: loc.time,
            cost,
        })
    }

    /// Raw feature this database's layout would store for `clip` at `time`.
    ///
    /// # Errors
    ///
    /// [`PoseSearchError::NoPoseLayout`] for a raw database, [`PoseSearchError::BadTime`],
    /// [`PoseSearchError::PoseMismatch`], [`PoseSearchError::BadClipDuration`], and the
    /// sampling errors of [`FeatureDatabase::build`].
    #[tracing::instrument(
        target = "regia_animation",
        level = "trace",
        skip_all,
        name = "pose_search.feature_at"
    )]
    pub fn feature_at(
        &self,
        skeleton: &Skeleton,
        clip: &AnimationClip,
        time: f32,
        looping: bool,
    ) -> Result<Vec<f32>, PoseSearchError> {
        let layout = self.layout.as_ref().ok_or(PoseSearchError::NoPoseLayout)?;
        if !time.is_finite() || time < 0.0 {
            return Err(PoseSearchError::BadTime { time });
        }
        check_duration(0, clip.duration)?;
        // Only times the build would store are queryable: past the clip end never, and for a
        // non-looping clip not past the last frame that can still cover the longest horizon.
        if time > clip.duration + TIME_EPS {
            return Err(PoseSearchError::BadTime { time });
        }
        if !looping {
            let max_horizon = layout.horizons.iter().copied().fold(0.0f32, f32::max);
            if time + max_horizon > clip.duration + TIME_EPS {
                return Err(PoseSearchError::BadTime { time });
            }
        }
        if skeleton.len() != layout.joint_count {
            return Err(PoseSearchError::PoseMismatch {
                got: skeleton.len(),
                expected: layout.joint_count,
            });
        }
        let bind = skeleton.bind_pose_pose();
        let mut pose = bind.clone();
        let mut pose_b = bind.clone();
        let ctx = SampleCtx {
            skeleton,
            clip,
            bind: &bind,
            root: layout.root,
            joints: &layout.joints,
            clip_index: 0,
        };
        let mut poses = PosePair {
            now: &mut pose,
            other: &mut pose_b,
        };
        let cycle = if looping {
            measure_cycle(&ctx, poses.now, poses.other)?
        } else {
            Cycle::identity()
        };
        extract_at(&ctx, &mut poses, layout, time, looping, &cycle)
    }

    /// Raw feature for a live pose and a desired future trajectory.
    ///
    /// `prev` is the pose one velocity window earlier. `None` stores zero velocities.
    /// `future` has one [`TrajectorySample`] per horizon, already in the current root frame.
    ///
    /// # Errors
    ///
    /// [`PoseSearchError::NoPoseLayout`], [`PoseSearchError::PoseMismatch`],
    /// [`PoseSearchError::TrajectoryMismatch`], [`PoseSearchError::NonFiniteQuery`],
    /// [`PoseSearchError::DegenerateRoot`], and [`PoseSearchError::JointOutOfRange`].
    #[tracing::instrument(
        target = "regia_animation",
        level = "trace",
        skip_all,
        name = "pose_search.query_pose"
    )]
    pub fn query_from_pose(
        &self,
        skeleton: &Skeleton,
        pose: &Pose,
        prev: Option<&Pose>,
        future: &[TrajectorySample],
    ) -> Result<Vec<f32>, PoseSearchError> {
        let layout = self.layout.as_ref().ok_or(PoseSearchError::NoPoseLayout)?;
        if skeleton.len() != layout.joint_count {
            return Err(PoseSearchError::PoseMismatch {
                got: skeleton.len(),
                expected: layout.joint_count,
            });
        }
        check_pose_arrays(pose, skeleton.len())?;
        if let Some(previous) = prev {
            check_pose_arrays(previous, skeleton.len())?;
        }
        if future.len() != layout.horizons.len() {
            return Err(PoseSearchError::TrajectoryMismatch {
                got: future.len(),
                expected: layout.horizons.len(),
            });
        }
        let now = relatives(skeleton, pose, layout, 0.0).map_err(map_query_error)?;
        let velocities = if let Some(previous) = prev {
            let before = relatives(skeleton, previous, layout, 0.0).map_err(map_query_error)?;
            velocities_between(&now.positions, &before.positions, layout.velocity_window, false)
        } else {
            vec![Vec3::ZERO; now.positions.len()]
        };
        let mut future_pos = Vec::with_capacity(future.len());
        let mut future_yaw = Vec::with_capacity(future.len());
        for sample in future {
            if !sample.offset.is_finite() || !sample.yaw.is_finite() {
                return Err(PoseSearchError::NonFiniteQuery);
            }
            future_pos.push(sample.offset);
            future_yaw.push(sample.yaw);
        }
        let mut raw = Vec::new();
        pack(&now.positions, &velocities, &future_pos, &future_yaw, &mut raw);
        if raw.iter().any(|value| !value.is_finite()) {
            return Err(PoseSearchError::NonFiniteQuery);
        }
        Ok(raw)
    }

    fn nearest_in_time(&self, clip: u32, time: f32) -> Result<u32, PoseSearchError> {
        let meta = self.clip(clip).ok_or_else(|| PoseSearchError::ClipOutOfRange {
            clip,
            count: self.clip_count(),
        })?;
        let mut best: Option<(u32, f32)> = None;
        for (index, frame) in self.frames.iter().enumerate() {
            if frame.clip != clip {
                continue;
            }
            let distance = time_distance(time, frame.time, meta.duration, meta.looping);
            let replace = match best {
                None => true,
                Some((_, best_distance)) => distance < best_distance,
            };
            if replace {
                best = Some((index as u32, distance));
            }
        }
        best.map(|(index, _)| index)
            .ok_or_else(|| PoseSearchError::ClipOutOfRange {
                clip,
                count: self.clip_count(),
            })
    }
}

struct Resolved {
    joints: Vec<u32>,
    root: usize,
    horizons: Vec<f32>,
    interval: f32,
    velocity_window: f32,
    max_horizon: f32,
}

struct SampleCtx<'a> {
    skeleton: &'a Skeleton,
    clip: &'a AnimationClip,
    bind: &'a Pose,
    root: usize,
    joints: &'a [u32],
    clip_index: u32,
}

struct PosePair<'a> {
    now: &'a mut Pose,
    other: &'a mut Pose,
}

struct RowSink<'a> {
    frames: &'a mut Vec<FrameLoc>,
    raw: &'a mut Vec<f32>,
}

struct Sampled {
    positions: Vec<Vec3>,
    root_pos: Vec3,
    root_yaw: f32,
    /// Inverse of the root's global rotation alone. Trajectory offsets are measured in this
    /// frame, so they stay in metres on a scaled root (anim 20).
    root_rot_inv: Quat,
}

struct Cycle {
    start_pos: Vec3,
    delta_pos: Vec3,
    delta_yaw: f32,
}

impl Cycle {
    /// A non-looping clip never leaves lap zero, so its cycle needs no end sampling.
    fn identity() -> Self {
        Self {
            start_pos: Vec3::ZERO,
            delta_pos: Vec3::ZERO,
            delta_yaw: 0.0,
        }
    }
}

fn finish(
    clips: Vec<DatabaseClip>,
    frames: Vec<FrameLoc>,
    raw: Vec<f32>,
    weights: Vec<f32>,
    layout: Option<PoseLayout>,
) -> Result<FeatureDatabase, PoseSearchError> {
    let dims = weights.len();
    let count = frames.len();
    if dims == 0 || count == 0 || raw.len() != count * dims {
        return Err(PoseSearchError::NoFeatures);
    }
    let (mean, deviation) = statistics(&raw, dims, count);
    let mut rows = vec![0.0; raw.len()];
    for (index, frame) in frames.iter().enumerate() {
        let start = index * dims;
        apply_norm(
            &raw[start..start + dims],
            &mean,
            &deviation,
            &weights,
            &mut rows[start..start + dims],
        );
        if rows[start..start + dims].iter().any(|value| !value.is_finite()) {
            return Err(PoseSearchError::NonFiniteFeature {
                clip: frame.clip,
                time: frame.time,
            });
        }
    }
    let (nodes, root) = build_kd(&rows, dims);
    tracing::debug!(
        target: "regia_animation",
        frames = count,
        dims,
        "pose search database built"
    );
    Ok(FeatureDatabase {
        clips,
        frames,
        raw,
        rows,
        dims,
        mean,
        deviation,
        weights,
        nodes,
        root,
        layout,
    })
}

fn resolve_config(skeleton: &Skeleton, config: &PoseSearchConfig) -> Result<Resolved, PoseSearchError> {
    if !config.sample_interval.is_finite() || config.sample_interval < MIN_SAMPLE_INTERVAL {
        return Err(PoseSearchError::BadSampleInterval {
            interval: config.sample_interval,
        });
    }
    if !config.velocity_window.is_finite() || config.velocity_window < 0.0 {
        return Err(PoseSearchError::BadVelocityWindow {
            window: config.velocity_window,
        });
    }
    let joint_count = skeleton.len();
    if config.root_joint as usize >= joint_count {
        return Err(PoseSearchError::RootOutOfRange {
            joint: config.root_joint,
            joint_count,
        });
    }
    let joints = if config.feature_joints.is_empty() {
        (0..joint_count)
            .filter(|&joint| joint != config.root_joint as usize)
            .map(|joint| joint as u32)
            .collect()
    } else {
        let mut seen = vec![false; joint_count];
        for &joint in &config.feature_joints {
            if joint as usize >= joint_count {
                return Err(PoseSearchError::JointOutOfRange { joint, joint_count });
            }
            if seen[joint as usize] {
                return Err(PoseSearchError::DuplicateJoint { joint });
            }
            seen[joint as usize] = true;
        }
        config.feature_joints.clone()
    };
    let mut max_horizon = 0.0f32;
    for (index, &seconds) in config.trajectory_horizons.iter().enumerate() {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(PoseSearchError::BadHorizon { index, seconds });
        }
        max_horizon = max_horizon.max(seconds);
    }
    if joints.is_empty() && config.trajectory_horizons.is_empty() {
        return Err(PoseSearchError::NoFeatures);
    }
    let velocity_window = if config.velocity_window == 0.0 {
        config.sample_interval
    } else {
        config.velocity_window
    };
    Ok(Resolved {
        joints,
        root: config.root_joint as usize,
        horizons: config.trajectory_horizons.clone(),
        interval: config.sample_interval,
        velocity_window,
        max_horizon,
    })
}

fn expand_weights(
    config: &PoseSearchConfig,
    joint_count: usize,
    horizon_count: usize,
) -> Result<Vec<f32>, PoseSearchError> {
    let dims = joint_count * 6 + horizon_count * 5;
    if !config.channel_weights.is_empty() {
        return resolve_channel_weights(&config.channel_weights, dims);
    }
    check_group("position", config.position_weight)?;
    check_group("velocity", config.velocity_weight)?;
    check_group("trajectory_position", config.trajectory_position_weight)?;
    check_group("trajectory_facing", config.trajectory_facing_weight)?;
    let mut weights = Vec::with_capacity(dims);
    for _ in 0..joint_count {
        for _ in 0..3 {
            weights.push(config.position_weight);
        }
        for _ in 0..3 {
            weights.push(config.velocity_weight);
        }
    }
    for _ in 0..horizon_count {
        for _ in 0..3 {
            weights.push(config.trajectory_position_weight);
        }
        for _ in 0..2 {
            weights.push(config.trajectory_facing_weight);
        }
    }
    Ok(weights)
}

fn resolve_channel_weights(weights: &[f32], dims: usize) -> Result<Vec<f32>, PoseSearchError> {
    if weights.is_empty() {
        return Ok(vec![1.0; dims]);
    }
    if weights.len() != dims {
        return Err(PoseSearchError::WeightLengthMismatch {
            got: weights.len(),
            expected: dims,
        });
    }
    for (channel, &weight) in weights.iter().enumerate() {
        if !weight.is_finite() || weight < 0.0 {
            return Err(PoseSearchError::BadChannelWeight { channel });
        }
    }
    Ok(weights.to_vec())
}

fn check_group(name: &'static str, weight: f32) -> Result<(), PoseSearchError> {
    if !weight.is_finite() || weight < 0.0 {
        return Err(PoseSearchError::BadWeight { name });
    }
    Ok(())
}

fn check_duration(index: usize, duration: f32) -> Result<(), PoseSearchError> {
    if !duration.is_finite() || duration < 0.0 {
        return Err(PoseSearchError::BadClipDuration { index, duration });
    }
    Ok(())
}

fn planned_samples(duration: f32, interval: f32) -> Result<usize, PoseSearchError> {
    if duration <= TIME_EPS {
        return Ok(1);
    }
    let ratio = duration / interval;
    if !ratio.is_finite() || ratio > MAX_DATABASE_FRAMES as f32 {
        return Err(PoseSearchError::TooManyFrames {
            count: MAX_DATABASE_FRAMES.saturating_add(1),
            limit: MAX_DATABASE_FRAMES,
        });
    }
    Ok((ratio.floor() as usize).saturating_add(1))
}

fn sample_times(duration: f32, interval: f32) -> Vec<f32> {
    if duration <= TIME_EPS {
        return vec![0.0];
    }
    let steps = (duration / interval).floor() as usize;
    let mut times = Vec::with_capacity(steps + 1);
    for step in 0..=steps {
        let time = interval * step as f32;
        times.push(if time > duration { duration } else { time });
    }
    times
}

fn bake_clip(
    ctx: &SampleCtx<'_>,
    source: &ClipSource<'_>,
    resolved: &Resolved,
    poses: &mut PosePair<'_>,
    sink: &mut RowSink<'_>,
) -> Result<(), PoseSearchError> {
    // Only looping clips extend futures past the end by whole laps; a non-looping clip never
    // uses its cycle, so a degenerate root at the very last key must not fail the build.
    let cycle = if source.looping {
        measure_cycle(ctx, poses.now, poses.other)?
    } else {
        Cycle::identity()
    };
    let layout = PoseLayout {
        joints: resolved.joints.clone(),
        root: resolved.root,
        horizons: resolved.horizons.clone(),
        velocity_window: resolved.velocity_window,
        joint_count: ctx.skeleton.len(),
    };
    let duration = ctx.clip.duration;
    for time in sample_times(duration, resolved.interval) {
        if !source.looping && time + resolved.max_horizon > duration + TIME_EPS {
            continue;
        }
        let feature = extract_at(ctx, poses, &layout, time, source.looping, &cycle)?;
        sink.raw.extend(feature);
        sink.frames.push(FrameLoc {
            clip: ctx.clip_index,
            time,
        });
    }
    Ok(())
}

#[tracing::instrument(
    target = "regia_animation",
    level = "trace",
    skip_all,
    name = "pose_search.frame"
)]
fn extract_at(
    ctx: &SampleCtx<'_>,
    poses: &mut PosePair<'_>,
    layout: &PoseLayout,
    time: f32,
    looping: bool,
    cycle: &Cycle,
) -> Result<Vec<f32>, PoseSearchError> {
    let duration = ctx.clip.duration;
    let now = sample_pose_at(ctx, poses.now, time)?;
    let velocities = match velocity_sample_time(time, layout.velocity_window, duration, looping) {
        None => vec![Vec3::ZERO; now.positions.len()],
        Some((other_time, other_is_future)) => {
            let other = sample_pose_at(ctx, poses.other, other_time)?;
            velocities_between(
                &now.positions,
                &other.positions,
                layout.velocity_window,
                other_is_future,
            )
        }
    };
    let mut future_pos = Vec::with_capacity(layout.horizons.len());
    let mut future_yaw = Vec::with_capacity(layout.horizons.len());
    for &horizon in &layout.horizons {
        let (local, laps) = locate(time + horizon, duration, looping);
        let sampled = sample_pose_at(ctx, poses.other, local)?;
        let pos = advance_laps(sampled.root_pos, cycle, laps);
        let yaw = (f64::from(sampled.root_yaw) + f64::from(cycle.delta_yaw) * laps as f64) as f32;
        // Rotation only: the root's scale would turn metres into root-local units (a cm rig
        // stored 100 per metre) while hosts pass `TrajectorySample::offset` in metres.
        let offset = now.root_rot_inv * (pos - now.root_pos);
        if !offset.is_finite() || !yaw.is_finite() {
            return Err(PoseSearchError::NonFiniteFeature {
                clip: ctx.clip_index,
                time,
            });
        }
        future_pos.push(offset);
        future_yaw.push(wrap_pi(yaw - now.root_yaw));
    }
    let mut feature = Vec::new();
    pack(
        &now.positions,
        &velocities,
        &future_pos,
        &future_yaw,
        &mut feature,
    );
    if feature.iter().any(|value| !value.is_finite()) {
        return Err(PoseSearchError::NonFiniteFeature {
            clip: ctx.clip_index,
            time,
        });
    }
    Ok(feature)
}

fn measure_cycle(ctx: &SampleCtx<'_>, pose: &mut Pose, pose_b: &mut Pose) -> Result<Cycle, PoseSearchError> {
    let start = sample_pose_at(ctx, pose, 0.0)?;
    let end_time = if ctx.clip.duration > 0.0 {
        ctx.clip.duration
    } else {
        0.0
    };
    let end = sample_pose_at(ctx, pose_b, end_time)?;
    let delta_pos = end.root_pos - start.root_pos;
    let delta_yaw = wrap_pi(end.root_yaw - start.root_yaw);
    if !delta_pos.is_finite() || !delta_yaw.is_finite() {
        return Err(PoseSearchError::NonFiniteFeature {
            clip: ctx.clip_index,
            time: 0.0,
        });
    }
    Ok(Cycle {
        start_pos: start.root_pos,
        delta_pos,
        delta_yaw,
    })
}

/// Position of the root `laps` whole laps past `local_pos`, composing each lap with the cycle
/// transform: the local phase rotates about the lap-start position by the accumulated cycle
/// yaw, and each lap's displacement is added turned by the yaw accumulated before it. A
/// straight-line loop (zero cycle yaw) reduces to `local_pos + delta_pos * laps`.
///
/// The lap sum is the closed geometric series of the yaw rotation. Walking each lap is wrong
/// past a few million laps (a long trajectory horizon) and the `i32` lap count used to saturate
/// and then spin.
fn advance_laps(local_pos: Vec3, cycle: &Cycle, laps: i64) -> Vec3 {
    let laps = laps.max(0);
    if laps == 0 {
        return local_pos;
    }
    let n = laps as f64;
    let (sin_lap, cos_lap) = crate::num::det_sin_cos_f64(f64::from(cycle.delta_yaw));
    let (sin_n, cos_n) = crate::num::det_sin_cos_f64(f64::from(cycle.delta_yaw) * n);
    let local = local_pos - cycle.start_pos;
    let lx = f64::from(local.x);
    let ly = f64::from(local.y);
    let lz = f64::from(local.z);
    // Glam's +Y rotation sends (x, z) to (x cos + z sin, -x sin + z cos).
    let rotated_x = lx * cos_n + lz * sin_n;
    let rotated_z = -lx * sin_n + lz * cos_n;
    let dx = f64::from(cycle.delta_pos.x);
    let dy = f64::from(cycle.delta_pos.y);
    let dz = f64::from(cycle.delta_pos.z);
    let det = (1.0 - cos_lap) * (1.0 - cos_lap) + sin_lap * sin_lap;
    let (sum_x, sum_z) = if det < 1.0e-18 {
        (n * dx, n * dz)
    } else {
        let ux = ((1.0 - cos_lap) * dx + sin_lap * dz) / det;
        let uz = (-sin_lap * dx + (1.0 - cos_lap) * dz) / det;
        ((1.0 - cos_n) * ux - sin_n * uz, sin_n * ux + (1.0 - cos_n) * uz)
    };
    Vec3::new(
        (f64::from(cycle.start_pos.x) + rotated_x + sum_x) as f32,
        (f64::from(cycle.start_pos.y) + ly + n * dy) as f32,
        (f64::from(cycle.start_pos.z) + rotated_z + sum_z) as f32,
    )
}

fn sample_pose_at(ctx: &SampleCtx<'_>, pose: &mut Pose, time: f32) -> Result<Sampled, PoseSearchError> {
    pose.copy_from(ctx.bind);
    ctx.clip.sample_pose(time, pose);
    relatives_of(ctx.skeleton, pose, ctx.root, ctx.joints, ctx.clip_index, time)
}

fn relatives(
    skeleton: &Skeleton,
    pose: &Pose,
    layout: &PoseLayout,
    time: f32,
) -> Result<Sampled, PoseSearchError> {
    relatives_of(skeleton, pose, layout.root, &layout.joints, 0, time)
}

fn relatives_of(
    skeleton: &Skeleton,
    pose: &Pose,
    root: usize,
    joints: &[u32],
    clip_index: u32,
    time: f32,
) -> Result<Sampled, PoseSearchError> {
    let globals = skeleton.globals(pose, None);
    let joint_count = skeleton.len();
    let Some(root_matrix) = globals.get(root).copied() else {
        return Err(PoseSearchError::RootOutOfRange {
            joint: root as u32,
            joint_count,
        });
    };
    let Some(root_inv) = root_matrix.try_inverse() else {
        return Err(PoseSearchError::DegenerateRoot {
            joint: root as u32,
            time,
        });
    };
    let root_pos = root_matrix.w_axis.truncate();
    let root_rot = root_matrix.to_scale_rotation_translation().1;
    let root_yaw = yaw_of(root_rot);
    let root_rot_inv = root_rot.inverse();
    if !root_pos.is_finite() || !root_yaw.is_finite() || !root_inv.is_finite() || !root_rot_inv.is_finite() {
        return Err(PoseSearchError::NonFiniteFeature {
            clip: clip_index,
            time,
        });
    }
    let mut positions = Vec::with_capacity(joints.len());
    for &joint in joints {
        let Some(global) = globals.get(joint as usize).copied() else {
            return Err(PoseSearchError::JointOutOfRange { joint, joint_count });
        };
        let position = root_inv.transform_point3(global.w_axis.truncate());
        if !position.is_finite() {
            return Err(PoseSearchError::NonFiniteFeature {
                clip: clip_index,
                time,
            });
        }
        positions.push(position);
    }
    Ok(Sampled {
        positions,
        root_pos,
        root_yaw,
        root_rot_inv,
    })
}

fn velocities_between(now: &[Vec3], other: &[Vec3], dt: f32, other_is_future: bool) -> Vec<Vec3> {
    now.iter()
        .zip(other.iter())
        .map(|(current, sample)| {
            if other_is_future {
                (*sample - *current) / dt
            } else {
                (*current - *sample) / dt
            }
        })
        .collect()
}

fn pack(
    positions: &[Vec3],
    velocities: &[Vec3],
    future_pos: &[Vec3],
    future_yaw: &[f32],
    out: &mut Vec<f32>,
) {
    out.clear();
    for (position, velocity) in positions.iter().zip(velocities) {
        out.extend_from_slice(&[
            position.x, position.y, position.z, velocity.x, velocity.y, velocity.z,
        ]);
    }
    for (position, yaw) in future_pos.iter().zip(future_yaw) {
        let (sin, cos) = crate::num::det_sin_cos(*yaw);
        out.extend_from_slice(&[position.x, position.y, position.z, sin, cos]);
    }
}

fn velocity_sample_time(time: f32, dt: f32, duration: f32, looping: bool) -> Option<(f32, bool)> {
    if !(dt.is_finite() && dt > 0.0) {
        return None;
    }
    if looping && duration > TIME_EPS {
        return Some(((time - dt).rem_euclid(duration), false));
    }
    if time >= dt {
        return Some((time - dt, false));
    }
    if duration > TIME_EPS && time + dt <= duration + TIME_EPS {
        return Some(((time + dt).min(duration), true));
    }
    None
}

fn locate(time: f32, duration: f32, looping: bool) -> (f32, i64) {
    if !looping || duration <= TIME_EPS || !time.is_finite() {
        let local = if duration <= TIME_EPS || !time.is_finite() {
            0.0
        } else {
            time.clamp(0.0, duration)
        };
        return (local, 0);
    }
    // f32 division saturates a multi-hour horizon into `i32::MAX`, and the old lap walk then
    // spun for billions of iterations. The lap count stays in f64 until it is a position.
    let shifted = f64::from(time.max(0.0));
    let dur = f64::from(duration);
    let laps_f = (shifted / dur).floor();
    let mut local = shifted - laps_f * dur;
    if local < 0.0 {
        local = 0.0;
    } else if local > dur {
        local = dur;
    }
    let laps = if laps_f.is_finite() && laps_f > 0.0 {
        if laps_f >= i64::MAX as f64 {
            i64::MAX
        } else {
            laps_f as i64
        }
    } else {
        0
    };
    (local as f32, laps)
}

/// Root yaw as the twist about +Y, the same measure root-motion extraction uses (ADR-2604).
///
/// Projecting local −Z onto the ground lost the yaw of every root carrying a Z-up basis
/// rotation (its −Z points straight down), so facing features read 0 on those rigs.
fn yaw_of(rotation: Quat) -> f32 {
    crate::clip::twist_yaw(rotation)
}

fn wrap_pi(angle: f32) -> f32 {
    if !angle.is_finite() {
        return angle;
    }
    let wrapped = (angle + PI).rem_euclid(TAU) - PI;
    if wrapped <= -PI { PI } else { wrapped }
}

/// Per-channel mean and population deviation, accumulated in f64 and rounded once.
///
/// The sums run in a fixed order with IEEE f64 arithmetic only, so they stay deterministic.
/// A sum of at most [`MAX_DATABASE_FRAMES`] f32 values is exact in f64, so a channel holding
/// one value everywhere gets that exact mean and a deviation of exactly 0 (an f32 running
/// mean of `x * (1/n)` terms misses by an ULP and turned such a channel into a ~1e-7 divisor).
fn statistics(raw: &[f32], dims: usize, count: usize) -> (Vec<f32>, Vec<f32>) {
    let mut sum = vec![0.0f64; dims];
    for frame in raw.chunks(dims).take(count) {
        for (acc, value) in sum.iter_mut().zip(frame) {
            *acc += f64::from(*value);
        }
    }
    let n = count as f64;
    let mean: Vec<f64> = sum.into_iter().map(|total| total / n).collect();
    let mut variance = vec![0.0f64; dims];
    for frame in raw.chunks(dims).take(count) {
        for ((acc, value), centre) in variance.iter_mut().zip(frame).zip(&mean) {
            let diff = f64::from(*value) - centre;
            *acc += diff * diff;
        }
    }
    let deviation = variance
        .into_iter()
        .map(|total| (total / n).sqrt() as f32)
        .collect();
    (mean.into_iter().map(|value| value as f32).collect(), deviation)
}

fn apply_norm(raw: &[f32], mean: &[f32], deviation: &[f32], weights: &[f32], out: &mut [f32]) {
    for index in 0..raw.len() {
        let scale = if deviation[index] > DEVIATION_EPSILON {
            deviation[index]
        } else {
            1.0
        };
        out[index] = (raw[index] - mean[index]) / scale * weights[index];
    }
}

fn row(data: &[f32], dims: usize, index: u32) -> Option<&[f32]> {
    let start = (index as usize).checked_mul(dims)?;
    let end = start.checked_add(dims)?;
    data.get(start..end)
}

fn distance_sq(left: &[f32], right: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    for (a, b) in left.iter().zip(right) {
        let delta = a - b;
        acc += delta * delta;
    }
    acc
}

fn consider(best: &mut Option<Best>, frame: u32, cost: f32) {
    let replace = match best {
        None => true,
        Some(current) => cost < current.cost || (cost == current.cost && frame < current.frame),
    };
    if replace {
        *best = Some(Best { frame, cost });
    }
}

fn brute_nearest(rows: &[f32], dims: usize, query: &[f32]) -> Option<(u32, f32)> {
    if dims == 0 {
        return None;
    }
    let count = rows.len() / dims;
    let mut best = None;
    for index in 0..count {
        let start = index * dims;
        let Some(point) = rows.get(start..start + dims) else {
            break;
        };
        consider(&mut best, index as u32, distance_sq(point, query));
    }
    best.map(|found| (found.frame, found.cost))
}

fn build_kd(rows: &[f32], dims: usize) -> (Vec<KdNode>, u32) {
    if dims == 0 || rows.is_empty() {
        return (Vec::new(), NONE);
    }
    let count = rows.len() / dims;
    let mut order: Vec<u32> = (0..count as u32).collect();
    let mut nodes = Vec::with_capacity(count);
    let root = build_range(&mut order, rows, dims, 0, &mut nodes);
    (nodes, root)
}

fn build_range(order: &mut [u32], rows: &[f32], dims: usize, depth: usize, nodes: &mut Vec<KdNode>) -> u32 {
    if order.is_empty() {
        return NONE;
    }
    let axis = depth % dims;
    let mid = order.len() / 2;
    order.select_nth_unstable_by(mid, |&left, &right| {
        let left_value = rows[left as usize * dims + axis];
        let right_value = rows[right as usize * dims + axis];
        left_value.total_cmp(&right_value).then(left.cmp(&right))
    });
    let frame = order[mid];
    let (left_part, rest) = order.split_at_mut(mid);
    let right_part = &mut rest[1..];
    let left = build_range(left_part, rows, dims, depth + 1, nodes);
    let right = build_range(right_part, rows, dims, depth + 1, nodes);
    let id = nodes.len() as u32;
    nodes.push(KdNode {
        frame,
        left,
        right,
        axis,
    });
    id
}

fn kd_nearest(nodes: &[KdNode], root: u32, rows: &[f32], dims: usize, query: &[f32]) -> Option<(u32, f32)> {
    let mut best = None;
    kd_walk(nodes, rows, dims, query, root, &mut best);
    best.map(|found| (found.frame, found.cost))
}

fn kd_walk(nodes: &[KdNode], rows: &[f32], dims: usize, query: &[f32], id: u32, best: &mut Option<Best>) {
    if id == NONE {
        return;
    }
    let Some(node) = nodes.get(id as usize) else {
        return;
    };
    let start = node.frame as usize * dims;
    let Some(point) = rows.get(start..start + dims) else {
        return;
    };
    consider(best, node.frame, distance_sq(point, query));
    let Some(&query_axis) = query.get(node.axis) else {
        return;
    };
    let Some(&point_axis) = point.get(node.axis) else {
        return;
    };
    let diff = query_axis - point_axis;
    let (near, far) = if diff <= 0.0 {
        (node.left, node.right)
    } else {
        (node.right, node.left)
    };
    kd_walk(nodes, rows, dims, query, near, best);
    // `<=` keeps a point that ties the best cost on the other side of the plane.
    let limit = best.as_ref().map_or(f32::MAX, |found| found.cost);
    if diff * diff <= limit {
        kd_walk(nodes, rows, dims, query, far, best);
    }
}

fn selection_from_hit(
    database: &FeatureDatabase,
    hit: &Neighbor,
    blend: bool,
) -> Result<PoseSelection, PoseSearchError> {
    let clip = database
        .clip(hit.clip)
        .ok_or_else(|| PoseSearchError::ClipOutOfRange {
            clip: hit.clip,
            count: database.clip_count(),
        })?;
    Ok(PoseSelection {
        clip: hit.clip,
        time: hit.time,
        cost: hit.cost,
        blend,
        frame: hit.frame,
        looping: clip.looping,
    })
}

fn step_time(time: f32, dt: f32, duration: f32, looping: bool) -> f32 {
    let next = time + dt;
    if looping && duration > TIME_EPS {
        let mut wrapped = next.rem_euclid(duration);
        if wrapped >= duration {
            wrapped = 0.0;
        }
        wrapped
    } else if duration <= TIME_EPS {
        0.0
    } else {
        next.clamp(0.0, duration)
    }
}

fn time_distance(time: f32, sample: f32, duration: f32, looping: bool) -> f32 {
    if looping && duration > TIME_EPS {
        let delta = (time - sample).rem_euclid(duration);
        delta.min(duration - delta)
    } else {
        (time - sample).abs()
    }
}

fn previous_sample_time(time: f32, dt: f32, duration: f32, looping: bool) -> f32 {
    // At a blend into `time < dt` there is no earlier sample: wrapping would measure the
    // target velocity across the whole lap (the position jump between the clip's end and its
    // start) and the offset inherits the spike.
    if looping && duration > TIME_EPS && time >= dt {
        (time - dt).rem_euclid(duration)
    } else {
        (time - dt).max(0.0)
    }
}

fn validate_blend(selection: &PoseSelection, blend: &InertialBlend<'_>) -> Result<(), PoseSearchError> {
    if blend.skeleton.is_empty() {
        return Err(PoseSearchError::EmptySkeleton);
    }
    if selection.clip as usize >= blend.clips.len() {
        return Err(PoseSearchError::ClipOutOfRange {
            clip: selection.clip,
            count: blend.clips.len(),
        });
    }
    if !selection.time.is_finite() || selection.time < 0.0 {
        return Err(PoseSearchError::BadTime { time: selection.time });
    }
    if !blend.dt.is_finite() {
        return Err(PoseSearchError::BadDelta { delta: blend.dt });
    }
    if blend.dt < 0.0 {
        return Err(PoseSearchError::BadDelta { delta: blend.dt });
    }
    if !blend.duration.is_finite() || blend.duration < 0.0 {
        return Err(PoseSearchError::BadBlendDuration {
            duration: blend.duration,
        });
    }
    check_pose_arrays(blend.source, blend.skeleton.len())?;
    if let Some(previous) = blend.source_prev {
        check_pose_arrays(previous, blend.skeleton.len())?;
    }
    Ok(())
}

/// A pose hand-built or deserialized with one array shorter than the skeleton would index
/// out of range in forward kinematics; reject it instead.
fn check_pose_arrays(pose: &Pose, expected: usize) -> Result<(), PoseSearchError> {
    for got in [pose.translations.len(), pose.rotations.len(), pose.scales.len()] {
        if got != expected {
            return Err(PoseSearchError::PoseMismatch { got, expected });
        }
    }
    Ok(())
}

/// Pin the sampled pose's root exactly like the runtime evaluator: the effective mode is the
/// caller's override, else the clip's own extraction mode. No root-motion track means no pin.
fn pin_for_blend(clip: &AnimationClip, override_mode: Option<RootMotionMode>, pose: &mut Pose) {
    let mode = override_mode.unwrap_or_else(|| {
        clip.root_motion
            .as_ref()
            .map_or(RootMotionMode::None, |track| track.mode)
    });
    if mode != RootMotionMode::None {
        clip.pin_root_in_place(pose, mode);
    }
}

fn pose_finite(pose: &Pose) -> bool {
    pose.translations.iter().all(|value| value.is_finite())
        && pose.rotations.iter().all(|value| value.is_finite())
        && pose.scales.iter().all(|value| value.is_finite())
}

fn map_query_error(error: PoseSearchError) -> PoseSearchError {
    match error {
        PoseSearchError::NonFiniteFeature { .. } => PoseSearchError::NonFiniteQuery,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::{JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
    use crate::pose::translated;
    use crate::skeleton::LocalBind;
    use regia_core::math::Mat4;

    fn skeleton(binds: Vec<LocalBind>) -> Skeleton {
        let count = binds.len();
        let names = (0..count)
            .map(|index| {
                if index == 0 {
                    "root".to_string()
                } else {
                    format!("j{index}")
                }
            })
            .collect();
        let parents = (0..count)
            .map(|index| if index == 0 { None } else { Some(0) })
            .collect();
        Skeleton::from_local_bind(names, parents, binds).expect("valid skeleton")
    }

    fn root_only() -> Skeleton {
        skeleton(vec![LocalBind::default()])
    }

    fn arm() -> Skeleton {
        skeleton(vec![LocalBind::default(), translated(Vec3::new(0.0, 1.0, 0.0))])
    }

    fn translation_clip(name: &str, duration: f32, joint: u32, keys: &[(f32, Vec3)]) -> AnimationClip {
        let mut clip = AnimationClip::empty(name, duration);
        clip.tracks.push(JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times: keys.iter().map(|(time, _)| *time).collect(),
                values: keys.iter().map(|(_, value)| *value).collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip
    }

    fn config_joints(interval: f32, joints: Vec<u32>, horizons: Vec<f32>) -> PoseSearchConfig {
        PoseSearchConfig {
            sample_interval: interval,
            feature_joints: joints,
            trajectory_horizons: horizons,
            ..PoseSearchConfig::default()
        }
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1.0e-4
    }

    fn near_vec(row: &[f32], expected: &[f32]) -> bool {
        row.len() == expected.len() && row.iter().zip(expected).all(|(a, b)| near(*a, *b))
    }

    #[test]
    fn default_config_uses_three_horizons_and_unit_weights() {
        let config = PoseSearchConfig::default();
        assert!(near(config.sample_interval, 1.0 / 30.0));
        assert_eq!(config.trajectory_horizons, vec![0.3, 0.6, 0.9]);
        assert!(near(config.position_weight, 1.0));
        assert!(config.channel_weights.is_empty());
        assert_eq!(SearchState::default().replay_threshold(), 0.0);
    }

    #[test]
    fn build_rejects_empty_inputs_and_bad_config() {
        let skel = arm();
        let clip = AnimationClip::empty("idle", 1.0);
        let source = [ClipSource {
            clip: &clip,
            looping: false,
        }];
        let config = config_joints(0.5, vec![1], vec![]);
        assert!(matches!(
            FeatureDatabase::build(&skel, &[], &config),
            Err(PoseSearchError::NoClips)
        ));
        assert!(matches!(
            FeatureDatabase::build(&Skeleton::default(), &source, &config),
            Err(PoseSearchError::EmptySkeleton)
        ));
        for interval in [0.0, -1.0, f32::NAN, f32::INFINITY, MIN_SAMPLE_INTERVAL * 0.5] {
            let mut bad = config.clone();
            bad.sample_interval = interval;
            assert!(matches!(
                FeatureDatabase::build(&skel, &source, &bad),
                Err(PoseSearchError::BadSampleInterval { .. })
            ));
        }
        let mut window = config.clone();
        window.velocity_window = -0.1;
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &window),
            Err(PoseSearchError::BadVelocityWindow { .. })
        ));
        window.velocity_window = f32::NAN;
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &window),
            Err(PoseSearchError::BadVelocityWindow { .. })
        ));
        let mut root = config.clone();
        root.root_joint = 9;
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &root),
            Err(PoseSearchError::RootOutOfRange {
                joint: 9,
                joint_count: 2
            })
        ));
        let mut missing = config.clone();
        missing.feature_joints = vec![4];
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &missing),
            Err(PoseSearchError::JointOutOfRange { joint: 4, .. })
        ));
        let mut twice = config.clone();
        twice.feature_joints = vec![1, 1];
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &twice),
            Err(PoseSearchError::DuplicateJoint { joint: 1 })
        ));
        let mut horizon = config.clone();
        horizon.trajectory_horizons = vec![0.2, -0.1];
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &horizon),
            Err(PoseSearchError::BadHorizon { index: 1, .. })
        ));
        horizon.trajectory_horizons = vec![f32::NAN];
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &horizon),
            Err(PoseSearchError::BadHorizon { index: 0, .. })
        ));
        let lone = config_joints(0.5, vec![], vec![]);
        assert!(matches!(
            FeatureDatabase::build(&root_only(), &source, &lone),
            Err(PoseSearchError::NoFeatures)
        ));
    }

    #[test]
    fn build_rejects_bad_weights_and_durations() {
        let skel = arm();
        let clip = AnimationClip::empty("idle", 1.0);
        let source = [ClipSource {
            clip: &clip,
            looping: false,
        }];
        let config = config_joints(0.5, vec![1], vec![]);
        let mut heavy = config.clone();
        heavy.position_weight = -1.0;
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &heavy),
            Err(PoseSearchError::BadWeight { name: "position" })
        ));
        heavy.position_weight = 1.0;
        heavy.velocity_weight = f32::NAN;
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &heavy),
            Err(PoseSearchError::BadWeight { name: "velocity" })
        ));
        heavy.velocity_weight = 1.0;
        heavy.trajectory_position_weight = f32::INFINITY;
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &heavy),
            Err(PoseSearchError::BadWeight {
                name: "trajectory_position"
            })
        ));
        heavy.trajectory_position_weight = 1.0;
        heavy.trajectory_facing_weight = -2.0;
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &heavy),
            Err(PoseSearchError::BadWeight {
                name: "trajectory_facing"
            })
        ));
        heavy.trajectory_facing_weight = 1.0;
        heavy.channel_weights = vec![1.0];
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &heavy),
            Err(PoseSearchError::WeightLengthMismatch { got: 1, expected: 6 })
        ));
        heavy.channel_weights = vec![1.0, 1.0, 1.0, f32::NAN, 1.0, 1.0];
        assert!(matches!(
            FeatureDatabase::build(&skel, &source, &heavy),
            Err(PoseSearchError::BadChannelWeight { channel: 3 })
        ));
        let broken = AnimationClip::empty("nan", f32::NAN);
        let bad_source = [ClipSource {
            clip: &broken,
            looping: false,
        }];
        assert!(matches!(
            FeatureDatabase::build(&skel, &bad_source, &config),
            Err(PoseSearchError::BadClipDuration { index: 0, .. })
        ));
        let negative = AnimationClip::empty("neg", -0.5);
        let neg_source = [ClipSource {
            clip: &negative,
            looping: false,
        }];
        assert!(matches!(
            FeatureDatabase::build(&skel, &neg_source, &config),
            Err(PoseSearchError::BadClipDuration { duration: -0.5, .. })
        ));
    }

    #[test]
    fn too_many_frames_and_uncovered_horizons_fail_before_sampling() {
        let clip = AnimationClip::empty("long", 12_500.0);
        let source = [ClipSource {
            clip: &clip,
            looping: false,
        }];
        let config = config_joints(0.125, vec![], vec![0.0]);
        match FeatureDatabase::build(&root_only(), &source, &config) {
            Err(PoseSearchError::TooManyFrames { count, limit }) => {
                assert!(count > limit);
                assert_eq!(limit, MAX_DATABASE_FRAMES);
            }
            other => panic!("expected TooManyFrames, got {other:?}"),
        }
        let short = AnimationClip::empty("short", 0.4);
        let short_source = [ClipSource {
            clip: &short,
            looping: false,
        }];
        let far = config_joints(0.2, vec![], vec![1.0]);
        assert!(matches!(
            FeatureDatabase::build(&root_only(), &short_source, &far),
            Err(PoseSearchError::NoFrames)
        ));
    }

    #[test]
    fn non_looping_drops_frames_that_cannot_see_the_horizon() {
        let clip = translation_clip(
            "move",
            1.0,
            0,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(4.0, 0.0, 0.0))],
        );
        let source = [ClipSource {
            clip: &clip,
            looping: false,
        }];
        let config = config_joints(0.5, vec![], vec![0.6]);
        let db = FeatureDatabase::build(&root_only(), &source, &config).expect("database");
        assert_eq!(db.frame_count(), 1);
        assert_eq!(db.frame(0).expect("frame").time, 0.0);
        let looping = [ClipSource {
            clip: &clip,
            looping: true,
        }];
        let wrapped = FeatureDatabase::build(&root_only(), &looping, &config).expect("looping database");
        assert_eq!(wrapped.frame_count(), 3);
    }

    #[test]
    fn looping_future_root_adds_whole_laps() {
        let clip = translation_clip(
            "stride",
            1.0,
            0,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(4.0, 0.0, 0.0))],
        );
        let source = [ClipSource {
            clip: &clip,
            looping: true,
        }];
        let config = config_joints(0.5, vec![], vec![0.5]);
        let db = FeatureDatabase::build(&root_only(), &source, &config).expect("database");
        // Constant velocity: every frame's half-second offset is +2 on X, facing unchanged.
        // The frame at t = 1 would read -2 without the lap delta (wrapped local time 0.5
        // sits behind the end pose).
        assert_eq!(db.frame_count(), 3);
        let last = db.raw_row(2).expect("last");
        assert!(near_vec(&last[..5], &[2.0, 0.0, 0.0, 0.0, 1.0]), "{last:?}");
        let first = db.raw_row(0).expect("first");
        assert!(near_vec(&first[..5], &[2.0, 0.0, 0.0, 0.0, 1.0]), "{first:?}");
    }

    #[test]
    fn relative_yaw_of_a_quarter_turn_is_sine_one() {
        let mut clip = AnimationClip::empty("turn", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::ZERO),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Quat::IDENTITY, Quat::from_rotation_y(PI * 0.5)],
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let source = [ClipSource {
            clip: &clip,
            looping: false,
        }];
        let config = config_joints(1.0, vec![], vec![1.0]);
        let db = FeatureDatabase::build(&root_only(), &source, &config).expect("database");
        let row = db.raw_row(0).expect("row");
        assert!(near_vec(row, &[0.0, 0.0, 0.0, 1.0, 0.0]), "{row:?}");
    }

    /// Future facing sine and cosine are part of the lockstep feature row. This yaw, stored as a
    /// det-built key, disagrees with platform `sin` by one ULP after the yaw round trip.
    #[test]
    fn future_facing_sine_matches_the_det_kernel() {
        let angle = f32::from_bits(0xc045_3de4);
        let end = crate::num::det_rotation_y(angle);
        let mut clip = AnimationClip::empty("turn", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::ZERO),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Quat::IDENTITY, end],
                exact_rotation: true,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let source = [ClipSource {
            clip: &clip,
            looping: false,
        }];
        let config = config_joints(1.0, vec![], vec![1.0]);
        let db = FeatureDatabase::build(&root_only(), &source, &config).expect("database");
        assert_eq!(db.frame_count(), 1, "the end sample cannot see the horizon");
        let row = db.raw_row(0).expect("row");
        assert_eq!(row.len(), 5, "{row:?}");

        let yaw_det = |rotation: Quat| {
            let forward = rotation * Vec3::NEG_Z;
            let horizontal = forward.x * forward.x + forward.z * forward.z;
            if horizontal <= 1.0e-12 {
                0.0
            } else {
                crate::num::det_atan2(forward.x, forward.z)
            }
        };
        let relative = wrap_pi(yaw_det(end) - yaw_det(Quat::IDENTITY));
        let (sin, cos) = crate::num::det_sin_cos(relative);
        assert_eq!(
            row[3].to_bits(),
            sin.to_bits(),
            "facing sine {:08x} want {:08x} (relative yaw {:08x})",
            row[3].to_bits(),
            sin.to_bits(),
            relative.to_bits()
        );
        assert_eq!(
            row[4].to_bits(),
            cos.to_bits(),
            "facing cosine {:08x} want {:08x}",
            row[4].to_bits(),
            cos.to_bits()
        );
    }

    /// `begin_blend` samples the target's previous pose `dt` earlier for the inertializer's
    /// velocity. On a looping clip at time 0 that sample wraps to `duration - dt`, one whole
    /// lap behind: for a root that travels 2 m per lap (no root-motion track, so nothing pins
    /// it) the measured target velocity is ~2 m per frame — a spike the offset inherits.
    #[test]
    fn begin_blend_at_a_loop_wrap_does_not_spike_the_target_velocity() {
        let skel = root_only();
        let clip = translation_clip(
            "lap",
            1.0,
            0,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(0.0, 0.0, -2.0))],
        );
        // Source shows the same pose as the target at t = 0, displaced 0.1 m on +Z, at rest.
        let mut source = skel.bind_pose_pose();
        clip.sample_pose(0.0, &mut source);
        source.translations[0] = Vec3::new(0.0, 0.0, 0.1);
        let blend = InertialBlend {
            skeleton: &skel,
            clips: std::slice::from_ref(&clip),
            source: &source,
            source_prev: None,
            decay: DecayMethod::Spring,
            duration: 0.25,
            dt: 1.0 / 30.0,
            root_motion: None,
        };
        let selection = PoseSelection {
            clip: 0,
            time: 0.0,
            cost: 0.0,
            blend: true,
            frame: 0,
            looping: true,
        };
        let mut inertializer = Inertializer::default();
        let mut target = Pose::from_identity(1);
        selection
            .begin_blend(&blend, &mut inertializer, &mut target)
            .expect("blend");
        assert!(inertializer.is_active());
        inertializer.advance(0.05);
        inertializer.apply(&mut target);
        // With a still target the spring offset is (x0 + y x0 t) e^(-y t) along +Z:
        // (0.1 + 30 * 0.1 * 0.05) e^-1.5 = 0.25 * 0.22313… ≈ 0.0558. The wrap bug instead
        // measures a ~58 m/s target velocity and drives the offset about -0.6 m.
        assert!(
            (target.translations[0].z - 0.0558).abs() < 0.01,
            "offset velocity spike leaked into the pose: {:?}",
            target.translations[0]
        );
    }

    /// A 1e10 s horizon on a 1 s loop used to cast the lap count to `i32::MAX` and then walk
    /// that many laps. The straight-line offset is the horizon itself (1 m/s along +X).
    #[test]
    fn looping_future_root_of_a_long_horizon_stays_on_the_line() {
        let clip = translation_clip(
            "stride",
            1.0,
            0,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(1.0, 0.0, 0.0))],
        );
        let horizon = 1.0e10_f32;
        let config = config_joints(1.0, vec![], vec![horizon]);
        let db = FeatureDatabase::build(
            &root_only(),
            &[ClipSource {
                clip: &clip,
                looping: true,
            }],
            &config,
        )
        .expect("database");
        let row = db.raw_row(0).expect("row");
        assert!(
            (row[0] - horizon).abs() <= horizon * 1.0e-5,
            "long horizon offset {row:?}"
        );
        assert!(row[1].abs() < 1.0e-3 && row[2].abs() < 1.0e-3, "{row:?}");
        assert!(row[3].abs() < 1.0e-4 && (row[4] - 1.0).abs() < 1.0e-4, "{row:?}");
    }

    #[test]
    fn joint_velocity_uses_a_forward_difference_at_the_start() {
        let clip = translation_clip(
            "step",
            1.0,
            1,
            &[
                (0.0, Vec3::new(0.0, 1.0, 0.0)),
                (0.5, Vec3::new(0.0, 1.0, 0.0)),
                (1.0, Vec3::new(0.0, 1.0, 2.0)),
            ],
        );
        let config = config_joints(0.5, vec![1], vec![]);
        let held = FeatureDatabase::build(
            &arm(),
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let start = held.raw_row(0).expect("start");
        // Forward difference across a flat first half: velocity is zero, position is the bind.
        assert!(near_vec(start, &[0.0, 1.0, 0.0, 0.0, 0.0, 0.0]), "{start:?}");
        let end = held.raw_row(2).expect("end");
        // Backward difference from t = 1 to t = 0.5: (2 - 0) / 0.5 = 4 on Z.
        assert!(near_vec(end, &[0.0, 1.0, 2.0, 0.0, 0.0, 4.0]), "{end:?}");
    }

    #[test]
    fn looping_velocity_at_the_start_looks_backward_across_the_wrap() {
        let clip = translation_clip(
            "cycle",
            1.0,
            1,
            &[(0.0, Vec3::new(0.0, 1.0, 0.0)), (1.0, Vec3::new(0.0, 1.0, 2.0))],
        );
        let config = config_joints(0.5, vec![1], vec![]);
        let looping = FeatureDatabase::build(
            &arm(),
            &[ClipSource {
                clip: &clip,
                looping: true,
            }],
            &config,
        )
        .expect("db");
        let row = looping.raw_row(0).expect("row");
        // Previous sample is t = 0.5 (z = 1). (0 - 1) / 0.5 = -2.
        assert!(near(row[5], -2.0), "{row:?}");
        let linear = FeatureDatabase::build(
            &arm(),
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let forward = linear.raw_row(0).expect("row");
        assert!(near(forward[5], 2.0), "{forward:?}");
    }

    #[test]
    fn zero_velocity_window_matches_the_sample_interval() {
        let clip = translation_clip(
            "lift",
            1.0,
            1,
            &[(0.0, Vec3::new(0.0, 1.0, 0.0)), (1.0, Vec3::new(0.0, 1.0, 2.0))],
        );
        let mut implicit = config_joints(0.5, vec![1], vec![]);
        implicit.velocity_window = 0.0;
        let mut explicit = implicit.clone();
        explicit.velocity_window = 0.5;
        let a = FeatureDatabase::build(
            &arm(),
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &implicit,
        )
        .expect("a");
        let b = FeatureDatabase::build(
            &arm(),
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &explicit,
        )
        .expect("b");
        assert_eq!(a.raw_row(0), b.raw_row(0));
        assert_eq!(a.raw_row(1), b.raw_row(1));
    }

    #[test]
    fn root_frame_removes_root_translation_and_yaw() {
        let binds = vec![
            LocalBind {
                translation: Vec3::new(10.0, 0.0, 0.0),
                rotation: Quat::from_rotation_y(PI * 0.5),
                scale: Vec3::ONE,
            },
            LocalBind {
                translation: Vec3::new(0.0, 0.0, -1.0),
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
            },
        ];
        let skel = skeleton(binds);
        let clip = AnimationClip::empty("bind", 0.0);
        let config = config_joints(0.5, vec![1], vec![]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let row = db.raw_row(0).expect("row");
        // Global position would be the rotated offset plus the root translation. Root-relative is local.
        assert!(near_vec(row, &[0.0, 0.0, -1.0, 0.0, 0.0, 0.0]), "{row:?}");
        assert_eq!(db.clip(0).expect("clip").name, "bind");
        assert!(!db.clip(0).expect("clip").looping);
    }

    #[test]
    fn degenerate_root_and_non_finite_tracks_are_errors() {
        let flat = LocalBind {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ZERO,
        };
        let skel = skeleton(vec![flat, translated(Vec3::Y)]);
        let clip = AnimationClip::empty("flat", 0.2);
        let config = config_joints(0.2, vec![1], vec![]);
        assert!(matches!(
            FeatureDatabase::build(
                &skel,
                &[ClipSource {
                    clip: &clip,
                    looping: false
                }],
                &config
            ),
            Err(PoseSearchError::DegenerateRoot { .. })
        ));
        let nan = translation_clip("nan", 0.5, 1, &[(0.0, Vec3::new(0.0, 1.0, f32::NAN))]);
        assert!(matches!(
            FeatureDatabase::build(
                &arm(),
                &[ClipSource {
                    clip: &nan,
                    looping: false
                }],
                &config
            ),
            Err(PoseSearchError::NonFiniteFeature { .. })
        ));
    }

    #[test]
    fn group_weights_zero_the_position_channels() {
        let clip = AnimationClip::empty("still", 0.0);
        let mut config = config_joints(0.5, vec![1], vec![]);
        config.position_weight = 0.0;
        config.velocity_weight = 2.0;
        let db = FeatureDatabase::build(
            &arm(),
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        assert_eq!(db.weights(), &[0.0, 0.0, 0.0, 2.0, 2.0, 2.0]);
        assert_eq!(db.dimensions(), 6);
    }

    /// Audit (corr-animation): an f32 running mean of a constant channel missed by an ULP, the
    /// channel got a ~1e-7 divisor, and its huge term made every row tie on frame 0.
    #[test]
    fn a_constant_channel_over_many_frames_stays_constant_and_does_not_swamp_the_search() {
        let clips = [DatabaseClip {
            name: "walk".into(),
            duration: 1.0,
            looping: false,
        }];
        let frames: Vec<RawFrame> = (0..10)
            .map(|i| RawFrame {
                clip: 0,
                time: i as f32 * 0.1,
                feature: vec![i as f32, 1.0],
            })
            .collect();
        let db = FeatureDatabase::from_raw(&clips, &frames, &[]).expect("db");
        assert_eq!(db.mean()[1], 1.0);
        assert_eq!(db.deviation()[1], 0.0);
        for method in [SearchMethod::BruteForce, SearchMethod::KdTree] {
            let hit = db.nearest(&[7.0, 0.99], method).expect("hit");
            assert_eq!(hit.frame, 7, "{method:?}");
        }
    }

    /// Audit (corr-animation): a root carrying a Z-up basis rotation (−Z points down) lost its
    /// yaw, so a quarter turn read as no turn at all.
    #[test]
    fn relative_yaw_of_a_quarter_turn_on_a_z_up_root_is_sine_one() {
        let z_up = Quat::from_rotation_x(-PI * 0.5);
        let mut clip = AnimationClip::empty("turn", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Static(Vec3::ZERO),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![z_up, Quat::from_rotation_y(PI * 0.5) * z_up],
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let source = [ClipSource {
            clip: &clip,
            looping: false,
        }];
        let config = config_joints(1.0, vec![], vec![1.0]);
        let db = FeatureDatabase::build(&root_only(), &source, &config).expect("database");
        let row = db.raw_row(0).expect("row");
        assert!(near_vec(&row[3..5], &[1.0, 0.0]), "{row:?}");
    }

    #[test]
    fn constant_channel_uses_divisor_one_and_centres_the_row() {
        let clips = [DatabaseClip {
            name: "only".into(),
            duration: 1.0,
            looping: false,
        }];
        let frames = [RawFrame {
            clip: 0,
            time: 0.0,
            feature: vec![3.0, 5.0],
        }];
        let db = FeatureDatabase::from_raw(&clips, &frames, &[]).expect("db");
        assert_eq!(db.mean(), &[3.0, 5.0]);
        assert!(db.deviation().iter().all(|value| *value <= DEVIATION_EPSILON));
        assert_eq!(db.weighted_row(0).expect("row"), &[0.0, 0.0]);
        let same = db.nearest(&[3.0, 5.0], SearchMethod::KdTree).expect("hit");
        assert_eq!(same.frame, 0);
        assert!(near(same.cost, 0.0));
        let shifted = db.cost(0, &[4.0, 5.0]).expect("cost");
        assert!(near(shifted, 1.0), "{shifted}");
    }

    #[test]
    fn population_deviation_of_two_points_is_half_the_gap() {
        let clips = [DatabaseClip {
            name: "pair".into(),
            duration: 1.0,
            looping: false,
        }];
        let frames = [
            RawFrame {
                clip: 0,
                time: 0.0,
                feature: vec![0.0],
            },
            RawFrame {
                clip: 0,
                time: 1.0,
                feature: vec![2.0],
            },
        ];
        let db = FeatureDatabase::from_raw(&clips, &frames, &[2.0]).expect("db");
        assert!(near(db.mean()[0], 1.0));
        assert!(near(db.deviation()[0], 1.0));
        assert!(near_vec(db.weighted_row(0).expect("a"), &[-2.0]));
        assert!(near_vec(db.weighted_row(1).expect("b"), &[2.0]));
        let tie = db.nearest(&[1.0], SearchMethod::BruteForce).expect("tie");
        let tie_tree = db.nearest(&[1.0], SearchMethod::KdTree).expect("tie tree");
        assert_eq!(tie.frame, 0);
        assert_eq!(tie_tree.frame, tie.frame);
        assert_eq!(tie.cost.to_bits(), tie_tree.cost.to_bits());
    }

    #[test]
    fn channel_weights_change_which_frame_wins() {
        let clips = [DatabaseClip {
            name: "poses".into(),
            duration: 1.0,
            looping: false,
        }];
        let frames = [
            RawFrame {
                clip: 0,
                time: 0.0,
                feature: vec![0.0, 0.0],
            },
            RawFrame {
                clip: 0,
                time: 0.5,
                feature: vec![5.0, 0.0],
            },
            RawFrame {
                clip: 0,
                time: 1.0,
                feature: vec![0.0, 5.0],
            },
        ];
        let query = [4.0, 1.0];
        let by_x = FeatureDatabase::from_raw(&clips, &frames, &[1.0, 0.0]).expect("x");
        let by_y = FeatureDatabase::from_raw(&clips, &frames, &[0.0, 1.0]).expect("y");
        let x_hit = by_x.nearest(&query, SearchMethod::KdTree).expect("x hit");
        let y_hit = by_y.nearest(&query, SearchMethod::BruteForce).expect("y hit");
        assert_eq!(x_hit.frame, 1);
        assert_eq!(y_hit.frame, 0);
        assert_eq!(
            x_hit.frame,
            by_x.nearest(&query, SearchMethod::BruteForce)
                .expect("x brute")
                .frame
        );
    }

    #[test]
    fn from_raw_rejects_malformed_rows() {
        let clips = [DatabaseClip {
            name: "a".into(),
            duration: 1.0,
            looping: false,
        }];
        assert!(matches!(
            FeatureDatabase::from_raw(&[], &[], &[]),
            Err(PoseSearchError::NoClips)
        ));
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, &[], &[]),
            Err(PoseSearchError::NoFrames)
        ));
        let empty = [RawFrame {
            clip: 0,
            time: 0.0,
            feature: vec![],
        }];
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, &empty, &[]),
            Err(PoseSearchError::NoFeatures)
        ));
        let bad_time = [RawFrame {
            clip: 0,
            time: -0.1,
            feature: vec![1.0],
        }];
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, &bad_time, &[]),
            Err(PoseSearchError::BadFrameTime { index: 0, .. })
        ));
        let bad_clip = [RawFrame {
            clip: 3,
            time: 0.0,
            feature: vec![1.0],
        }];
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, &bad_clip, &[]),
            Err(PoseSearchError::RawClipOutOfRange { clip: 3, .. })
        ));
        let mismatch = [
            RawFrame {
                clip: 0,
                time: 0.0,
                feature: vec![1.0, 2.0],
            },
            RawFrame {
                clip: 0,
                time: 0.2,
                feature: vec![1.0],
            },
        ];
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, &mismatch, &[]),
            Err(PoseSearchError::RawFeatureMismatch {
                index: 1,
                got: 1,
                expected: 2
            })
        ));
        let nan = [RawFrame {
            clip: 0,
            time: 0.0,
            feature: vec![f32::NAN],
        }];
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, &nan, &[]),
            Err(PoseSearchError::NonFiniteFeature { .. })
        ));
        let row = [RawFrame {
            clip: 0,
            time: 0.0,
            feature: vec![1.0],
        }];
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, &row, &[1.0, 2.0]),
            Err(PoseSearchError::WeightLengthMismatch { .. })
        ));
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, &row, &[-1.0]),
            Err(PoseSearchError::BadChannelWeight { channel: 0 })
        ));
        let bad_duration = [DatabaseClip {
            name: "a".into(),
            duration: f32::INFINITY,
            looping: false,
        }];
        assert!(matches!(
            FeatureDatabase::from_raw(&bad_duration, &row, &[]),
            Err(PoseSearchError::BadClipDuration { .. })
        ));
    }

    #[test]
    fn nearest_query_errors_and_exact_row_has_zero_cost() {
        let clips = [DatabaseClip {
            name: "a".into(),
            duration: 1.0,
            looping: false,
        }];
        let frames = [RawFrame {
            clip: 0,
            time: 0.25,
            feature: vec![1.0, -2.0],
        }];
        let db = FeatureDatabase::from_raw(&clips, &frames, &[]).expect("db");
        assert!(matches!(
            db.nearest(&[1.0], SearchMethod::BruteForce),
            Err(PoseSearchError::FeatureLengthMismatch { got: 1, expected: 2 })
        ));
        assert!(matches!(
            db.nearest(&[1.0, f32::NAN], SearchMethod::KdTree),
            Err(PoseSearchError::NonFiniteQuery)
        ));
        assert!(matches!(
            db.cost(4, &[1.0, -2.0]),
            Err(PoseSearchError::FrameOutOfRange { index: 4 })
        ));
        assert!(db.raw_row(4).is_none());
        let hit = db.nearest(&[1.0, -2.0], SearchMethod::KdTree).expect("hit");
        assert_eq!(hit.frame, 0);
        assert_eq!(hit.clip, 0);
        assert!(near(hit.time, 0.25));
        assert!(near(hit.cost, 0.0));
        assert!(matches!(
            db.feature_at(&root_only(), &AnimationClip::empty("x", 1.0), 0.0, false),
            Err(PoseSearchError::NoPoseLayout)
        ));
        assert!(matches!(
            db.query_from_pose(&root_only(), &Pose::from_identity(1), None, &[]),
            Err(PoseSearchError::NoPoseLayout)
        ));
    }

    #[test]
    fn feature_at_matches_the_stored_row_and_query_from_pose() {
        let clip = translation_clip(
            "arm",
            1.0,
            1,
            &[(0.0, Vec3::new(0.0, 1.0, 0.0)), (1.0, Vec3::new(0.0, 1.0, 2.0))],
        );
        let skel = arm();
        let config = config_joints(0.5, vec![1], vec![]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        for index in 0..db.frame_count() {
            let frame = db.frame(index as u32).expect("frame");
            let again = db.feature_at(&skel, &clip, frame.time, false).expect("feature");
            assert_eq!(again.as_slice(), db.raw_row(index as u32).expect("raw"));
            let hit = db.nearest(&again, SearchMethod::BruteForce).expect("brute");
            let tree = db.nearest(&again, SearchMethod::KdTree).expect("tree");
            assert_eq!(hit.frame, tree.frame);
            assert_eq!(hit.cost.to_bits(), tree.cost.to_bits());
            assert!(near(hit.cost, 0.0));
        }
        let pose = skel.bind_pose_pose();
        let query = db.query_from_pose(&skel, &pose, None, &[]).expect("query");
        // No previous pose: velocities are zero, position is the bind, which is frame 0's position
        // but not its forward-difference velocity. The position channels still match frame 0.
        let raw = db.raw_row(0).expect("raw");
        assert!(near(query[0], raw[0]) && near(query[1], raw[1]) && near(query[2], raw[2]));
        assert!(near_vec(&query[3..6], &[0.0, 0.0, 0.0]));
        assert!(matches!(
            db.feature_at(&skel, &AnimationClip::empty("bad", f32::NAN), 0.0, false),
            Err(PoseSearchError::BadClipDuration { .. })
        ));
        assert!(matches!(
            db.feature_at(&skel, &clip, -0.2, false),
            Err(PoseSearchError::BadTime { .. })
        ));
    }

    #[test]
    fn query_from_pose_checks_the_live_pose() {
        let clip = AnimationClip::empty("bind", 0.5);
        let skel = arm();
        let config = config_joints(0.5, vec![1], vec![]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let pose = skel.bind_pose_pose();
        assert!(matches!(
            db.query_from_pose(&root_only(), &pose, None, &[]),
            Err(PoseSearchError::PoseMismatch { .. })
        ));
        let short = Pose::from_identity(1);
        assert!(matches!(
            db.query_from_pose(&skel, &short, None, &[]),
            Err(PoseSearchError::PoseMismatch { .. })
        ));
        assert!(matches!(
            db.query_from_pose(&skel, &pose, Some(&short), &[]),
            Err(PoseSearchError::PoseMismatch { .. })
        ));
        let mut earlier = pose.clone();
        earlier.translations[1].z = 1.0;
        let with_prev = db
            .query_from_pose(&skel, &pose, Some(&earlier), &[])
            .expect("prev");
        // Window is the 0.5 s sample interval. (0 - 1) / 0.5 = -2 on Z.
        assert!(near(with_prev[5], -2.0), "{with_prev:?}");
        let mut scaled = pose.clone();
        scaled.scales[0] = Vec3::ZERO;
        assert!(matches!(
            db.query_from_pose(&skel, &scaled, None, &[]),
            Err(PoseSearchError::DegenerateRoot { .. })
        ));
        let horizon = config_joints(0.5, vec![1], vec![0.0]);
        let with_horizon = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: true,
            }],
            &horizon,
        )
        .expect("horizon db");
        assert!(matches!(
            with_horizon.query_from_pose(&skel, &pose, None, &[]),
            Err(PoseSearchError::TrajectoryMismatch { got: 0, expected: 1 })
        ));
        let bad_future = [TrajectorySample {
            offset: Vec3::splat(f32::NAN),
            yaw: 0.0,
        }];
        assert!(matches!(
            with_horizon.query_from_pose(&skel, &pose, None, &bad_future),
            Err(PoseSearchError::NonFiniteQuery)
        ));
    }

    #[test]
    fn identical_rows_and_seeded_sets_agree_between_searches() {
        let clips = [DatabaseClip {
            name: "dup".into(),
            duration: 1.0,
            looping: false,
        }];
        let frames = [
            RawFrame {
                clip: 0,
                time: 0.0,
                feature: vec![1.0, 1.0, 1.0],
            },
            RawFrame {
                clip: 0,
                time: 0.2,
                feature: vec![1.0, 1.0, 1.0],
            },
            RawFrame {
                clip: 0,
                time: 0.4,
                feature: vec![1.0, 1.0, 1.0],
            },
        ];
        let db = FeatureDatabase::from_raw(&clips, &frames, &[]).expect("db");
        let hit = db.nearest(&[1.0, 1.0, 1.0], SearchMethod::KdTree).expect("hit");
        assert_eq!(hit.frame, 0);
        assert_eq!(
            hit.frame,
            db.nearest(&[1.0, 1.0, 1.0], SearchMethod::BruteForce)
                .expect("brute")
                .frame
        );

        let mut rng = Lcg(0xC0FFEE_u64);
        for case in 0..24 {
            let dims = 1 + (rng.next_u32() as usize % 6);
            let count = 1 + (rng.next_u32() as usize % 28);
            let mut drawn: Vec<RawFrame> = Vec::with_capacity(count);
            for index in 0..count {
                let mut feature: Vec<f32> = Vec::with_capacity(dims);
                for _ in 0..dims {
                    feature.push(rng.unit() * 4.0 - 2.0);
                }
                if index > 0 && rng.next_u32().is_multiple_of(5) {
                    feature.clone_from(&drawn[index - 1].feature);
                }
                drawn.push(RawFrame {
                    clip: 0,
                    // Spread the rows inside the clip: a raw frame time must stay within its
                    // clip's duration.
                    time: index as f32 * (1.0 / count as f32),
                    feature,
                });
            }
            let weights: Vec<f32> = (0..dims).map(|_| 0.25 + rng.unit() * 2.0).collect();
            let built = FeatureDatabase::from_raw(&clips, &drawn, &weights).expect("built");
            for _ in 0..5 {
                let query: Vec<f32> = (0..dims).map(|_| rng.unit() * 4.0 - 2.0).collect();
                assert_same(&built, &query, case);
            }
            for frame in &drawn {
                assert_same(&built, &frame.feature, case);
            }
        }
    }

    fn assert_same(db: &FeatureDatabase, query: &[f32], case: u32) {
        let brute = db.nearest(query, SearchMethod::BruteForce).expect("brute");
        let tree = db.nearest(query, SearchMethod::KdTree).expect("tree");
        let manual = manual_nearest(db, query);
        assert_eq!(brute.frame, tree.frame, "case {case} brute/tree");
        assert_eq!(brute.cost.to_bits(), tree.cost.to_bits(), "case {case} cost bits");
        assert_eq!(brute.frame, manual.0, "case {case} manual");
        assert_eq!(
            brute.cost.to_bits(),
            manual.1.to_bits(),
            "case {case} manual cost"
        );
    }

    fn manual_nearest(db: &FeatureDatabase, query: &[f32]) -> (u32, f32) {
        let weighted = db.normalize(query).expect("normalize");
        let mut best_index = 0u32;
        let mut best_cost = f32::MAX;
        for index in 0..db.frame_count() {
            let row = db.weighted_row(index as u32).expect("row");
            let mut cost = 0.0f32;
            for (left, right) in row.iter().zip(&weighted) {
                let delta = left - right;
                cost += delta * delta;
            }
            if index == 0 || cost < best_cost {
                best_cost = cost;
                best_index = index as u32;
            }
        }
        (best_index, best_cost)
    }

    struct Lcg(u64);

    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (self.0 >> 32) as u32
        }

        fn unit(&mut self) -> f32 {
            (self.next_u32() >> 8) as f32 / 16_777_216.0
        }
    }

    fn replay_db() -> FeatureDatabase {
        let clips = [
            DatabaseClip {
                name: "walk".into(),
                duration: 1.0,
                looping: false,
            },
            DatabaseClip {
                name: "idle".into(),
                duration: 1.0,
                looping: false,
            },
        ];
        let frames = [
            RawFrame {
                clip: 0,
                time: 0.0,
                feature: vec![0.0],
            },
            RawFrame {
                clip: 0,
                time: 0.5,
                feature: vec![1.0],
            },
            RawFrame {
                clip: 0,
                time: 1.0,
                feature: vec![4.0],
            },
            RawFrame {
                clip: 1,
                time: 0.0,
                feature: vec![10.0],
            },
        ];
        FeatureDatabase::from_raw(&clips, &frames, &[]).expect("replay db")
    }

    #[test]
    fn replay_threshold_blocks_same_clip_jumps_until_the_cost_improves() {
        let db = replay_db();
        let q0 = db.raw_row(0).expect("q0").to_vec();
        let q2 = db.raw_row(2).expect("q2").to_vec();
        let improvement = db.cost(0, &q2).expect("stay") - db.cost(2, &q2).expect("jump");
        assert!(improvement > 0.0);

        let mut first = SearchState::new(0.0).expect("state");
        let opened = first.search(&db, &q0, SearchMethod::BruteForce).expect("open");
        assert!(opened.blend);
        assert_eq!(opened.frame, 0);
        let again = first.search(&db, &q0, SearchMethod::KdTree).expect("again");
        assert!(!again.blend);
        assert_eq!(again.time, 0.0);

        let mut held = SearchState::new(improvement).expect("held");
        held.search(&db, &q0, SearchMethod::KdTree).expect("seed");
        let kept = held.search(&db, &q2, SearchMethod::BruteForce).expect("kept");
        assert!(!kept.blend);
        assert_eq!(kept.time, 0.0);
        assert_eq!(kept.clip, 0);

        let mut moved = SearchState::new(improvement * 0.5).expect("moved");
        moved.search(&db, &q0, SearchMethod::BruteForce).expect("seed");
        let jumped = moved.search(&db, &q2, SearchMethod::KdTree).expect("jumped");
        assert!(jumped.blend);
        assert_eq!(jumped.frame, 2);
        assert!(near(jumped.time, 1.0));
        let settled = moved.search(&db, &q2, SearchMethod::BruteForce).expect("settled");
        assert!(!settled.blend);
        assert_eq!(settled.frame, 2);

        let q_idle = db.raw_row(3).expect("idle").to_vec();
        let mut sticky = SearchState::new(1.0e9).expect("sticky");
        sticky.search(&db, &q0, SearchMethod::BruteForce).expect("seed");
        let switched = sticky.search(&db, &q_idle, SearchMethod::KdTree).expect("switch");
        assert!(switched.blend);
        assert_eq!(switched.clip, 1);
        let stay_idle = sticky
            .search(&db, &q_idle, SearchMethod::BruteForce)
            .expect("stay idle");
        assert!(!stay_idle.blend);

        let brute_state = run_sequence(&db, SearchMethod::BruteForce, &[&q0, &q2, &q_idle]);
        let tree_state = run_sequence(&db, SearchMethod::KdTree, &[&q0, &q2, &q_idle]);
        assert_eq!(brute_state, tree_state);
    }

    fn run_sequence(db: &FeatureDatabase, method: SearchMethod, queries: &[&[f32]]) -> Vec<PoseSelection> {
        let mut state = SearchState::new(0.0).expect("state");
        queries
            .iter()
            .map(|query| state.search(db, query, method).expect("search"))
            .collect()
    }

    #[test]
    fn search_state_rejects_bad_thresholds_and_advances_the_clock() {
        assert!(matches!(
            SearchState::new(-0.1),
            Err(PoseSearchError::BadThreshold { .. })
        ));
        assert!(matches!(
            SearchState::new(f32::NAN),
            Err(PoseSearchError::BadThreshold { .. })
        ));
        let db = replay_db();
        let mut state = SearchState::new(0.0).expect("state");
        assert!(state.advance(&db, 0.25).is_ok());
        assert!(state.current().is_none());
        let query = db.raw_row(0).expect("row").to_vec();
        state
            .search(&db, &query, SearchMethod::BruteForce)
            .expect("search");
        let before = state.current().expect("current").time;
        assert!(matches!(
            state.advance(&db, f32::NAN),
            Err(PoseSearchError::BadDelta { .. })
        ));
        assert_eq!(state.current().expect("current").time, before);
        state.advance(&db, 0.75).expect("clamp");
        assert!(near(state.current().expect("current").time, 0.75));
        state.advance(&db, 1.0).expect("end");
        assert!(near(state.current().expect("current").time, 1.0));
        state.advance(&db, -2.0).expect("floor");
        assert!(near(state.current().expect("current").time, 0.0));

        let looping_clips = [DatabaseClip {
            name: "loop".into(),
            duration: 1.0,
            looping: true,
        }];
        let frames = [RawFrame {
            clip: 0,
            time: 0.25,
            feature: vec![1.0],
        }];
        let loop_db = FeatureDatabase::from_raw(&looping_clips, &frames, &[]).expect("loop");
        let mut loop_state = SearchState::new(0.0).expect("state");
        loop_state
            .search(&loop_db, &[1.0], SearchMethod::KdTree)
            .expect("search");
        loop_state.advance(&loop_db, 1.0).expect("wrap");
        // 0.25 + 1 = 1.25, wrapped into a 1 s clip.
        assert!(near(loop_state.current().expect("current").time, 0.25));
        loop_state.clear();
        assert!(loop_state.current().is_none());
        let bad = db.nearest(&[0.0], SearchMethod::BruteForce).expect("hit");
        assert!(matches!(
            db.cost(bad.frame, &[f32::INFINITY]),
            Err(PoseSearchError::NonFiniteQuery)
        ));
    }

    #[test]
    fn looping_continue_frame_uses_wrapped_time() {
        let clips = [DatabaseClip {
            name: "loop".into(),
            duration: 1.0,
            looping: true,
        }];
        let frames = [
            RawFrame {
                clip: 0,
                time: 0.0,
                feature: vec![0.0],
            },
            RawFrame {
                clip: 0,
                time: 0.8,
                feature: vec![10.0],
            },
        ];
        let db = FeatureDatabase::from_raw(&clips, &frames, &[]).expect("db");
        let mut state = SearchState::new(1.0e9).expect("state");
        state.search(&db, &[0.0], SearchMethod::BruteForce).expect("open");
        state.advance(&db, 0.95).expect("near the wrap");
        assert!(near(state.current().expect("current").time, 0.95));
        let kept = state.search(&db, &[10.0], SearchMethod::KdTree).expect("kept");
        // 0.95 is 0.05 from frame 0 across the wrap, and 0.15 from the frame at 0.8.
        assert!(!kept.blend);
        assert_eq!(kept.frame, 0);
        assert!(near(kept.time, 0.95));
    }

    #[test]
    fn begin_blend_samples_the_clip_and_starts_the_inertializer() {
        let skel = arm();
        let clip = translation_clip(
            "lift",
            1.0,
            1,
            &[(0.0, Vec3::new(0.0, 1.0, 0.0)), (1.0, Vec3::new(0.0, 3.0, 0.0))],
        );
        let config = config_joints(1.0, vec![1], vec![]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let at_end = db.feature_at(&skel, &clip, 1.0, false).expect("feature");
        let mut state = SearchState::default();
        let selection = state.search(&db, &at_end, SearchMethod::KdTree).expect("search");
        assert!(selection.blend);
        assert!(near(selection.time, 1.0));
        let source = skel.bind_pose_pose();
        let blend = InertialBlend {
            skeleton: &skel,
            clips: std::slice::from_ref(&clip),
            source: &source,
            source_prev: None,
            decay: DecayMethod::Quintic,
            duration: 0.25,
            dt: 1.0 / 30.0,
            root_motion: None,
        };
        let mut inertializer = Inertializer::default();
        let mut target = Pose::from_identity(2);
        selection
            .begin_blend(&blend, &mut inertializer, &mut target)
            .expect("blend");
        assert!(inertializer.is_active());
        assert!(near(target.translations[1].y, 3.0));
        inertializer.apply(&mut target);
        assert!(near(target.translations[1].y, source.translations[1].y));

        let mut quiet = selection;
        quiet.blend = false;
        let mut idle = Inertializer::default();
        let mut shown = Pose::from_identity(2);
        quiet
            .begin_blend(&blend, &mut idle, &mut shown)
            .expect("continue");
        assert!(!idle.is_active());
        assert!(near(shown.translations[1].y, 3.0));
    }

    #[test]
    fn begin_blend_leaves_outputs_untouched_on_error() {
        let skel = arm();
        let clip = AnimationClip::empty("idle", 1.0);
        let source = skel.bind_pose_pose();
        let blend = InertialBlend {
            skeleton: &skel,
            clips: std::slice::from_ref(&clip),
            source: &source,
            source_prev: None,
            decay: DecayMethod::Spring,
            duration: 0.2,
            dt: 0.016,
            root_motion: None,
        };
        let mut selection = PoseSelection {
            clip: 4,
            time: 0.0,
            cost: 0.0,
            blend: true,
            frame: 0,
            looping: false,
        };
        let mut inertializer = Inertializer::default();
        let mut target = source.clone();
        target.translations[1].y = 8.0;
        assert!(matches!(
            selection.begin_blend(&blend, &mut inertializer, &mut target),
            Err(PoseSearchError::ClipOutOfRange { clip: 4, count: 1 })
        ));
        assert!(!inertializer.is_active());
        assert!(near(target.translations[1].y, 8.0));

        selection.clip = 0;
        selection.time = -1.0;
        assert!(matches!(
            selection.begin_blend(&blend, &mut inertializer, &mut target),
            Err(PoseSearchError::BadTime { .. })
        ));
        selection.time = f32::NAN;
        assert!(matches!(
            selection.begin_blend(&blend, &mut inertializer, &mut target),
            Err(PoseSearchError::BadTime { .. })
        ));
        selection.time = 0.0;
        let mut bad_dt = blend;
        bad_dt.dt = -0.1;
        assert!(matches!(
            selection.begin_blend(&bad_dt, &mut inertializer, &mut target),
            Err(PoseSearchError::BadDelta { .. })
        ));
        bad_dt.dt = f32::INFINITY;
        assert!(matches!(
            selection.begin_blend(&bad_dt, &mut inertializer, &mut target),
            Err(PoseSearchError::BadDelta { .. })
        ));
        let mut bad_duration = blend;
        bad_duration.duration = -0.2;
        assert!(matches!(
            selection.begin_blend(&bad_duration, &mut inertializer, &mut target),
            Err(PoseSearchError::BadBlendDuration { .. })
        ));
        let empty = InertialBlend {
            skeleton: &Skeleton::default(),
            ..blend
        };
        assert!(matches!(
            selection.begin_blend(&empty, &mut inertializer, &mut target),
            Err(PoseSearchError::EmptySkeleton)
        ));
        assert!(near(target.translations[1].y, 8.0));
        assert!(!inertializer.is_active());

        let nan = translation_clip("nan", 0.5, 1, &[(0.0, Vec3::new(0.0, f32::NAN, 0.0))]);
        let nan_blend = InertialBlend {
            clips: std::slice::from_ref(&nan),
            ..blend
        };
        assert!(matches!(
            selection.begin_blend(&nan_blend, &mut inertializer, &mut target),
            Err(PoseSearchError::NonFinitePose)
        ));
        assert!(near(target.translations[1].y, 8.0));
    }

    /// A clip whose root travels 2 m over 1 s with `RootMotionMode::XZ`, as the review's
    /// failure case: the blend target must stay pinned at the origin, including across the
    /// loop wrap.
    #[test]
    fn begin_blend_pins_a_travelling_root() {
        let skel = arm();
        let mut clip = translation_clip(
            "walk",
            1.0,
            0,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(2.0, 0.0, 0.0))],
        );
        clip.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XZ,
        });
        let source = skel.bind_pose_pose();
        let blend = InertialBlend {
            skeleton: &skel,
            clips: std::slice::from_ref(&clip),
            source: &source,
            source_prev: None,
            decay: DecayMethod::Quintic,
            duration: 0.25,
            dt: 1.0 / 30.0,
            root_motion: None,
        };
        let mut inertializer = Inertializer::default();
        let mut target = Pose::from_identity(2);
        // A jump at 0.8 s: the clip's root sits at x = 1.6; the pinned target must not.
        let jump = PoseSelection {
            clip: 0,
            time: 0.8,
            cost: 0.0,
            blend: true,
            frame: 8,
            looping: true,
        };
        jump.begin_blend(&blend, &mut inertializer, &mut target)
            .expect("blend");
        assert!(inertializer.is_active());
        assert!(
            near(target.translations[0].x, 0.0),
            "x = {}",
            target.translations[0].x
        );
        assert!(near(target.translations[0].z, 0.0));
        inertializer.apply(&mut target);
        assert!(
            near(target.translations[0].x, 0.0),
            "x = {}",
            target.translations[0].x
        );

        // Continuing playback after a wrap across the end: without pinning the visible root
        // would reset from x = 1.9 to x = 0.1; pinned, both stay at the origin.
        for time in [0.95f32, 0.05] {
            let continued = PoseSelection {
                time,
                blend: false,
                ..jump
            };
            let mut idle = Inertializer::default();
            let mut shown = Pose::from_identity(2);
            continued
                .begin_blend(&blend, &mut idle, &mut shown)
                .expect("continue");
            assert!(!idle.is_active());
            assert!(
                near(shown.translations[0].x, 0.0),
                "t = {time}: x = {}",
                shown.translations[0].x
            );
        }
    }

    /// Looping trajectory extension composes each lap with the cycle transform: the lap
    /// displacement turns with the cycle yaw instead of staying on the first lap's axes.
    #[test]
    fn looping_trajectory_rotates_each_lap() {
        let skel = root_only();
        let quarter = std::f32::consts::FRAC_PI_2;
        let mut clip = AnimationClip::empty("turn", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Quat::IDENTITY, Quat::from_rotation_y(quarter)],
                exact_rotation: true,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let config = config_joints(0.1, vec![], vec![0.3, 1.5]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: true,
            }],
            &config,
        )
        .expect("db");
        let row = db.feature_at(&skel, &clip, 0.9, true).expect("feature");

        // Cycle: d = (1, 0, 0), delta_yaw = 90 deg, start = origin.
        // One lap to local 0.2: pos = R(90 deg) * p(0.2) + d = (1, 0, -0.2).
        // Two laps to local 0.4: pos = R(180 deg) * p(0.4) + (I + R(90 deg)) * d = (0.6, 0, -1).
        // The offset channels are in the current root frame (root at (0.9, 0, 0), yaw 81 deg).
        let rot = Mat4::from_rotation_y(-0.9 * quarter);
        let one_lap = rot.transform_point3(Vec3::new(1.0, 0.0, -0.2) - Vec3::new(0.9, 0.0, 0.0));
        let two_laps = rot.transform_point3(Vec3::new(0.6, 0.0, -1.0) - Vec3::new(0.9, 0.0, 0.0));
        for (channel, expected) in one_lap.to_array().iter().enumerate() {
            assert!(
                (row[channel] - expected).abs() < 2.0e-3,
                "horizon 1 channel {channel}: {} vs {expected}",
                row[channel]
            );
        }
        for (channel, expected) in two_laps.to_array().iter().enumerate() {
            assert!(
                (row[5 + channel] - expected).abs() < 2.0e-3,
                "horizon 2 channel {channel}: {} vs {expected}",
                row[5 + channel]
            );
        }
    }

    #[test]
    fn feature_at_rejects_times_the_build_would_not_store() {
        let skel = root_only();
        let clip = translation_clip(
            "move",
            1.0,
            0,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(4.0, 0.0, 0.0))],
        );
        let config = config_joints(0.5, vec![], vec![0.6]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        // The build drops frames past duration - max_horizon; querying one must fail.
        assert!(matches!(
            db.feature_at(&skel, &clip, 0.5, false),
            Err(PoseSearchError::BadTime { .. })
        ));
        assert!(db.feature_at(&skel, &clip, 0.4, false).is_ok());
        // Past the clip end, looping or not.
        assert!(matches!(
            db.feature_at(&skel, &clip, 2.0, false),
            Err(PoseSearchError::BadTime { .. })
        ));
        assert!(matches!(
            db.feature_at(&skel, &clip, 2.0, true),
            Err(PoseSearchError::BadTime { .. })
        ));
        // Raw frames obey their clip's duration too.
        let clips = [DatabaseClip {
            name: "c".to_string(),
            duration: 1.0,
            looping: false,
        }];
        let good = RawFrame {
            clip: 0,
            time: 1.0,
            feature: vec![1.0, 1.0],
        };
        assert!(FeatureDatabase::from_raw(&clips, std::slice::from_ref(&good), &[]).is_ok());
        let past_end = RawFrame {
            clip: 0,
            time: 2.0,
            feature: vec![1.0, 1.0],
        };
        assert!(matches!(
            FeatureDatabase::from_raw(&clips, std::slice::from_ref(&past_end), &[]),
            Err(PoseSearchError::BadFrameTime { .. })
        ));
    }

    #[test]
    fn partial_poses_return_pose_mismatch_instead_of_panicking() {
        let skel = arm();
        let clip = translation_clip(
            "move",
            1.0,
            0,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(4.0, 0.0, 0.0))],
        );
        let config = config_joints(0.5, vec![1], vec![]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let good = skel.bind_pose_pose();
        let future = [];
        let mut short_rotations = good.clone();
        short_rotations.rotations.truncate(1);
        assert!(matches!(
            db.query_from_pose(&skel, &short_rotations, None, &future),
            Err(PoseSearchError::PoseMismatch { .. })
        ));
        let mut short_scales = good.clone();
        short_scales.scales.truncate(1);
        assert!(matches!(
            db.query_from_pose(&skel, &short_scales, None, &future),
            Err(PoseSearchError::PoseMismatch { .. })
        ));
        let mut short_prev = good.clone();
        short_prev.rotations.truncate(1);
        assert!(matches!(
            db.query_from_pose(&skel, &good, Some(&short_prev), &future),
            Err(PoseSearchError::PoseMismatch { .. })
        ));

        let selection = PoseSelection {
            clip: 0,
            time: 0.0,
            cost: 0.0,
            blend: true,
            frame: 0,
            looping: false,
        };
        let mut inertializer = Inertializer::default();
        let mut target = good.clone();
        let bad_source = InertialBlend {
            skeleton: &skel,
            clips: std::slice::from_ref(&clip),
            source: &short_rotations,
            source_prev: None,
            decay: DecayMethod::Spring,
            duration: 0.2,
            dt: 1.0 / 30.0,
            root_motion: None,
        };
        assert!(matches!(
            selection.begin_blend(&bad_source, &mut inertializer, &mut target),
            Err(PoseSearchError::PoseMismatch { .. })
        ));
        let bad_prev = InertialBlend {
            source: &good,
            source_prev: Some(&short_scales),
            ..bad_source
        };
        assert!(matches!(
            selection.begin_blend(&bad_prev, &mut inertializer, &mut target),
            Err(PoseSearchError::PoseMismatch { .. })
        ));
    }

    #[test]
    fn advance_recomputes_the_nearest_frame() {
        let skel = arm();
        let clip = translation_clip(
            "move",
            1.0,
            1,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(4.0, 0.0, 0.0))],
        );
        let config = config_joints(0.5, vec![1], vec![]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let mut state = SearchState::default();
        let selection = state
            .search(
                &db,
                &db.feature_at(&skel, &clip, 0.0, false).expect("q"),
                SearchMethod::BruteForce,
            )
            .expect("search");
        assert_eq!(selection.frame, 0);
        state.advance(&db, 0.8).expect("advance");
        let advanced = state.current().expect("selection");
        assert!(near(advanced.time, 0.8));
        // Nearest stored frame to 0.8 s is the 1.0 s frame; the cost is the last query's.
        assert_eq!(advanced.frame, 2);
        assert!(near(advanced.cost, selection.cost));
    }

    #[test]
    fn non_looping_build_never_samples_the_clip_end() {
        let skel = root_only();
        // The root collapses to zero scale exactly at the final key, in a frame the horizon
        // rule drops; only end sampling (the cycle measurement) touches it.
        let mut clip = AnimationClip::empty("broken_end", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 0.5],
                values: vec![Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ONE, Vec3::ZERO],
                exact_rotation: false,
            },
        });
        let config = config_joints(0.25, vec![], vec![0.6]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("non-looping build");
        assert_eq!(db.frame_count(), 2);
        // Looping still measures the cycle across the end and fails on the degenerate key.
        assert!(
            FeatureDatabase::build(
                &skel,
                &[ClipSource {
                    clip: &clip,
                    looping: true,
                }],
                &config,
            )
            .is_err()
        );
    }

    /// A centimetre rig: the root scales its children by 0.01 and the hand sits 100 cm (1 m)
    /// above it. The clip carries the root scale in its own static scale track (clips authored
    /// for a cm rig do), because a clip track overwrites the bind scale when it is sampled.
    fn cm_rig_and_walk() -> (Skeleton, AnimationClip) {
        let cm = Vec3::splat(0.01);
        let skel = skeleton(vec![
            LocalBind {
                scale: cm,
                ..LocalBind::default()
            },
            translated(Vec3::new(0.0, 100.0, 0.0)),
        ]);
        let mut clip = AnimationClip::empty("cm_walk", 1.0);
        // The root's own translation lives in its parent's space (metres): 1 m forward (-Z)
        // over the clip.
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(cm),
        });
        (skel, clip)
    }

    /// Anim 20 (corr-animation): trajectory offsets were taken through the root's full inverse,
    /// scale included, so a cm-scaled root stored 100 per metre while [`TrajectorySample`]
    /// (what a host passes to `query_from_pose`) is documented in metres.
    #[test]
    fn trajectory_offsets_on_a_cm_scaled_root_are_metres() {
        let (skel, clip) = cm_rig_and_walk();
        // Fixture check: the sampled root really carries the 0.01 scale.
        let mut sampled = skel.bind_pose_pose();
        clip.sample_pose(0.0, &mut sampled);
        let root = skel.globals(&sampled, None)[0];
        let (scale, _, _) = root.to_scale_rotation_translation();
        assert!(near_vec(&scale.to_array(), &[0.01, 0.01, 0.01]), "{scale:?}");
        let config = config_joints(0.5, vec![1], vec![1.0]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        // The 1 s horizon only fits at t = 0 on a 1 s non-looping clip.
        assert_eq!(db.frame_count(), 1);
        let row = db.raw_row(0).expect("row");
        // Layout: hand position (3), hand velocity (3), then offset (3) and facing sin/cos (2).
        assert_eq!(row.len(), 11);
        assert!(
            near_vec(&row[6..9], &[0.0, 0.0, -1.0]),
            "1 m of root travel must read as 1 m, not 100 cm: {row:?}"
        );
        assert!(near_vec(&row[9..11], &[0.0, 1.0]), "{row:?}");
        // The joint channels stay in the root's own (cm) frame on both the build and the query
        // side, so they need no host conversion: the hand is 100 cm up.
        assert!(near_vec(&row[0..3], &[0.0, 100.0, 0.0]), "{row:?}");
    }

    /// The offset frame keeps the root's rotation and drops only its scale: a cm root turned a
    /// quarter turn about +Y faces world -X, so 1 m of travel along -X reads as 1 m ahead (-Z).
    #[test]
    fn a_turned_cm_scaled_root_keeps_its_rotation_in_the_offset_frame() {
        let (skel, mut clip) = cm_rig_and_walk();
        let turn = Quat::from_rotation_y(PI * 0.5);
        clip.tracks[0].rotations = TrackCurve::Static(turn);
        clip.tracks[0].translations = TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Vec3::ZERO, Vec3::new(-1.0, 0.0, 0.0)],
            exact_rotation: false,
        };
        let config = config_joints(0.5, vec![1], vec![1.0]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let row = db.raw_row(0).expect("row");
        assert!(near_vec(&row[6..9], &[0.0, 0.0, -1.0]), "{row:?}");
        assert!(near_vec(&row[9..11], &[0.0, 1.0]), "{row:?}");
    }

    /// The host side of anim 20: the live pose of frame 0 plus the documented metre trajectory
    /// (1 m ahead after 1 s) must rebuild frame 0's own row, so the search finds it at cost 0.
    #[test]
    fn a_metre_trajectory_query_matches_its_own_frame_on_a_cm_scaled_root() {
        let (skel, clip) = cm_rig_and_walk();
        let config = config_joints(0.5, vec![1], vec![1.0]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        let mut live = skel.bind_pose_pose();
        clip.sample_pose(0.0, &mut live);
        let query = db
            .query_from_pose(
                &skel,
                &live,
                None,
                &[TrajectorySample {
                    offset: Vec3::new(0.0, 0.0, -1.0),
                    yaw: 0.0,
                }],
            )
            .expect("query");
        let row = db.raw_row(0).expect("row");
        assert!(
            near_vec(&query[6..11], &row[6..11]),
            "query {query:?} vs row {row:?}"
        );
        assert!(db.cost(0, &query).expect("cost") <= 1.0e-6);
    }

    /// A 2 s non-looping clip whose hand moves 1 m per second, so every stored row differs (a
    /// clip whose rows tie would send every search to frame 0, the lowest index). With a 0.1 s
    /// interval and a 0.9 s horizon the build stores t = 0.0 ..= 1.1: twelve frames.
    fn two_second_reach() -> (Skeleton, AnimationClip, FeatureDatabase) {
        let skel = arm();
        let clip = translation_clip(
            "reach",
            2.0,
            1,
            &[(0.0, Vec3::ZERO), (2.0, Vec3::new(2.0, 0.0, 0.0))],
        );
        let config = config_joints(0.1, vec![1], vec![0.9]);
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config,
        )
        .expect("db");
        (skel, clip, db)
    }

    /// Anim 21 (corr-animation): at a non-looping clip's clamped end the stored frame nearest
    /// in time is the last one (1.1 s), so "continue" kept winning against the same frame and
    /// playback stayed frozen on the end pose. Spec 52 § matcher: continuing is not a
    /// candidate once a non-looping clip has ended.
    #[test]
    fn a_non_looping_clip_at_its_clamped_end_jumps_back_to_the_match() {
        let (_skel, _clip, db) = two_second_reach();
        // Fixture checks against the real `planned_samples` / horizon layout.
        assert_eq!(db.frame_count(), 12);
        let last = db.frame(11).expect("last frame");
        assert!(near(last.time, 1.1), "{last:?}");
        let query = db.raw_row(11).expect("row").to_vec();
        let mut state = SearchState::default();
        let first = state
            .search(&db, &query, SearchMethod::BruteForce)
            .expect("first");
        assert_eq!(first.frame, 11, "the fixture's rows must not tie");
        assert!(near(first.time, 1.1));
        state.advance(&db, 1.0).expect("advance");
        let ended = state.current().expect("current");
        assert!(near(ended.time, 2.0), "clamped end: {ended:?}");
        let again = state.search(&db, &query, SearchMethod::KdTree).expect("again");
        assert!(
            again.blend && near(again.time, 1.1) && again.frame == 11,
            "playback stuck at the clamped end: {again:?}"
        );
    }

    /// The same end rule with a huge replay threshold: a different frame of the ended clip is
    /// still adopted, because there is nothing to continue.
    #[test]
    fn an_ended_clip_ignores_the_replay_threshold() {
        let (_skel, _clip, db) = two_second_reach();
        let mut state = SearchState::new(1.0e9).expect("state");
        let query_end = db.raw_row(11).expect("row").to_vec();
        state
            .search(&db, &query_end, SearchMethod::BruteForce)
            .expect("first");
        state.advance(&db, 5.0).expect("advance past the end");
        let query_mid = db.raw_row(4).expect("row").to_vec();
        let picked = state
            .search(&db, &query_mid, SearchMethod::BruteForce)
            .expect("search");
        assert!(picked.blend, "{picked:?}");
        assert_eq!(picked.frame, 4);
        assert!(near(picked.time, 0.4));
    }

    /// Guard for the end rule: when the clip's own last stored frame sits exactly at the end
    /// (no horizon), re-matching it is not a jump, so the blend does not restart every search.
    #[test]
    fn an_ended_clip_matching_its_end_frame_keeps_playing_without_a_blend() {
        let skel = arm();
        let clip = translation_clip(
            "reach",
            1.0,
            1,
            &[(0.0, Vec3::ZERO), (1.0, Vec3::new(1.0, 0.0, 0.0))],
        );
        let db = FeatureDatabase::build(
            &skel,
            &[ClipSource {
                clip: &clip,
                looping: false,
            }],
            &config_joints(0.5, vec![1], vec![]),
        )
        .expect("db");
        assert_eq!(db.frame_count(), 3);
        let query = db.raw_row(2).expect("row").to_vec();
        let mut state = SearchState::default();
        assert!(
            state
                .search(&db, &query, SearchMethod::BruteForce)
                .expect("first")
                .blend
        );
        let kept = state.search(&db, &query, SearchMethod::BruteForce).expect("kept");
        assert!(!kept.blend, "{kept:?}");
        assert!(near(kept.time, 1.0));
    }
}
