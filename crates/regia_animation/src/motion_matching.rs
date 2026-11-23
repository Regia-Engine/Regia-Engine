// SPDX-License-Identifier: MPL-2.0
//! Motion matching: a pose-feature database built from clips, and a runtime search that keeps
//! picking the clip frame whose features best match the character's current pose and the path
//! the game wants it to take.
//!
//! The method is the one Simon Clavet presented in "Motion Matching and The Road to Next-Gen
//! Animation" (GDC 2016) and that Holden et al. use as the baseline of "Learned Motion
//! Matching" (SIGGRAPH 2020): every frame of every clip becomes one feature vector, the game
//! builds a query of the same shape every few frames, and playback jumps to the nearest frame.
//! Only the non-learned baseline is built here.
//!
//! # Features
//!
//! All features are expressed in the *character frame* of the frame they describe: the root
//! joint projected onto the ground (Y up, the XZ plane), turned to the root's facing (the root
//! joint's `forward` axis projected onto the ground). A frame's vector is, in order:
//!
//! 1. the root joint's velocity (3 floats),
//! 2. the future trajectory positions at each [`FeatureSchema::trajectory_times`] (2 floats
//!    each, character-frame `x`, `z`),
//! 3. the future facing directions at the same times (2 floats each),
//! 4. each foot joint's position relative to the ground point (3 floats each),
//! 5. each foot joint's velocity (3 floats each).
//!
//! Velocities are central differences over one database frame (one-sided at the ends of a
//! clip that does not loop). Future samples of a looping clip continue into the next lap (the
//! clip's lap displacement is applied once per lap); a clip that does not loop clamps them to
//! its last frame.
//!
//! # Normalization and search
//!
//! Each of the five groups is normalized as a whole: every dimension has its mean removed and
//! is divided by the group's standard deviation (the root-mean-square of the dimensions'
//! deviations, so the dimensions of one group keep their relative scale), then multiplied by
//! the group's [`FeatureWeights`]. A query is normalized the same way; the cost of a frame is the
//! squared Euclidean distance between the two normalized vectors.
//!
//! [`MotionDatabase::search`] finds the cheapest frame by a linear scan
//! ([`SearchMethod::BruteForce`]) or an exact k-d tree ([`SearchMethod::KdTree`]); both return
//! the same frame, ties going to the lowest frame index.
//!
//! # Runtime
//!
//! [`MotionMatcher`] advances the playing clip and searches every
//! [`MatcherSettings::search_interval`] seconds with a query made of the playing frame's pose
//! features and the desired trajectory. The playing frame's natural continuation costs its plain
//! feature distance; a jump to any other frame costs its distance plus
//! [`MatcherSettings::transition_cost`], frames within [`MatcherSettings::exclude_window`] of the
//! playing frame in the same clip are not jump targets, and no jump happens within
//! [`MatcherSettings::min_jump_interval`] of the previous one. Together these keep a query that
//! sits between two similar frames from flickering between them. The host plays the returned
//! clip and time (inertializing each jump, [`crate::inertialize`]).

// Source: Simon Clavet, "Motion Matching and The Road to Next-Gen Animation", GDC 2016 talk
// (features: root velocity, future trajectory, foot positions and velocities; transition cost
// and minimum time between jumps). Daniel Holden, Oussama Kanoun, Maksym Perepichka, Tiberiu
// Popa, "Learned Motion Matching", ACM Trans. Graph. 39(4), 2020, § 3 (the non-learned
// baseline: feature layout, per-group normalization with user weights, search interval,
// excluding the frames around the current one). k-d tree: J. L. Bentley, "Multidimensional
// binary search trees used for associative searching", CACM 18(9), 1975, and J. H. Friedman,
// J. L. Bentley, R. A. Finkel, "An algorithm for finding best matches in logarithmic expected
// time", ACM TOMS 3(3), 1977. Original code; nothing ported.

pub mod animator;
pub mod asset;
pub mod contacts;
pub mod debug;
pub mod gait;
mod kd;
pub mod lanes;
pub mod player;
pub mod predict;
pub mod procedural;
pub mod registry;
pub mod scope;
mod tags;
pub mod warp;

use std::ops::Range;
use std::sync::Arc;

use regia_app::{FixedUpdate, Update};
use regia_core::math::{Mat4, Quat, Vec2, Vec3};
use regia_ecs::prelude::*;

use crate::clip::AnimationClip;
use crate::num::{det_atan2, det_rotation_y, det_sin_cos};
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use warp::ArcLengthArrays;

pub use animator::{AnimatorMotionMatch, apply_stage, drive_animators_from_motion_matching};
pub use asset::{
    COOKED_MOTIONDB_VERSION, EntrySource, MOTIONDB_FORMAT_VERSION, MotionDatabaseSource, NormalizationSource,
    PoseJointSource, SampleWeightsSource, SchemaSource, SpanKindSource, SpanSource, WeightsSource,
    cooked_motiondb_migration_sources, cooked_motiondb_schema_fingerprint, migrate_cooked_motiondb,
    motiondb_migration_sources, motiondb_schema_fingerprint,
};
pub use contacts::{
    ContactSettings, FootLock, FootLockSettings, FootLocker, MotionMatchFootLocks, player_feet,
    run_foot_locks,
};
pub use debug::{CandidateReport, CandidateRow, CandidateStatus, CostParts};
pub use kd::{
    KdStats, KdTree, LEAF_SIZE, LEAF_SIZE_RANGE, Nearest, SearchWork, nearest_brute_force,
    nearest_brute_force_biased, nearest_brute_force_biased_counted, squared_distance,
};
pub use player::{
    MotionMatchPlayer, PlayerWarps, RootMotionDelta, WarpInputs, run_motion_match_players, step_player,
};
pub use predict::{
    FacingMode, MAX_TURN_RATE, PredictorSettings, PredictorState, TrajectoryPredictor, predict_trajectory,
    run_trajectory_predictors,
};
pub use registry::{
    LoadedMotionDatabase, MOTION_DATABASE_TYPE_PATH, MotionDatabaseAsset, MotionDatabaseAssetError,
    MotionDatabaseRef, MotionDatabaseRefState, MotionDatabases, load_motion_database,
    load_motion_database_file, load_motion_database_id, resolve_motion_database_refs,
};
pub use scope::{MontageScopeRule, MotionMatchMontageScope, montage_scope_filter, run_montage_scopes};
pub use tags::{
    ClipAnnotation, FLAG_MIRRORED, FLAG_PASS_THROUGH, FLAG_UNINDEXED, FrameMeta, MAX_BIAS, MAX_TAGS,
    SearchFilter, Span, SpanKind, TagError, TagTable, validate_spans,
};

/// The default future trajectory sample times, seconds: a third, two thirds and one second
/// ahead (20, 40 and 60 frames at 60 Hz, the spacing of both sources).
pub const DEFAULT_TRAJECTORY_TIMES: [f32; 3] = [1.0 / 3.0, 2.0 / 3.0, 1.0];

/// The default database sample rate, frames per second.
pub const DEFAULT_SAMPLE_RATE: f32 = 30.0;

/// A standard deviation below this counts as a constant feature group (divided by 1 instead).
const MIN_DEVIATION: f32 = 1e-6;

/// A clip duration or time below this is treated as zero.
const TIME_EPSILON: f32 = 1e-5;
/// The matcher clock is rebased once it reaches this many seconds (`MotionMatcher::rebase_clock`).
const CLOCK_REBASE_AT: f32 = 4096.0;
/// The matcher clock moves down by a multiple of this many seconds when it is rebased.
const CLOCK_REBASE_STEP: f32 = 2048.0;

/// Most future trajectory samples a schema may have.
pub const MAX_TRAJECTORY_SAMPLES: usize = 16;
/// Most past (history) samples a schema may have.
pub const MAX_HISTORY_SAMPLES: usize = 8;
/// Most joints a schema may name across `feet` and `pose_joints`.
pub const MAX_SCHEMA_JOINTS: usize = 64;
/// Most floats a feature vector may have (before padding to a multiple of eight).
pub const MAX_DIMENSIONS: usize = 192;
/// Largest sample or joint weight.
pub const MAX_SAMPLE_WEIGHT: f32 = 100.0;
/// Largest group weight.
pub const MAX_GROUP_WEIGHT: f32 = 1000.0;
/// `Auto` searches with the tree from this many frames on.
pub const AUTO_TREE_MIN_FRAMES: usize = 2048;

/// The feature groups, in vector order. Each group is normalized and weighted as a whole. The
/// first five are the baseline layout; the history and pose groups are present only when the
/// schema asks for history samples or extra pose joints (`docs/specs/52-motion-matching.md`
/// § 2.2), the contact and phase groups only when it asks for contact labels
/// ([`FeatureSchema::contacts`], § 2.2a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FeatureGroup {
    /// The root joint's velocity (3 floats).
    RootVelocity,
    /// Future trajectory positions (2 floats per sample time).
    TrajectoryPosition,
    /// Future facing directions (2 floats per sample time).
    TrajectoryDirection,
    /// Foot positions relative to the ground point (3 floats per foot).
    FootPosition,
    /// Foot velocities (3 floats per foot).
    FootVelocity,
    /// Past root positions (2 floats per history time).
    TrajectoryHistory,
    /// Extra pose joint positions relative to the ground point (3 floats per joint with
    /// `position`).
    PosePosition,
    /// Extra pose joint velocities (3 floats per joint with `velocity`).
    PoseVelocity,
    /// Foot contact labels (1 float per foot: 1 planted, 0 not; [`contacts`]).
    FootContact,
    /// Foot gait phase (2 floats per foot: `cos`, `sin` of the phase angle; [`contacts`]).
    FootPhase,
}

impl FeatureGroup {
    /// Number of groups.
    pub const COUNT: usize = 10;

    /// Every group, in vector order.
    pub const ALL: [FeatureGroup; FeatureGroup::COUNT] = [
        FeatureGroup::RootVelocity,
        FeatureGroup::TrajectoryPosition,
        FeatureGroup::TrajectoryDirection,
        FeatureGroup::FootPosition,
        FeatureGroup::FootVelocity,
        FeatureGroup::TrajectoryHistory,
        FeatureGroup::PosePosition,
        FeatureGroup::PoseVelocity,
        FeatureGroup::FootContact,
        FeatureGroup::FootPhase,
    ];

    /// The five baseline groups (always present, possibly empty).
    pub const BASELINE: [FeatureGroup; 5] = [
        FeatureGroup::RootVelocity,
        FeatureGroup::TrajectoryPosition,
        FeatureGroup::TrajectoryDirection,
        FeatureGroup::FootPosition,
        FeatureGroup::FootVelocity,
    ];

    /// The group's index in [`FeatureGroup::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }

    /// The group with snake_case name `name`.
    pub fn from_name(name: &str) -> Option<FeatureGroup> {
        FeatureGroup::ALL.into_iter().find(|g| g.name() == name)
    }

    /// The group's snake_case name (also its agent-protocol key).
    pub fn name(self) -> &'static str {
        match self {
            FeatureGroup::RootVelocity => "root_velocity",
            FeatureGroup::TrajectoryPosition => "trajectory_position",
            FeatureGroup::TrajectoryDirection => "trajectory_direction",
            FeatureGroup::FootPosition => "foot_position",
            FeatureGroup::FootVelocity => "foot_velocity",
            FeatureGroup::TrajectoryHistory => "trajectory_history",
            FeatureGroup::PosePosition => "pose_position",
            FeatureGroup::PoseVelocity => "pose_velocity",
            FeatureGroup::FootContact => "foot_contact",
            FeatureGroup::FootPhase => "foot_phase",
        }
    }
}

/// Per-group feature weights: how much each group counts in the cost. Weights multiply the
/// normalized features, so a weight of 2 makes a group's differences cost four times as much;
/// 0 removes the group from the search. Every weight defaults to 1 except the contact and phase
/// groups, which default to 0 (an unset channel never changes a search).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FeatureWeights {
    /// Weight of [`FeatureGroup::RootVelocity`].
    pub root_velocity: f32,
    /// Weight of [`FeatureGroup::TrajectoryPosition`].
    pub trajectory_position: f32,
    /// Weight of [`FeatureGroup::TrajectoryDirection`].
    pub trajectory_direction: f32,
    /// Weight of [`FeatureGroup::FootPosition`].
    pub foot_position: f32,
    /// Weight of [`FeatureGroup::FootVelocity`].
    pub foot_velocity: f32,
    /// Weight of [`FeatureGroup::TrajectoryHistory`].
    pub trajectory_history: f32,
    /// Weight of [`FeatureGroup::PosePosition`].
    pub pose_position: f32,
    /// Weight of [`FeatureGroup::PoseVelocity`].
    pub pose_velocity: f32,
    /// Weight of [`FeatureGroup::FootContact`] (default 0).
    pub foot_contact: f32,
    /// Weight of [`FeatureGroup::FootPhase`] (default 0).
    pub foot_phase: f32,
}

impl Default for FeatureWeights {
    fn default() -> Self {
        Self {
            root_velocity: 1.0,
            trajectory_position: 1.0,
            trajectory_direction: 1.0,
            foot_position: 1.0,
            foot_velocity: 1.0,
            trajectory_history: 1.0,
            pose_position: 1.0,
            pose_velocity: 1.0,
            foot_contact: 0.0,
            foot_phase: 0.0,
        }
    }
}

impl FeatureWeights {
    /// The weight of one group.
    pub fn get(&self, group: FeatureGroup) -> f32 {
        match group {
            FeatureGroup::RootVelocity => self.root_velocity,
            FeatureGroup::TrajectoryPosition => self.trajectory_position,
            FeatureGroup::TrajectoryDirection => self.trajectory_direction,
            FeatureGroup::FootPosition => self.foot_position,
            FeatureGroup::FootVelocity => self.foot_velocity,
            FeatureGroup::TrajectoryHistory => self.trajectory_history,
            FeatureGroup::PosePosition => self.pose_position,
            FeatureGroup::PoseVelocity => self.pose_velocity,
            FeatureGroup::FootContact => self.foot_contact,
            FeatureGroup::FootPhase => self.foot_phase,
        }
    }

    /// A mutable reference to the weight of one group.
    pub fn get_mut(&mut self, group: FeatureGroup) -> &mut f32 {
        match group {
            FeatureGroup::RootVelocity => &mut self.root_velocity,
            FeatureGroup::TrajectoryPosition => &mut self.trajectory_position,
            FeatureGroup::TrajectoryDirection => &mut self.trajectory_direction,
            FeatureGroup::FootPosition => &mut self.foot_position,
            FeatureGroup::FootVelocity => &mut self.foot_velocity,
            FeatureGroup::TrajectoryHistory => &mut self.trajectory_history,
            FeatureGroup::PosePosition => &mut self.pose_position,
            FeatureGroup::PoseVelocity => &mut self.pose_velocity,
            FeatureGroup::FootContact => &mut self.foot_contact,
            FeatureGroup::FootPhase => &mut self.foot_phase,
        }
    }
}

/// How feature dimensions are scaled before the distance (`docs/specs/52-motion-matching.md`
/// § 2.3). Statistics always come from every frame of the database, in frame order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NormalizationMode {
    /// Each group divided by its pooled deviation (the baseline).
    #[default]
    GroupDeviation,
    /// Each dimension divided by its own deviation (for groups whose dimensions are unrelated).
    PerDimension,
    /// Mean-centered only: the scale is the weight (tests and hand-tuned data).
    None,
}

impl NormalizationMode {
    /// The snake_case name (`group_deviation`, `per_dimension`, `none`).
    pub fn name(self) -> &'static str {
        match self {
            NormalizationMode::GroupDeviation => "group_deviation",
            NormalizationMode::PerDimension => "per_dimension",
            NormalizationMode::None => "none",
        }
    }
}

/// An extra pose joint (hands, hips, head) whose position and/or velocity are features.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoseJoint {
    /// The joint.
    pub joint: u32,
    /// Its position relative to the ground point is a feature ([`FeatureGroup::PosePosition`]).
    pub position: bool,
    /// Its character-frame velocity is a feature ([`FeatureGroup::PoseVelocity`]).
    pub velocity: bool,
    /// Multiplier on this joint's dimensions, `[0, 100]`.
    pub weight: f32,
}

impl PoseJoint {
    /// A joint with both position and velocity at weight 1.
    pub fn new(joint: u32) -> Self {
        Self {
            joint,
            position: true,
            velocity: true,
            weight: 1.0,
        }
    }
}

/// Per-sample multipliers inside the trajectory and history groups. An empty list means every
/// sample weighs 1; otherwise it has one entry per sample time, each in `[0, 100]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SampleWeights {
    /// One per [`FeatureSchema::trajectory_times`] entry (both the position and the direction
    /// group).
    pub trajectory: Vec<f32>,
    /// One per [`FeatureSchema::history_times`] entry.
    pub history: Vec<f32>,
}

/// What a database extracts from each clip frame, and how the groups are weighted.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureSchema {
    /// The joint whose ground projection and facing define the character frame (usually the
    /// root-motion joint, or the hips).
    pub root_joint: u32,
    /// The root joint's local axis that points where the character faces (default `+Z`).
    pub forward: Vec3,
    /// Joints whose positions and velocities are features (usually the two feet). May be empty.
    pub feet: Vec<u32>,
    /// Future trajectory sample times, seconds, each finite and above zero.
    pub trajectory_times: Vec<f32>,
    /// Database frames per second of clip time.
    pub sample_rate: f32,
    /// Per-group weights.
    pub weights: FeatureWeights,
    /// Past trajectory sample times, seconds before now, each finite and above zero (at most
    /// eight). Empty = no [`FeatureGroup::TrajectoryHistory`] group (the baseline layout).
    pub history_times: Vec<f32>,
    /// Extra pose joints beyond the feet. Empty = the baseline layout.
    pub pose_joints: Vec<PoseJoint>,
    /// Per-sample weights inside the trajectory and history groups.
    pub sample_weights: SampleWeights,
    /// How dimensions are scaled.
    pub normalization: NormalizationMode,
    /// Derive per-foot contact labels (and, with [`ContactSettings::phase`], the gait phase) at
    /// build time: groups [`FeatureGroup::FootContact`] and [`FeatureGroup::FootPhase`] after
    /// every other group. `None` = neither group (the baseline layout).
    pub contacts: Option<ContactSettings>,
}

impl FeatureSchema {
    /// A schema with the default forward axis (`+Z`), trajectory times
    /// ([`DEFAULT_TRAJECTORY_TIMES`]), sample rate ([`DEFAULT_SAMPLE_RATE`]) and unit weights.
    pub fn new(root_joint: u32, feet: Vec<u32>) -> Self {
        Self {
            root_joint,
            forward: Vec3::Z,
            feet,
            trajectory_times: DEFAULT_TRAJECTORY_TIMES.to_vec(),
            sample_rate: DEFAULT_SAMPLE_RATE,
            weights: FeatureWeights::default(),
            history_times: Vec::new(),
            pose_joints: Vec::new(),
            sample_weights: SampleWeights::default(),
            normalization: NormalizationMode::GroupDeviation,
            contacts: None,
        }
    }

    /// Floats per feature vector.
    pub fn dimensions(&self) -> usize {
        3 + 4 * self.trajectory_times.len()
            + 6 * self.feet.len()
            + 2 * self.history_times.len()
            + 3 * self.pose_position_count()
            + 3 * self.pose_velocity_count()
            + self.contact_count()
            + 2 * self.phase_count()
    }

    /// Feet with a contact label (all feet with [`FeatureSchema::contacts`], none without).
    pub fn contact_count(&self) -> usize {
        if self.contacts.is_some() {
            self.feet.len()
        } else {
            0
        }
    }

    /// Feet with a phase feature (all feet when the contact settings ask for the phase).
    pub fn phase_count(&self) -> usize {
        if self.contacts.is_some_and(|c| c.phase) {
            self.feet.len()
        } else {
            0
        }
    }

    /// Floats per vector padded to the canonical lane width ([`lanes::padded_dims`]).
    pub fn padded_dimensions(&self) -> usize {
        lanes::padded_dims(self.dimensions())
    }

    /// Pose joints with a position feature.
    pub fn pose_position_count(&self) -> usize {
        self.pose_joints.iter().filter(|j| j.position).count()
    }

    /// Pose joints with a velocity feature.
    pub fn pose_velocity_count(&self) -> usize {
        self.pose_joints.iter().filter(|j| j.velocity).count()
    }

    /// The weight multiplier of every dimension (group weight times sample or joint weight),
    /// before dividing by the deviation.
    pub fn dimension_weights(&self) -> Vec<f32> {
        let mut out = vec![0.0f32; self.dimensions()];
        let sample = |list: &[f32], i: usize| list.get(i).copied().unwrap_or(1.0);
        for group in FeatureGroup::ALL {
            let weight = self.weights.get(group);
            let range = self.group_range(group);
            for (offset, w) in out[range].iter_mut().enumerate() {
                let extra = match group {
                    FeatureGroup::TrajectoryPosition | FeatureGroup::TrajectoryDirection => {
                        sample(&self.sample_weights.trajectory, offset / 2)
                    }
                    FeatureGroup::TrajectoryHistory => sample(&self.sample_weights.history, offset / 2),
                    FeatureGroup::PosePosition => self
                        .pose_joints
                        .iter()
                        .filter(|j| j.position)
                        .nth(offset / 3)
                        .map_or(1.0, |j| j.weight),
                    FeatureGroup::PoseVelocity => self
                        .pose_joints
                        .iter()
                        .filter(|j| j.velocity)
                        .nth(offset / 3)
                        .map_or(1.0, |j| j.weight),
                    _ => 1.0,
                };
                *w = weight * extra;
            }
        }
        out
    }

    /// The slice of a feature vector one group occupies.
    pub fn group_range(&self, group: FeatureGroup) -> Range<usize> {
        let points = self.trajectory_times.len();
        let feet = self.feet.len();
        let trajectory = 3;
        let directions = trajectory + 2 * points;
        let foot_positions = directions + 2 * points;
        let foot_velocities = foot_positions + 3 * feet;
        let history = foot_velocities + 3 * feet;
        let pose_positions = history + 2 * self.history_times.len();
        let pose_velocities = pose_positions + 3 * self.pose_position_count();
        let contacts = pose_velocities + 3 * self.pose_velocity_count();
        let phases = contacts + self.contact_count();
        match group {
            FeatureGroup::RootVelocity => 0..3,
            FeatureGroup::TrajectoryPosition => trajectory..directions,
            FeatureGroup::TrajectoryDirection => directions..foot_positions,
            FeatureGroup::FootPosition => foot_positions..foot_velocities,
            FeatureGroup::FootVelocity => foot_velocities..history,
            FeatureGroup::TrajectoryHistory => history..pose_positions,
            FeatureGroup::PosePosition => pose_positions..pose_velocities,
            FeatureGroup::PoseVelocity => pose_velocities..contacts,
            FeatureGroup::FootContact => contacts..phases,
            FeatureGroup::FootPhase => phases..phases + 2 * self.phase_count(),
        }
    }

    /// Check the schema against a skeleton.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::BadSampleRate`], [`MotionMatchError::BadTrajectoryTime`],
    /// [`MotionMatchError::BadHistoryTime`], [`MotionMatchError::BadWeight`],
    /// [`MotionMatchError::SampleWeightCount`], [`MotionMatchError::BadForward`],
    /// [`MotionMatchError::JointOutOfRange`], [`MotionMatchError::DuplicateJoint`],
    /// [`MotionMatchError::BadContacts`] or [`MotionMatchError::TooManyDimensions`].
    pub fn validate(&self, skeleton: &Skeleton) -> Result<(), MotionMatchError> {
        if !self.sample_rate.is_finite() || self.sample_rate <= 0.0 {
            return Err(MotionMatchError::BadSampleRate(self.sample_rate));
        }
        if let Some(&time) = self
            .trajectory_times
            .iter()
            .find(|t| !t.is_finite() || **t <= 0.0)
        {
            return Err(MotionMatchError::BadTrajectoryTime(time));
        }
        if let Some(&time) = self.history_times.iter().find(|t| !t.is_finite() || **t <= 0.0) {
            return Err(MotionMatchError::BadHistoryTime(time));
        }
        for group in FeatureGroup::ALL {
            let value = self.weights.get(group);
            if !value.is_finite() || !(0.0..=MAX_GROUP_WEIGHT).contains(&value) {
                return Err(MotionMatchError::BadWeight {
                    group: group.name(),
                    value,
                });
            }
        }
        for (group, list, expected) in [
            (
                FeatureGroup::TrajectoryPosition,
                &self.sample_weights.trajectory,
                self.trajectory_times.len(),
            ),
            (
                FeatureGroup::TrajectoryHistory,
                &self.sample_weights.history,
                self.history_times.len(),
            ),
        ] {
            if !list.is_empty() && list.len() != expected {
                return Err(MotionMatchError::SampleWeightCount {
                    group: group.name(),
                    got: list.len(),
                    expected,
                });
            }
            if let Some(&value) = list
                .iter()
                .find(|w| !w.is_finite() || !(0.0..=MAX_SAMPLE_WEIGHT).contains(*w))
            {
                return Err(MotionMatchError::BadWeight {
                    group: group.name(),
                    value,
                });
            }
        }
        if let Some(joint) = self
            .pose_joints
            .iter()
            .find(|j| !j.weight.is_finite() || !(0.0..=MAX_SAMPLE_WEIGHT).contains(&j.weight))
        {
            return Err(MotionMatchError::BadWeight {
                group: FeatureGroup::PosePosition.name(),
                value: joint.weight,
            });
        }
        if let Some(contacts) = &self.contacts {
            contacts.validate()?;
        }
        let flat = Vec2::new(self.forward.x, self.forward.z);
        if !self.forward.is_finite() || flat.length_squared() < 1e-12 {
            return Err(MotionMatchError::BadForward(self.forward));
        }
        let joints = skeleton.len();
        let named: Vec<u32> = self
            .feet
            .iter()
            .copied()
            .chain(self.pose_joints.iter().map(|j| j.joint))
            .collect();
        for &joint in std::iter::once(&self.root_joint).chain(&named) {
            if joint as usize >= joints {
                return Err(MotionMatchError::JointOutOfRange { joint, joints });
            }
        }
        for (i, joint) in named.iter().enumerate() {
            if named[..i].contains(joint) {
                return Err(MotionMatchError::DuplicateJoint(*joint));
            }
        }
        for (what, got, max) in [
            (
                "trajectory samples",
                self.trajectory_times.len(),
                MAX_TRAJECTORY_SAMPLES,
            ),
            ("history samples", self.history_times.len(), MAX_HISTORY_SAMPLES),
            ("joints", named.len(), MAX_SCHEMA_JOINTS),
            ("dimensions", self.dimensions(), MAX_DIMENSIONS),
        ] {
            if got > max {
                return Err(MotionMatchError::TooManyDimensions { what, got, max });
            }
        }
        Ok(())
    }
}

/// Errors from building or querying a motion-matching database.
#[derive(Clone, Debug, thiserror::Error, PartialEq)]
pub enum MotionMatchError {
    /// The database was given no clips.
    #[error("a motion-matching database needs at least one clip")]
    NoClips,
    /// The sample rate is not a positive finite number.
    #[error("sample rate {0} must be finite and above zero")]
    BadSampleRate(f32),
    /// A trajectory time is not a positive finite number.
    #[error("trajectory time {0} must be finite and above zero")]
    BadTrajectoryTime(f32),
    /// A group weight is negative or not finite.
    #[error("weight {value} for {group} must be finite and not negative")]
    BadWeight {
        /// The group's name ([`FeatureGroup::name`]).
        group: &'static str,
        /// The rejected weight.
        value: f32,
    },
    /// The forward axis has no ground-plane component (or is not finite).
    #[error("forward axis {0} must be finite and not vertical")]
    BadForward(Vec3),
    /// A schema joint is not in the skeleton.
    #[error("joint {joint} is not in the skeleton ({joints} joints)")]
    JointOutOfRange {
        /// The joint id.
        joint: u32,
        /// The skeleton's joint count.
        joints: usize,
    },
    /// A clip's duration is not a positive finite number.
    #[error("clip {clip:?} has duration {duration}; it must be finite and above zero")]
    BadClipDuration {
        /// The clip's name.
        clip: String,
        /// The rejected duration.
        duration: f32,
    },
    /// A clip produced a non-finite feature (a non-finite key).
    #[error("clip {clip:?} produced a non-finite feature at {time} s")]
    NonFiniteFeature {
        /// The clip's name.
        clip: String,
        /// The clip time of the frame.
        time: f32,
    },
    /// The database would have more frames than it can index.
    #[error("the database would have {0} frames, more than it can index")]
    TooManyFrames(usize),
    /// A query part has the wrong length for the schema.
    #[error("the query has {got} {part}, the schema has {expected}")]
    QueryShape {
        /// Which part: `"trajectory points"`, `"feet"`, `"history points"`, `"pose joints"`,
        /// `"contacts"`, `"phases"` or `"features"`.
        part: &'static str,
        /// The query's length.
        got: usize,
        /// The schema's length.
        expected: usize,
    },
    /// A query value is not finite.
    #[error("the query has a non-finite value")]
    NonFiniteQuery,
    /// A frame index is past the end of the database.
    #[error("frame {frame} is not in the database ({frames} frames)")]
    FrameOutOfRange {
        /// The requested frame.
        frame: usize,
        /// The database's frame count.
        frames: usize,
    },
    /// A history time is not a positive finite number.
    #[error("history time {0} must be finite and above zero")]
    BadHistoryTime(f32),
    /// The schema exceeds a size limit.
    #[error("the schema has {got} {what}, at most {max} are allowed")]
    TooManyDimensions {
        /// What is counted: `"trajectory samples"`, `"history samples"`, `"joints"` or
        /// `"dimensions"`.
        what: &'static str,
        /// The schema's count.
        got: usize,
        /// The limit.
        max: usize,
    },
    /// A joint is named twice across `feet` and `pose_joints`.
    #[error("joint {0} is named twice across feet and pose joints")]
    DuplicateJoint(u32),
    /// A mirror table is invalid for the skeleton.
    #[error("bad mirror table: {0}")]
    BadMirrorTable(String),
    /// A source names a joint the skeleton lacks.
    #[error("no joint named {0:?} in the skeleton")]
    UnknownJoint(String),
    /// A source entry's clip was not found.
    #[error("entry {entry}: no clip {clip:?}")]
    UnknownClip {
        /// Entry index.
        entry: usize,
        /// The clip path.
        clip: String,
    },
    /// A mirrored entry, but no mirror table was given.
    #[error("entry {entry} is mirrored but there is no mirror table")]
    NoMirrorTable {
        /// Entry index.
        entry: usize,
    },
    /// A source entry or span names a tag the table lacks.
    #[error("entry {entry}: no tag named {tag:?}")]
    UnknownTag {
        /// Entry index.
        entry: usize,
        /// The tag name.
        tag: String,
    },
    /// A span, trim or tag table is invalid.
    #[error("entry {entry}: {reason}")]
    BadSpan {
        /// Entry index.
        entry: usize,
        /// What is wrong.
        reason: String,
    },
    /// A source or cooked file of a newer version.
    #[error("format version {found} is newer than the supported {supported}")]
    UnsupportedFormatVersion {
        /// The file's version.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// A cooked database failed its checks (hash, shape or the recomputed sample rows).
    #[error("corrupt motion database: {0}")]
    CorruptDatabase(String),
    /// A `.motiondb.ron` source does not parse.
    #[error("bad motion database source: {0}")]
    BadSource(String),
    /// A sample-weight list does not have one entry per sample time.
    #[error("{group} has {got} sample weights, the schema has {expected} sample times")]
    SampleWeightCount {
        /// The group's name.
        group: &'static str,
        /// Weights given.
        got: usize,
        /// Sample times in the schema.
        expected: usize,
    },
    /// The contact-label thresholds are invalid ([`ContactSettings::validate`]).
    #[error("bad contact settings: {0}")]
    BadContacts(String),
}

/// One clip given to [`MotionDatabase::build`].
#[derive(Clone, Copy, Debug)]
pub struct MotionClip<'a> {
    /// The clip. Its joint tracks are sampled on top of the skeleton's bind pose; root motion
    /// must be in the root joint's tracks (the character moves through object space).
    pub clip: &'a AnimationClip,
    /// Whether the clip loops: future samples continue into the next lap instead of clamping,
    /// and playback wraps.
    pub looping: bool,
}

/// One clip given to [`MotionDatabase::build_entries`]: [`MotionClip`] plus a mirror flag.
#[derive(Clone, Copy, Debug)]
pub struct MotionEntry<'a> {
    /// The clip (as in [`MotionClip::clip`]).
    pub clip: &'a AnimationClip,
    /// The clip loops.
    pub looping: bool,
    /// Also add the clip mirrored (left and right joints swapped, the lateral axis negated),
    /// as a second range right after the first. Needs a [`MirrorTable`].
    pub mirrored: bool,
}

/// Left/right joint pairs for mirrored entries. The lateral axis is object-space X (the
/// character's right); the skeleton must be mirror-symmetric: each joint's partner has the
/// partner of its parent as parent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MirrorTable {
    /// Joint pairs; joints in no pair are their own partner.
    pub pairs: Vec<(u32, u32)>,
}

impl MirrorTable {
    /// The partner of every joint of `skeleton`.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::BadMirrorTable`] for a joint out of range, a joint in two pairs, or
    /// a hierarchy that is not mirror-symmetric.
    pub fn partners(&self, skeleton: &Skeleton) -> Result<Vec<u32>, MotionMatchError> {
        let joints = skeleton.len();
        let mut partner: Vec<u32> = (0..joints as u32).collect();
        let mut seen = vec![false; joints];
        for &(a, b) in &self.pairs {
            for j in [a, b] {
                if j as usize >= joints {
                    return Err(MotionMatchError::BadMirrorTable(format!(
                        "joint {j} is not in the skeleton ({joints} joints)"
                    )));
                }
                if seen[j as usize] {
                    return Err(MotionMatchError::BadMirrorTable(format!(
                        "joint {j} is in two pairs"
                    )));
                }
                seen[j as usize] = true;
            }
            if a == b {
                return Err(MotionMatchError::BadMirrorTable(format!(
                    "joint {a} is paired with itself"
                )));
            }
            partner[a as usize] = b;
            partner[b as usize] = a;
        }
        for (j, &p) in partner.iter().enumerate() {
            let parent = skeleton.parents.get(j).copied().flatten();
            let partner_parent = skeleton.parents.get(p as usize).copied().flatten();
            if partner_parent != parent.map(|q| partner[q as usize]) {
                return Err(MotionMatchError::BadMirrorTable(format!(
                    "joint {j} and its partner {p} do not have mirrored parents"
                )));
            }
        }
        Ok(partner)
    }
}

/// Reflect a local transform across the YZ plane (negate X): translation `(-x, y, z)`,
/// rotation `(x, -y, -z, w)`.
pub(crate) fn mirror_translation(v: Vec3) -> Vec3 {
    Vec3::new(-v.x, v.y, v.z)
}

/// See [`mirror_translation`].
pub(crate) fn mirror_rotation(q: Quat) -> Quat {
    Quat::from_xyzw(q.x, -q.y, -q.z, q.w)
}

/// Mirror a local pose: joint `j` takes its partner's reflected transform.
pub(crate) fn mirror_pose(pose: &mut Pose, partners: &[u32]) {
    let source = pose.clone();
    for (j, &p) in partners.iter().enumerate() {
        let p = p as usize;
        if j >= pose.len() || p >= source.len() {
            continue;
        }
        pose.translations[j] = mirror_translation(source.translations[p]);
        pose.rotations[j] = mirror_rotation(source.rotations[p]);
        pose.scales[j] = source.scales[p];
    }
}

/// One clip's place in a database.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipRange {
    /// The clip's name.
    pub name: String,
    /// The clip's duration, seconds.
    pub duration: f32,
    /// Whether the clip loops.
    pub looping: bool,
    /// Index of the clip's first database frame.
    pub start: usize,
    /// Number of database frames from the clip.
    pub frames: usize,
    /// The clip is played by distance (stops, pivots, landings; the player's distance
    /// matching). Set from a `.motiondb.ron` entry; false for [`MotionDatabase::build`].
    pub distance_matched: bool,
    /// Index of the entry (the clip given to the build) this range samples; a mirrored entry's
    /// two ranges share it.
    pub source: usize,
    /// The range is the mirrored copy of its entry (left and right swapped, lateral axis
    /// negated); the player mirrors the pose it samples.
    pub mirrored: bool,
}

impl ClipRange {
    /// The duration as a finite, non-negative number (a cooked range may hold NaN or a negative
    /// value; those read as zero so no `clamp` or `rem_euclid` gets a bad bound).
    pub(crate) fn safe_duration(&self) -> f32 {
        if self.duration.is_finite() {
            self.duration.max(0.0)
        } else {
            0.0
        }
    }

    /// The database frames of this clip.
    pub fn range(&self) -> Range<usize> {
        self.start..self.start + self.frames
    }
}

/// Where one database frame comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameRef {
    /// Index into [`MotionDatabase::clips`].
    pub clip: usize,
    /// Clip time of the frame, seconds.
    pub time: f32,
}

/// A future trajectory sample in the character frame: `x` is the character's right-left axis
/// (the root's local `X` for the default forward axis), `y` is forward (`Z`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Reflect)]
#[reflect(Default, PartialEq)]
pub struct TrajectoryPoint {
    /// Ground position relative to the character now, meters (`x`, forward).
    pub position: Vec2,
    /// Facing direction, a unit vector (`x`, forward).
    pub direction: Vec2,
}

impl TrajectoryPoint {
    /// A point `distance` meters straight ahead, facing forward.
    pub fn ahead(distance: f32) -> Self {
        Self {
            position: Vec2::new(0.0, distance),
            direction: Vec2::Y,
        }
    }
}

/// One foot's features, in the character frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FootFeature {
    /// Position relative to the character's ground point, meters.
    pub position: Vec3,
    /// Velocity, meters per second.
    pub velocity: Vec3,
}

/// A search query (or one frame's decoded features), in the character frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MotionQuery {
    /// The root joint's velocity, meters per second (`z` forward).
    pub root_velocity: Vec3,
    /// One point per [`FeatureSchema::trajectory_times`] entry.
    pub trajectory: Vec<TrajectoryPoint>,
    /// One entry per [`FeatureSchema::feet`] joint.
    pub feet: Vec<FootFeature>,
    /// One past ground position per [`FeatureSchema::history_times`] entry, character frame
    /// (`x`, forward), meters.
    pub history: Vec<Vec2>,
    /// One entry per [`FeatureSchema::pose_joints`] joint (the parts the joint does not use
    /// are ignored).
    pub pose_joints: Vec<FootFeature>,
    /// One contact label per foot (1 planted, 0 not) when the schema has contacts, else empty.
    pub contacts: Vec<f32>,
    /// One phase feature per foot (`cos`, `sin` of the phase angle; `(0, 0)` undefined) when the
    /// schema has the phase group, else empty.
    pub phases: Vec<Vec2>,
}

/// The frame a search picked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchHit {
    /// Database frame index.
    pub frame: usize,
    /// Its cost: the squared distance between the normalized features and the normalized query.
    pub cost: f32,
}

/// How a database is searched. Every method is exact: for the same database, query, filter
/// and exclusion they return the same frame and a bit-identical cost
/// (`docs/specs/52-motion-matching.md` § 2.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect, serde::Serialize, serde::Deserialize)]
#[reflect(Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMethod {
    /// A linear scan over every frame (the reference), with a partial-distance early out.
    BruteForce,
    /// The exact k-d tree; returns the same frame as the scan.
    KdTree,
    /// The scan when fewer than [`AUTO_TREE_MIN_FRAMES`] frames exist or under an eighth of
    /// them are eligible (estimated from per-tag counts), the tree otherwise. A pure speed choice.
    #[default]
    Auto,
}

impl SearchMethod {
    /// The snake_case name (`brute_force`, `kd_tree`, `auto`).
    pub fn name(self) -> &'static str {
        match self {
            SearchMethod::BruteForce => "brute_force",
            SearchMethod::KdTree => "kd_tree",
            SearchMethod::Auto => "auto",
        }
    }

    /// The method named `name` ([`SearchMethod::name`]).
    pub fn from_name(name: &str) -> Option<SearchMethod> {
        [SearchMethod::BruteForce, SearchMethod::KdTree, SearchMethod::Auto]
            .into_iter()
            .find(|m| m.name() == name)
    }
}

/// What one search did: its answer, the method it actually ran and the work counters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SearchOutcome {
    /// The cheapest eligible frame by biased cost.
    pub hit: Option<SearchHit>,
    /// The method that ran ([`SearchMethod::Auto`] resolved).
    pub method_used: SearchMethod,
    /// Tree nodes visited and rows scanned.
    pub work: SearchWork,
}

/// Database frames a search skips: up to two half-open index spans (two when a window wraps
/// around the end of a looping clip).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Exclusion {
    spans: [(usize, usize); 2],
}

impl Exclusion {
    /// Skip nothing.
    pub const NONE: Exclusion = Exclusion { spans: [(0, 0); 2] };

    /// Skip the frames in `range`.
    pub fn range(range: Range<usize>) -> Self {
        Self {
            spans: [(range.start, range.end), (0, 0)],
        }
    }

    /// True when `frame` is skipped.
    pub fn contains(&self, frame: usize) -> bool {
        self.spans
            .iter()
            .any(|&(start, end)| frame >= start && frame < end)
    }

    /// Number of skipped frames (spans do not overlap).
    pub fn len(&self) -> usize {
        self.spans.iter().map(|&(s, e)| e.saturating_sub(s)).sum()
    }

    /// True when nothing is skipped.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A motion-matching feature database: one normalized feature vector per clip frame, plus the
/// k-d tree over them. Build it once (at load or cook time) and share it between characters.
#[derive(Clone, Debug)]
pub struct MotionDatabase {
    schema: FeatureSchema,
    clips: Vec<ClipRange>,
    frames: Vec<FrameRef>,
    raw: Vec<f32>,
    features: Vec<f32>,
    mean: Vec<f32>,
    scale: Vec<f32>,
    tree: KdTree,
    // Tags, spans and landing biases baked per frame (`annotate`); empty = none.
    tags: TagTable,
    meta: Vec<FrameMeta>,
    min_bias: f32,
    // Frames carrying each tag bit, and frames that may be jump targets (for `Auto`).
    tag_counts: [u32; MAX_TAGS],
    target_count: usize,
    // Computed on first use by `median_jump_cost`.
    median_jump_cost: std::sync::OnceLock<f32>,
    // Set on a runtime weight overlay far from the build weights: every search scans.
    force_brute_force: bool,
    warnings: Vec<BuildWarning>,
    // Partner joint per joint for mirrored ranges; empty without any.
    mirror: Vec<u32>,
    // Per-frame clip ground speed (spec 52 § 2.10, ADR-7817): read from a cooked payload that
    // stores it, else computed from the raw root velocity at build or load (the same bits).
    clip_speed: Vec<f32>,
    // Arc-length arrays of distance-matched clips, by clip (spec 52 § 4, ADR-7817); `None` or
    // a short list = not stored, sampled from the clip on first use.
    arc_lengths: Vec<Option<ArcLengthArrays>>,
}

/// A frame's clip ground speed from its raw features: the length of the root velocity's ground
/// part (`x`, `z`). The cooked clip-speed array stores exactly this value.
fn root_ground_speed(raw: &[f32]) -> f32 {
    match raw {
        [x, _, z, ..] => (x * x + z * z).sqrt(),
        _ => 0.0,
    }
}

/// What a [`BuildWarning`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildWarningKind {
    /// Every foot velocity is within 1e-4 of zero for more than 90 % of the clip's frames while
    /// the root moves: the root motion is probably missing from the root joint's tracks.
    MissingRootMotion,
}

impl BuildWarningKind {
    /// The snake_case name (`missing_root_motion`).
    pub fn name(self) -> &'static str {
        match self {
            BuildWarningKind::MissingRootMotion => "missing_root_motion",
        }
    }

    fn code(self) -> u8 {
        match self {
            BuildWarningKind::MissingRootMotion => 1,
        }
    }
}

/// A problem the build found in the data (the database is still usable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildWarning {
    /// Index into [`MotionDatabase::clips`].
    pub clip: usize,
    /// What it is.
    pub kind: BuildWarningKind,
}

impl BuildWarning {
    fn from_code(clip: usize, code: u8) -> Option<Self> {
        match code {
            1 => Some(BuildWarning {
                clip,
                kind: BuildWarningKind::MissingRootMotion,
            }),
            _ => None,
        }
    }
}

/// The missing-root-motion check of one clip's raw features (spec 52 § 2.4).
fn missing_root_motion(schema: &FeatureSchema, raw: &[f32], frames: Range<usize>) -> bool {
    let dims = schema.dimensions();
    let feet = schema.group_range(FeatureGroup::FootVelocity);
    if feet.is_empty() || frames.is_empty() {
        return false;
    }
    let mut still = 0usize;
    for frame in frames.clone() {
        let row = &raw[frame * dims..(frame + 1) * dims];
        let root_moves = row[0..3].iter().any(|v| v.abs() > 1e-4);
        let feet_still = row[feet.clone()].iter().all(|v| v.abs() <= 1e-4);
        if root_moves && feet_still {
            still += 1;
        }
    }
    still * 10 > frames.len() * 9
}

impl MotionDatabase {
    /// Sample every clip at [`FeatureSchema::sample_rate`], extract the features, normalize them
    /// and build the k-d tree.
    ///
    /// A looping clip of duration `d` gives `ceil(d · rate)` frames (its end equals its start);
    /// a clip that does not loop gives `floor(d · rate) + 1` (its last frame is its end).
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::NoClips`], a schema error from [`FeatureSchema::validate`],
    /// [`MotionMatchError::BadClipDuration`], [`MotionMatchError::NonFiniteFeature`] or
    /// [`MotionMatchError::TooManyFrames`].
    pub fn build(
        skeleton: &Skeleton,
        clips: &[MotionClip<'_>],
        schema: FeatureSchema,
    ) -> Result<Self, MotionMatchError> {
        let entries: Vec<MotionEntry<'_>> = clips
            .iter()
            .map(|c| MotionEntry {
                clip: c.clip,
                looping: c.looping,
                mirrored: false,
            })
            .collect();
        Self::build_entries(skeleton, &entries, schema, None)
    }

    /// [`MotionDatabase::build`] with mirrored entries: a mirrored entry adds a second range,
    /// right after its own, sampled with left and right swapped and the lateral axis negated
    /// (`mirror` gives the joint pairs). Ranges record their entry ([`ClipRange::source`]).
    ///
    /// # Errors
    ///
    /// The errors of [`MotionDatabase::build`], [`MotionMatchError::NoMirrorTable`] for a
    /// mirrored entry without `mirror`, [`MotionMatchError::BadMirrorTable`].
    #[tracing::instrument(target = "regia_animation", name = "motion_matching.build", skip_all, fields(clips = entries.len()))]
    pub fn build_entries(
        skeleton: &Skeleton,
        entries: &[MotionEntry<'_>],
        schema: FeatureSchema,
        mirror: Option<&MirrorTable>,
    ) -> Result<Self, MotionMatchError> {
        if entries.is_empty() {
            return Err(MotionMatchError::NoClips);
        }
        schema.validate(skeleton)?;
        let partners = match (mirror, entries.iter().position(|e| e.mirrored)) {
            (_, None) => Vec::new(),
            (None, Some(entry)) => return Err(MotionMatchError::NoMirrorTable { entry }),
            (Some(table), Some(_)) => table.partners(skeleton)?,
        };
        let dims = schema.dimensions();
        let mut ranges = Vec::with_capacity(entries.len());
        let mut frames = Vec::new();
        for (index, source) in entries.iter().enumerate() {
            let duration = source.clip.duration;
            if !duration.is_finite() || duration <= TIME_EPSILON {
                return Err(MotionMatchError::BadClipDuration {
                    clip: source.clip.name.clone(),
                    duration,
                });
            }
            let count = frame_count(duration, schema.sample_rate, source.looping);
            for mirrored in [false, true] {
                if mirrored && !source.mirrored {
                    continue;
                }
                if frames.len().saturating_add(count) > u32::MAX as usize {
                    return Err(MotionMatchError::TooManyFrames(
                        frames.len().saturating_add(count),
                    ));
                }
                let clip = ranges.len();
                ranges.push(ClipRange {
                    name: if mirrored {
                        format!("{}.mirrored", source.clip.name)
                    } else {
                        source.clip.name.clone()
                    },
                    duration,
                    looping: source.looping,
                    start: frames.len(),
                    frames: count,
                    distance_matched: false,
                    source: index,
                    mirrored,
                });
                let step = 1.0 / schema.sample_rate;
                frames.extend((0..count).map(|k| FrameRef {
                    clip,
                    time: (k as f32 * step).min(duration),
                }));
            }
        }

        let mut raw = vec![0.0f32; frames.len() * dims];
        let mut extractor = Extractor::new(skeleton, &schema);
        extractor.partners = partners.clone();
        for (frame, values) in frames.iter().zip(raw.chunks_exact_mut(dims)) {
            let range = &ranges[frame.clip];
            let entry = entries[range.source];
            let source = MotionClip {
                clip: entry.clip,
                looping: entry.looping,
            };
            extractor.mirrored = range.mirrored;
            extractor.extract(source, frame.time, values);
            if values.iter().any(|v| !v.is_finite()) {
                return Err(MotionMatchError::NonFiniteFeature {
                    clip: source.clip.name.clone(),
                    time: frame.time,
                });
            }
        }
        if schema.contacts.is_some() {
            // Contact labels and phases need the whole clip (hysteresis, touchdown to touchdown).
            for range in &ranges {
                contacts::label_range(
                    &schema,
                    &mut raw[range.start * dims..(range.start + range.frames) * dims],
                    range.looping,
                );
            }
        }

        let (mean, scale) = normalization(&schema, &raw, frames.len());
        let target_count = frames.len();
        let warnings: Vec<BuildWarning> = ranges
            .iter()
            .enumerate()
            .filter(|(_, r)| missing_root_motion(&schema, &raw, r.range()))
            .map(|(clip, _)| BuildWarning {
                clip,
                kind: BuildWarningKind::MissingRootMotion,
            })
            .collect();
        for w in &warnings {
            tracing::warn!(target: "regia_animation", "motion database: clip {:?}: {}", ranges[w.clip].name, w.kind.name());
        }
        let mut features = raw.clone();
        for values in features.chunks_exact_mut(dims) {
            normalize_in_place(values, &mean, &scale);
        }
        let tree = KdTree::build(&features, dims).ok_or(MotionMatchError::TooManyFrames(frames.len()))?;
        let mut database = Self {
            schema,
            clips: ranges,
            frames,
            raw,
            features,
            mean,
            scale,
            tree,
            tags: TagTable::default(),
            meta: Vec::new(),
            min_bias: 0.0,
            tag_counts: [0; MAX_TAGS],
            target_count,
            median_jump_cost: std::sync::OnceLock::new(),
            force_brute_force: false,
            warnings,
            mirror: partners,
            clip_speed: Vec::new(),
            arc_lengths: Vec::new(),
        };
        database.bake_clip_speeds();
        Ok(database)
    }

    /// Store the per-frame clip ground speed (from the raw root velocity) for the cooked payload.
    pub(crate) fn bake_clip_speeds(&mut self) {
        let dims = self.dimensions();
        self.clip_speed = self.raw.chunks_exact(dims).map(root_ground_speed).collect();
    }

    /// Frame `frame`'s clip ground speed, meters per second (what stride warping compares the
    /// character's speed with, spec 52 § 2.10): the length of the ground part of the raw root
    /// velocity, read from the per-frame clip-speed array. `None` past the last frame.
    pub fn clip_speed(&self, frame: usize) -> Option<f32> {
        match self.clip_speed.get(frame) {
            Some(&speed) => Some(speed),
            None => self.raw_features(frame).map(root_ground_speed),
        }
    }

    /// The per-frame clip-speed array, one entry per frame (ADR-7817): read from a cooked
    /// payload of version 3, or computed from the stored raw features when the database was
    /// built or loaded from an older payload (the same values bit for bit).
    pub fn clip_speeds(&self) -> &[f32] {
        &self.clip_speed
    }

    /// The stored arc-length arrays of clip `clip` (spec 52 § 4): present for a distance-matched
    /// clip of a database built from a source, cooked at version 3 or later, or baked with
    /// [`MotionDatabase::bake_arc_lengths`]. `None` otherwise; the player then samples the clip.
    pub fn cooked_arc_lengths(&self, clip: usize) -> Option<&ArcLengthArrays> {
        self.arc_lengths.get(clip)?.as_ref()
    }

    /// Store the arc-length arrays of every distance-matched clip ([`ClipRange::distance_matched`])
    /// by sampling the root path at the database frames: `clips` are the build's clips in entry
    /// order ([`ClipRange::source`]), as [`player::MotionMatchPlayer::clips`]. A source build
    /// does this itself; call it to complete a database migrated from an older cooked payload.
    /// Replaces any stored arrays; clips that are not distance matched get none.
    ///
    /// # Errors
    ///
    /// A schema error from [`FeatureSchema::validate`] when `skeleton` does not fit, or
    /// [`MotionMatchError::UnknownClip`] when `clips` has no entry for a distance-matched clip.
    /// The database is unchanged on error.
    pub fn bake_arc_lengths(
        &mut self,
        skeleton: &Skeleton,
        clips: &[AnimationClip],
    ) -> Result<(), MotionMatchError> {
        self.bake_arc_lengths_with(skeleton, |entry| clips.get(entry))
    }

    pub(crate) fn bake_arc_lengths_with<'c>(
        &mut self,
        skeleton: &Skeleton,
        clip_of: impl Fn(usize) -> Option<&'c AnimationClip>,
    ) -> Result<(), MotionMatchError> {
        if !self.clips.iter().any(|c| c.distance_matched) {
            self.arc_lengths.clear();
            return Ok(());
        }
        self.schema.validate(skeleton)?;
        let mut extractor = Extractor::new(skeleton, &self.schema);
        extractor.partners = self.mirror.clone();
        let mut out = Vec::with_capacity(self.clips.len());
        for range in &self.clips {
            if !range.distance_matched {
                out.push(None);
                continue;
            }
            let clip = clip_of(range.source).ok_or_else(|| MotionMatchError::UnknownClip {
                entry: range.source,
                clip: range.name.clone(),
            })?;
            let source = MotionClip {
                clip,
                looping: range.looping,
            };
            extractor.mirrored = range.mirrored;
            let times: Vec<f32> = self.frames[range.range()].iter().map(|f| f.time).collect();
            let positions: Vec<Vec3> = times.iter().map(|&t| extractor.sample(source, t).root).collect();
            out.push(ArcLengthArrays::from_root_positions(&positions, &times));
        }
        self.arc_lengths = out;
        Ok(())
    }

    /// The mirror partner of every joint (empty when the database has no mirrored range).
    pub fn mirror_partners(&self) -> &[u32] {
        &self.mirror
    }

    /// Attach tags and spans (`docs/specs/52-motion-matching.md` § 2.5): `annotations` has one
    /// entry per clip, in clip order. The per-frame tag mask, landing bias and flags are baked
    /// once; normalization statistics and feature vectors do not change, so a filter changes
    /// which frames are eligible but never any cost.
    ///
    /// # Errors
    ///
    /// [`TagError::ClipCount`], [`TagError::BadSpan`], [`TagError::BadBias`]; the database is
    /// unchanged on error.
    pub fn annotate(&mut self, tags: TagTable, annotations: &[ClipAnnotation]) -> Result<(), TagError> {
        if annotations.len() != self.clips.len() {
            return Err(TagError::ClipCount {
                got: annotations.len(),
                expected: self.clips.len(),
            });
        }
        for annotation in annotations {
            validate_spans(&annotation.spans)?;
        }
        let mut meta = Vec::with_capacity(self.frames.len());
        for (index, annotation) in annotations.iter().enumerate() {
            let range = self.clips[index].range();
            let mirrored = self.clips[index].mirrored;
            meta.extend(
                tags::bake(annotation, self.frames[range].iter().map(|f| f.time))
                    .into_iter()
                    .map(|mut m| {
                        if mirrored {
                            m.flags |= FLAG_MIRRORED;
                        }
                        m
                    }),
            );
        }
        self.min_bias = meta.iter().map(|m| m.bias).fold(0.0, f32::min);
        let mut counts = [0u32; MAX_TAGS];
        for m in &meta {
            for (bit, count) in counts.iter_mut().enumerate() {
                if m.tags & (1 << bit) != 0 {
                    *count += 1;
                }
            }
        }
        self.tag_counts = counts;
        self.target_count = meta.iter().filter(|m| m.is_target()).count();
        self.meta = meta;
        self.tags = tags;
        Ok(())
    }

    /// The tag table (empty until [`MotionDatabase::annotate`]).
    pub fn tag_table(&self) -> &TagTable {
        &self.tags
    }

    /// The baked tags, bias and flags of `frame` (all zero without annotations); `None` past
    /// the end.
    pub fn frame_meta(&self, frame: usize) -> Option<FrameMeta> {
        (frame < self.frames.len()).then(|| self.meta.get(frame).copied().unwrap_or_default())
    }

    /// True once [`MotionDatabase::annotate`] has run.
    pub fn is_annotated(&self) -> bool {
        !self.meta.is_empty()
    }

    /// Frames carrying tag bit `bit` (0 past the table).
    pub fn tag_count(&self, bit: usize) -> u32 {
        self.tag_counts.get(bit).copied().unwrap_or(0)
    }

    /// Frames that may be jump targets (not pass-through or unindexed).
    pub fn target_count(&self) -> usize {
        self.target_count
    }

    /// The method [`SearchMethod::Auto`] resolves to for `filter`: the scan when the database
    /// has fewer than [`AUTO_TREE_MIN_FRAMES`] frames or the estimated eligible fraction (target
    /// frames, then the rarest required tag) is below 1/8, the tree otherwise. The estimate uses
    /// the stored per-tag counts; it never scans.
    pub fn resolve_method(&self, method: SearchMethod, filter: SearchFilter) -> SearchMethod {
        if self.force_brute_force {
            return SearchMethod::BruteForce;
        }
        match method {
            SearchMethod::Auto => {
                let frames = self.frames.len();
                if frames < AUTO_TREE_MIN_FRAMES {
                    return SearchMethod::BruteForce;
                }
                let mut eligible = self.target_count;
                for (bit, count) in self.tag_counts.iter().enumerate().take(MAX_TAGS) {
                    if filter.require & (1 << bit) != 0 {
                        eligible = eligible.min(*count as usize);
                    }
                }
                if eligible * 8 < frames {
                    SearchMethod::BruteForce
                } else {
                    SearchMethod::KdTree
                }
            }
            other => other,
        }
    }

    /// The cheapest eligible frame by biased cost (`distance + landing bias + loop_clip_bias`
    /// for frames of looping clips). Eligible frames are not excluded, not `locked`, not
    /// pass-through or unindexed, and accepted by `filter`. Ties go to the lowest frame index,
    /// and every method returns the same frame. With no annotations, no filter, no loop bias and
    /// nothing locked it equals [`MotionDatabase::search`]. The hit's `cost` is the biased cost.
    pub fn search_with(
        &self,
        normalized: &[f32],
        exclude: Exclusion,
        filter: SearchFilter,
        loop_clip_bias: f32,
        locked: impl Fn(usize) -> bool,
        method: SearchMethod,
    ) -> Option<SearchHit> {
        self.search_detailed(normalized, exclude, filter, loop_clip_bias, locked, method)
            .hit
    }

    /// [`MotionDatabase::search_with`] without a loop bias or lock-out
    /// (`docs/specs/52-motion-matching.md` § 3).
    pub fn search_filtered(
        &self,
        normalized: &[f32],
        filter: SearchFilter,
        exclude: Exclusion,
        method: SearchMethod,
    ) -> Option<SearchHit> {
        self.search_with(normalized, exclude, filter, 0.0, |_| false, method)
    }

    /// [`MotionDatabase::search_with`] that also reports the method used and the work done.
    #[tracing::instrument(
        target = "regia_animation",
        name = "motion_matching.search",
        skip_all,
        fields(
            method_used = tracing::field::Empty,
            eligible = self.target_count,
            visited_nodes = tracing::field::Empty
        )
    )]
    pub fn search_detailed(
        &self,
        normalized: &[f32],
        exclude: Exclusion,
        filter: SearchFilter,
        loop_clip_bias: f32,
        locked: impl Fn(usize) -> bool,
        method: SearchMethod,
    ) -> SearchOutcome {
        let method_used = self.resolve_method(method, filter);
        let loop_bias = clean_bias(loop_clip_bias);
        let excluded = |row: usize| {
            let meta = self.meta.get(row).copied().unwrap_or_default();
            exclude.contains(row) || locked(row) || !meta.is_target() || !filter.accepts(meta.tags)
        };
        let bias = |row: usize| self.landing_bias(row, loop_bias);
        let (nearest, work) = match method_used {
            SearchMethod::KdTree => {
                let min_bias = self.min_bias.min(0.0) + loop_bias.min(0.0);
                self.tree
                    .nearest_biased_counted(normalized, excluded, bias, min_bias)
            }
            _ => nearest_brute_force_biased_counted(
                &self.features,
                self.dimensions(),
                normalized,
                excluded,
                bias,
            ),
        };
        let span = tracing::Span::current();
        span.record("method_used", method_used.name());
        span.record("visited_nodes", work.nodes_visited);
        SearchOutcome {
            hit: nearest.map(|n| SearchHit {
                frame: n.row,
                cost: n.distance,
            }),
            method_used,
            work,
        }
    }

    /// The bias a frame adds as a jump target: its landing bias plus `loop_clip_bias` when its
    /// clip loops.
    fn landing_bias(&self, row: usize, loop_bias: f32) -> f32 {
        let own = self.meta.get(row).map_or(0.0, |m| m.bias);
        let looping = self
            .frames
            .get(row)
            .and_then(|f| self.clips.get(f.clip))
            .is_some_and(|c| c.looping);
        own + if looping { loop_bias } else { 0.0 }
    }

    /// The `k` cheapest eligible frames (at most 32) by biased cost (distance plus landing
    /// bias; no loop bias or lock-out), cheapest first, ties by frame index. Uses the exact tree
    /// with a bounded best list; equal to sorting every eligible frame.
    pub fn search_top_k(
        &self,
        normalized: &[f32],
        filter: SearchFilter,
        exclude: Exclusion,
        k: usize,
    ) -> Vec<SearchHit> {
        let excluded = |row: usize| {
            let meta = self.meta.get(row).copied().unwrap_or_default();
            exclude.contains(row) || !meta.is_target() || !filter.accepts(meta.tags)
        };
        let bias = |row: usize| self.meta.get(row).map_or(0.0, |m| m.bias);
        self.tree
            .nearest_k_biased(normalized, excluded, bias, self.min_bias.min(0.0), k.min(32))
            .0
            .into_iter()
            .map(|n| SearchHit {
                frame: n.row,
                cost: n.distance,
            })
            .collect()
    }

    /// The cost of frame `frame` against a normalized query split per group (the sum of
    /// squared normalized differences over each group's slice), plus the frame's landing bias.
    /// `total` is the canonical distance ([`squared_distance`]); the group costs add up to it
    /// within rounding. `None` for a frame past the end or a query of the wrong length.
    pub fn cost_parts(&self, normalized: &[f32], frame: usize) -> Option<CostParts> {
        let features = self.features(frame)?;
        if normalized.len() != features.len() {
            return None;
        }
        let mut groups = [0.0f32; FeatureGroup::COUNT];
        for group in FeatureGroup::ALL {
            let range = self.schema.group_range(group);
            groups[group.index()] = normalized[range.clone()]
                .iter()
                .zip(&features[range])
                .map(|(a, b)| (a - b) * (a - b))
                .sum();
        }
        Some(CostParts {
            total: squared_distance(normalized, features),
            groups,
            bias: self.meta.get(frame).map_or(0.0, |m| m.bias),
        })
    }

    /// The median cost of a jump in this database: over every frame (or a deterministic
    /// subsample of 256 evenly spaced frames in a larger database), the cost of the nearest frame
    /// outside a quarter second of it in the same clip; the median of those. The player's
    /// default blend-duration reference. 1 when every such cost is zero. Computed once.
    pub fn median_jump_cost(&self) -> f32 {
        *self.median_jump_cost.get_or_init(|| {
            let frames = self.frames.len();
            let step = (frames / 256).max(1);
            let radius = (0.25 * self.schema.sample_rate).round() as usize;
            let mut costs: Vec<f32> = (0..frames)
                .step_by(step)
                .filter_map(|frame| {
                    let exclude = self.exclusion_around(frame, radius);
                    let query = self.features(frame)?;
                    self.tree
                        .nearest(query, |row| exclude.contains(row))
                        .map(|n| n.distance)
                })
                .collect();
            costs.sort_by(f32::total_cmp);
            costs
                .get(costs.len() / 2)
                .copied()
                .filter(|c| c.is_finite() && *c > 0.0)
                .unwrap_or(1.0)
        })
    }

    /// A copy of the database searched with other group weights (an agent's runtime tuning
    /// overlay, `docs/specs/52-motion-matching.md` § 5): the raw features, statistics, frames,
    /// tags and tree shape are kept; the scale and the normalized features are recomputed with
    /// `weights`, and the tree is re-read over them (exact, because a per-dimension rescale by a
    /// non-negative factor keeps the tree's partition). When any group's weight moves beyond
    /// ±50 % of its build weight the copy always scans ([`SearchMethod::BruteForce`]), since the
    /// tree's pruning degrades; [`SearchOutcome::method_used`] reports it.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::BadWeight`] for a negative, non-finite or too large weight.
    pub fn with_weight_overlay(&self, weights: FeatureWeights) -> Result<MotionDatabase, MotionMatchError> {
        let mut schema = self.schema.clone();
        schema.weights = weights;
        for group in FeatureGroup::ALL {
            let value = weights.get(group);
            if !value.is_finite() || !(0.0..=MAX_GROUP_WEIGHT).contains(&value) {
                return Err(MotionMatchError::BadWeight {
                    group: group.name(),
                    value,
                });
            }
        }
        let far = FeatureGroup::ALL.into_iter().any(|group| {
            let (old, new) = (self.schema.weights.get(group), weights.get(group));
            if old == 0.0 {
                new != 0.0
            } else {
                !(0.5..=1.5).contains(&(new / old))
            }
        });
        let (_, scale) = normalization(&schema, &self.raw, self.frames.len());
        let dims = self.dimensions();
        let mut features = self.raw.clone();
        for values in features.chunks_exact_mut(dims) {
            normalize_in_place(values, &self.mean, &scale);
        }
        let tree = self
            .tree
            .with_rows(features.clone())
            .ok_or(MotionMatchError::TooManyFrames(self.frames.len()))?;
        Ok(MotionDatabase {
            schema,
            features,
            scale,
            tree,
            median_jump_cost: std::sync::OnceLock::new(),
            force_brute_force: self.force_brute_force || far,
            ..self.clone()
        })
    }

    /// True when every search of this database scans (a far weight overlay).
    pub fn forces_brute_force(&self) -> bool {
        self.force_brute_force
    }

    /// A 64-bit FNV-1a hash of the per-dimension mean and scale (little-endian bits).
    pub fn mean_scale_hash(&self) -> u64 {
        let mut hash = Fnv1a::new();
        for v in self.mean.iter().chain(&self.scale) {
            hash.write(&v.to_le_bytes());
        }
        hash.finish()
    }

    /// The schema the database was built with.
    pub fn schema(&self) -> &FeatureSchema {
        &self.schema
    }

    /// Floats per feature vector.
    pub fn dimensions(&self) -> usize {
        self.schema.dimensions()
    }

    /// Number of frames.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// True when the database has no frames (never true for a built database).
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The clips, in the order given to [`MotionDatabase::build`].
    pub fn clips(&self) -> &[ClipRange] {
        &self.clips
    }

    /// Where frame `frame` comes from.
    pub fn frame(&self, frame: usize) -> Option<FrameRef> {
        self.frames.get(frame).copied()
    }

    /// Frame `frame`'s features before normalization.
    pub fn raw_features(&self, frame: usize) -> Option<&[f32]> {
        let dims = self.dimensions();
        self.raw.get(frame * dims..(frame + 1) * dims)
    }

    /// Frame `frame`'s normalized, weighted features (what the search compares).
    pub fn features(&self, frame: usize) -> Option<&[f32]> {
        let dims = self.dimensions();
        self.features.get(frame * dims..(frame + 1) * dims)
    }

    /// The per-dimension mean removed by normalization.
    pub fn mean(&self) -> &[f32] {
        &self.mean
    }

    /// The per-dimension factor normalization multiplies by (group weight over group deviation).
    pub fn scale(&self) -> &[f32] {
        &self.scale
    }

    /// Warnings the build attached (also in the agent output).
    pub fn warnings(&self) -> &[BuildWarning] {
        &self.warnings
    }

    /// Rows per leaf of the k-d tree.
    pub fn kd_leaf_size(&self) -> usize {
        self.tree.leaf_size()
    }

    /// Node and depth counts of the k-d tree.
    pub fn kd_stats(&self) -> KdStats {
        self.tree.stats()
    }

    /// The database frame nearest to `time` in clip `clip` (a looping clip wraps `time`, one that
    /// does not loop clamps it). `None` for an unknown clip or a non-finite time.
    pub fn frame_at(&self, clip: usize, time: f32) -> Option<usize> {
        let range = self.clips.get(clip)?;
        if !time.is_finite() {
            return None;
        }
        let rate = self.schema.sample_rate;
        let duration = range.safe_duration();
        let index = if range.looping {
            let wrapped = time.rem_euclid(duration);
            ((wrapped * rate).round() as usize) % range.frames.max(1)
        } else {
            ((time.clamp(0.0, duration) * rate).round() as usize).min(range.frames - 1)
        };
        Some(range.start + index)
    }

    /// The frames around `frame` in the same clip, `radius` frames to each side (wrapping in a
    /// looping clip, clamped to a clip that does not), `frame` included.
    pub fn exclusion_around(&self, frame: usize, radius: usize) -> Exclusion {
        let Some(at) = self.frames.get(frame) else {
            return Exclusion::NONE;
        };
        let range = &self.clips[at.clip];
        let local = frame - range.start;
        // A radius past the clip covers all of it; capping it keeps `2 * radius + 1` and
        // `local + radius + 1` from overflowing for a huge window (settings accept any finite
        // `exclude_window`, which saturates to `usize::MAX` here).
        let radius = radius.min(range.frames);
        if range.looping && 2 * radius + 1 >= range.frames {
            return Exclusion::range(range.range());
        }
        if range.looping {
            let lo = local as isize - radius as isize;
            let hi = local + radius + 1;
            let mut spans = [(0, 0); 2];
            if lo < 0 {
                spans[0] = (range.start, range.start + hi);
                spans[1] = (
                    range.start + (range.frames as isize + lo) as usize,
                    range.start + range.frames,
                );
            } else if hi > range.frames {
                spans[0] = (range.start + lo as usize, range.start + range.frames);
                spans[1] = (range.start, range.start + hi - range.frames);
            } else {
                spans[0] = (range.start + lo as usize, range.start + hi);
            }
            Exclusion { spans }
        } else {
            let lo = local.saturating_sub(radius);
            let hi = (local + radius + 1).min(range.frames);
            Exclusion::range(range.start + lo..range.start + hi)
        }
    }

    /// The contact label of every foot at `frame` (`true` = planted); empty when the schema has
    /// no [`FeatureSchema::contacts`], `None` past the end.
    pub fn contacts(&self, frame: usize) -> Option<Vec<bool>> {
        self.raw_features(frame)
            .map(|raw| contacts::row_contacts(&self.schema, raw))
    }

    /// The gait phase in `[0, 1)` of every foot at `frame` (`None` where the cycle is undefined);
    /// empty without the phase group, `None` past the end.
    pub fn phases(&self, frame: usize) -> Option<Vec<Option<f32>>> {
        self.raw_features(frame)
            .map(|raw| contacts::row_phases(&self.schema, raw))
    }

    /// Decode frame `frame`'s raw features into a [`MotionQuery`].
    pub fn frame_query(&self, frame: usize) -> Option<MotionQuery> {
        self.raw_features(frame).map(|raw| decode(&self.schema, raw))
    }

    /// A query from frame `frame`'s pose features (root and feet) with `trajectory` as the
    /// desired path: what the runtime asks every search.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::FrameOutOfRange`].
    pub fn query_from_frame(
        &self,
        frame: usize,
        trajectory: &[TrajectoryPoint],
    ) -> Result<MotionQuery, MotionMatchError> {
        let mut query = self.frame_query(frame).ok_or(MotionMatchError::FrameOutOfRange {
            frame,
            frames: self.len(),
        })?;
        query.trajectory = trajectory.to_vec();
        Ok(query)
    }

    /// A query with the database's mean pose features (a neutral pose: every pose dimension
    /// normalizes to zero) and `trajectory` as the desired path. Used for the first search,
    /// before anything plays.
    pub fn query_with_mean_pose(&self, trajectory: &[TrajectoryPoint]) -> MotionQuery {
        let mut query = decode(&self.schema, &self.mean);
        query.trajectory = trajectory.to_vec();
        query
    }

    /// Encode and normalize `query` into the space the search compares in.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::QueryShape`] when the trajectory or the feet do not match the schema,
    /// [`MotionMatchError::NonFiniteQuery`] for a non-finite value.
    pub fn normalize(&self, query: &MotionQuery) -> Result<Vec<f32>, MotionMatchError> {
        let points = self.schema.trajectory_times.len();
        if query.trajectory.len() != points {
            return Err(MotionMatchError::QueryShape {
                part: "trajectory points",
                got: query.trajectory.len(),
                expected: points,
            });
        }
        if query.feet.len() != self.schema.feet.len() {
            return Err(MotionMatchError::QueryShape {
                part: "feet",
                got: query.feet.len(),
                expected: self.schema.feet.len(),
            });
        }
        if query.history.len() != self.schema.history_times.len() {
            return Err(MotionMatchError::QueryShape {
                part: "history points",
                got: query.history.len(),
                expected: self.schema.history_times.len(),
            });
        }
        if query.pose_joints.len() != self.schema.pose_joints.len() {
            return Err(MotionMatchError::QueryShape {
                part: "pose joints",
                got: query.pose_joints.len(),
                expected: self.schema.pose_joints.len(),
            });
        }
        if query.contacts.len() != self.schema.contact_count() {
            return Err(MotionMatchError::QueryShape {
                part: "contacts",
                got: query.contacts.len(),
                expected: self.schema.contact_count(),
            });
        }
        if query.phases.len() != self.schema.phase_count() {
            return Err(MotionMatchError::QueryShape {
                part: "phases",
                got: query.phases.len(),
                expected: self.schema.phase_count(),
            });
        }
        let mut values = vec![0.0f32; self.dimensions()];
        encode(&self.schema, query, &mut values);
        if values.iter().any(|v| !v.is_finite()) {
            return Err(MotionMatchError::NonFiniteQuery);
        }
        normalize_in_place(&mut values, &self.mean, &self.scale);
        Ok(values)
    }

    /// The cost of frame `frame` against a normalized query (see [`MotionDatabase::normalize`]).
    /// `None` for a frame past the end or a query of the wrong length.
    pub fn cost(&self, normalized: &[f32], frame: usize) -> Option<f32> {
        let features = self.features(frame)?;
        (normalized.len() == features.len()).then(|| squared_distance(normalized, features))
    }

    /// The cheapest frame for a normalized query, skipping `exclude`, ignoring tags, spans and
    /// biases. Every method returns the same frame (ties go to the lowest index). `None` when the query has the wrong length or
    /// every frame is excluded.
    pub fn search(&self, normalized: &[f32], exclude: Exclusion, method: SearchMethod) -> Option<SearchHit> {
        let excluded = |row: usize| exclude.contains(row);
        let nearest = match self.resolve_method(method, SearchFilter::NONE) {
            SearchMethod::KdTree => self.tree.nearest(normalized, excluded),
            _ => nearest_brute_force(&self.features, self.dimensions(), normalized, excluded),
        };
        nearest.map(|n| SearchHit {
            frame: n.row,
            cost: n.distance,
        })
    }

    /// Normalize `query` and search (see [`MotionDatabase::search`]).
    ///
    /// # Errors
    ///
    /// The errors of [`MotionDatabase::normalize`].
    pub fn search_query(
        &self,
        query: &MotionQuery,
        exclude: Exclusion,
        method: SearchMethod,
    ) -> Result<Option<SearchHit>, MotionMatchError> {
        let normalized = self.normalize(query)?;
        Ok(self.search(&normalized, exclude, method))
    }
}

/// 64-bit FNV-1a (Fowler, Noll, Vo; public domain).
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A signed bias clamped to `[-MAX_BIAS, MAX_BIAS]`; non-finite is 0.
fn clean_bias(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-MAX_BIAS, MAX_BIAS)
    } else {
        0.0
    }
}

/// Database frames of one clip.
fn frame_count(duration: f32, rate: f32, looping: bool) -> usize {
    let span = duration * rate;
    if looping {
        ((span - 1e-3).ceil() as usize).max(1)
    } else {
        (span + 1e-3).floor() as usize + 1
    }
}

/// Per-dimension mean and normalization factor (weight over deviation, per the schema's
/// [`NormalizationMode`]). Accumulated in `f64`, in frame order.
fn normalization(schema: &FeatureSchema, raw: &[f32], frames: usize) -> (Vec<f32>, Vec<f32>) {
    let dims = schema.dimensions();
    let mut mean = vec![0.0f64; dims];
    for values in raw.chunks_exact(dims) {
        for (m, &v) in mean.iter_mut().zip(values) {
            *m += f64::from(v);
        }
    }
    let count = frames.max(1) as f64;
    for m in &mut mean {
        *m /= count;
    }
    let mut variance = vec![0.0f64; dims];
    for values in raw.chunks_exact(dims) {
        for ((s, &v), &m) in variance.iter_mut().zip(values).zip(&mean) {
            let d = f64::from(v) - m;
            *s += d * d;
        }
    }
    let clean = |deviation: f32| {
        if deviation.is_finite() && deviation > MIN_DEVIATION {
            deviation
        } else {
            1.0
        }
    };
    let weights = schema.dimension_weights();
    let mut scale = vec![0.0f32; dims];
    for group in FeatureGroup::ALL {
        let range = schema.group_range(group);
        if range.is_empty() {
            continue;
        }
        let group_variance = variance[range.clone()].iter().sum::<f64>() / (count * range.len() as f64);
        let group_deviation = clean(group_variance.sqrt() as f32);
        for d in range {
            let deviation = match schema.normalization {
                NormalizationMode::GroupDeviation => group_deviation,
                NormalizationMode::PerDimension => clean((variance[d] / count).sqrt() as f32),
                NormalizationMode::None => 1.0,
            };
            scale[d] = weights[d] / deviation;
        }
    }
    (mean.into_iter().map(|m| m as f32).collect(), scale)
}

fn normalize_in_place(values: &mut [f32], mean: &[f32], scale: &[f32]) {
    for ((v, &m), &s) in values.iter_mut().zip(mean).zip(scale) {
        *v = (*v - m) * s;
    }
}

/// Write `query` into a raw feature vector laid out by `schema` (the lengths are checked by the
/// caller; missing entries stay zero).
fn encode(schema: &FeatureSchema, query: &MotionQuery, out: &mut [f32]) {
    out[0..3].copy_from_slice(&query.root_velocity.to_array());
    let positions = schema.group_range(FeatureGroup::TrajectoryPosition);
    let directions = schema.group_range(FeatureGroup::TrajectoryDirection);
    for (i, point) in query.trajectory.iter().enumerate() {
        let p = positions.start + 2 * i;
        let d = directions.start + 2 * i;
        if d + 2 > directions.end {
            break;
        }
        out[p..p + 2].copy_from_slice(&point.position.to_array());
        out[d..d + 2].copy_from_slice(&point.direction.to_array());
    }
    let foot_positions = schema.group_range(FeatureGroup::FootPosition);
    let foot_velocities = schema.group_range(FeatureGroup::FootVelocity);
    for (i, foot) in query.feet.iter().enumerate() {
        let p = foot_positions.start + 3 * i;
        let v = foot_velocities.start + 3 * i;
        if v + 3 > foot_velocities.end {
            break;
        }
        out[p..p + 3].copy_from_slice(&foot.position.to_array());
        out[v..v + 3].copy_from_slice(&foot.velocity.to_array());
    }
    let history = schema.group_range(FeatureGroup::TrajectoryHistory);
    for (i, point) in query.history.iter().enumerate() {
        let h = history.start + 2 * i;
        if h + 2 > history.end {
            break;
        }
        out[h..h + 2].copy_from_slice(&point.to_array());
    }
    let mut p = schema.group_range(FeatureGroup::PosePosition).start;
    let mut v = schema.group_range(FeatureGroup::PoseVelocity).start;
    for (joint, feature) in schema.pose_joints.iter().zip(&query.pose_joints) {
        if joint.position {
            out[p..p + 3].copy_from_slice(&feature.position.to_array());
            p += 3;
        }
        if joint.velocity {
            out[v..v + 3].copy_from_slice(&feature.velocity.to_array());
            v += 3;
        }
    }
    let contacts = schema.group_range(FeatureGroup::FootContact);
    for (slot, &value) in out[contacts].iter_mut().zip(&query.contacts) {
        *slot = value;
    }
    let phases = schema.group_range(FeatureGroup::FootPhase);
    for (pair, value) in out[phases].as_chunks_mut::<2>().0.iter_mut().zip(&query.phases) {
        pair.copy_from_slice(&value.to_array());
    }
}

/// Read a raw feature vector laid out by `schema` back into a query.
fn decode(schema: &FeatureSchema, raw: &[f32]) -> MotionQuery {
    let at3 = |i: usize| Vec3::new(raw[i], raw[i + 1], raw[i + 2]);
    let at2 = |i: usize| Vec2::new(raw[i], raw[i + 1]);
    let positions = schema.group_range(FeatureGroup::TrajectoryPosition).start;
    let directions = schema.group_range(FeatureGroup::TrajectoryDirection).start;
    let foot_positions = schema.group_range(FeatureGroup::FootPosition).start;
    let foot_velocities = schema.group_range(FeatureGroup::FootVelocity).start;
    let history = schema.group_range(FeatureGroup::TrajectoryHistory).start;
    let mut p = schema.group_range(FeatureGroup::PosePosition).start;
    let mut v = schema.group_range(FeatureGroup::PoseVelocity).start;
    let pose_joints = schema
        .pose_joints
        .iter()
        .map(|joint| {
            let mut feature = FootFeature::default();
            if joint.position {
                feature.position = at3(p);
                p += 3;
            }
            if joint.velocity {
                feature.velocity = at3(v);
                v += 3;
            }
            feature
        })
        .collect();
    MotionQuery {
        root_velocity: at3(0),
        trajectory: (0..schema.trajectory_times.len())
            .map(|i| TrajectoryPoint {
                position: at2(positions + 2 * i),
                direction: at2(directions + 2 * i),
            })
            .collect(),
        feet: (0..schema.feet.len())
            .map(|i| FootFeature {
                position: at3(foot_positions + 3 * i),
                velocity: at3(foot_velocities + 3 * i),
            })
            .collect(),
        history: (0..schema.history_times.len())
            .map(|i| at2(history + 2 * i))
            .collect(),
        pose_joints,
        contacts: raw[schema.group_range(FeatureGroup::FootContact)].to_vec(),
        phases: raw[schema.group_range(FeatureGroup::FootPhase)]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[c, s]| Vec2::new(c, s))
            .collect(),
    }
}

/// The root joint and the feet in object space at one (unwrapped) clip time.
struct Sample {
    root: Vec3,
    /// Yaw of the facing direction about +Y (0 = facing +Z).
    yaw: f32,
    /// The feet, then the pose joints, in schema order.
    joints: Vec<Vec3>,
}

/// Samples clips into object-space joint positions, reusing its pose buffer.
struct Extractor<'s> {
    skeleton: &'s Skeleton,
    schema: &'s FeatureSchema,
    bind: Pose,
    pose: Pose,
    /// Partner joint per joint (for mirrored ranges).
    partners: Vec<u32>,
    /// Sample mirrored (needs `partners`).
    mirrored: bool,
}

impl<'s> Extractor<'s> {
    fn new(skeleton: &'s Skeleton, schema: &'s FeatureSchema) -> Self {
        let bind = skeleton.bind_pose_pose();
        Self {
            skeleton,
            schema,
            pose: bind.clone(),
            bind,
            partners: Vec::new(),
            mirrored: false,
        }
    }

    /// Object-space joint matrices of `clip` at clip-local time `time` (already in range),
    /// mirrored when `mirrored` is set (`G'_j = S · G_partner(j) · S`, `S` negating X).
    fn globals(&mut self, clip: &AnimationClip, time: f32) -> Vec<Mat4> {
        self.pose.clone_from(&self.bind);
        clip.sample_pose(time, &mut self.pose);
        if self.mirrored && !self.partners.is_empty() {
            mirror_pose(&mut self.pose, &self.partners);
        }
        self.skeleton.globals(&self.pose, None)
    }

    /// The sample at unwrapped time `time`: a looping clip applies its lap transform once per
    /// lap (`G(k·d + t) = C^k · G(t)`, `C = G_root(d) · G_root(0)⁻¹`), a clip that does not loop
    /// clamps to its ends.
    fn sample(&mut self, source: MotionClip<'_>, time: f32) -> Sample {
        let clip = source.clip;
        // A cooked database or clip is untrusted: out-of-range schema joints or a non-finite
        // duration give an empty sample instead of a panic.
        let joints = self.skeleton.len();
        let named = std::iter::once(self.schema.root_joint)
            .chain(self.schema.feet.iter().copied())
            .chain(self.schema.pose_joints.iter().map(|j| j.joint));
        if named.clone().any(|j| j as usize >= joints) {
            return Sample {
                root: Vec3::ZERO,
                yaw: 0.0,
                joints: vec![Vec3::ZERO; self.schema.feet.len() + self.schema.pose_joints.len()],
            };
        }
        let duration = if clip.duration.is_finite() {
            clip.duration.max(0.0)
        } else {
            0.0
        };
        let (local, lap) = if source.looping {
            let laps = if duration > 0.0 {
                (time / duration).floor()
            } else {
                0.0
            };
            let local = (time - laps * duration).clamp(0.0, duration);
            (local, laps as i32)
        } else {
            (time.clamp(0.0, duration), 0)
        };
        let globals = self.globals(clip, local);
        let lap_transform = if lap == 0 {
            Mat4::IDENTITY
        } else {
            let root = self.schema.root_joint as usize;
            let end = self.globals(clip, duration)[root];
            let start = self.globals(clip, 0.0)[root];
            mat_pow(end * start.inverse(), lap)
        };
        let root_matrix = lap_transform * globals[self.schema.root_joint as usize];
        let facing = root_matrix.transform_vector3(self.schema.forward);
        let yaw = if facing.x * facing.x + facing.z * facing.z > 1e-12 {
            det_atan2(facing.x, facing.z)
        } else {
            0.0
        };
        Sample {
            root: root_matrix.w_axis.truncate(),
            yaw,
            joints: self
                .schema
                .feet
                .iter()
                .copied()
                .chain(self.schema.pose_joints.iter().map(|j| j.joint))
                .map(|j| lap_transform.transform_point3(globals[j as usize].w_axis.truncate()))
                .collect(),
        }
    }

    /// Write the features of `source` at clip time `time` into `out`.
    fn extract(&mut self, source: MotionClip<'_>, time: f32, out: &mut [f32]) {
        let step = 1.0 / self.schema.sample_rate;
        let duration = source.clip.duration;
        let (before, after) = if source.looping {
            (time - step, time + step)
        } else {
            ((time - step).max(0.0), (time + step).min(duration))
        };
        let now = self.sample(source, time);
        let prev = self.sample(source, before);
        let next = self.sample(source, after);
        let span = after - before;
        let velocity = |a: Vec3, b: Vec3| {
            if span > TIME_EPSILON {
                (b - a) / span
            } else {
                Vec3::ZERO
            }
        };
        let ground = Vec3::new(now.root.x, 0.0, now.root.z);
        let to_local = det_rotation_y(-now.yaw);

        let mut query = MotionQuery {
            root_velocity: to_local * velocity(prev.root, next.root),
            trajectory: Vec::with_capacity(self.schema.trajectory_times.len()),
            feet: Vec::with_capacity(self.schema.feet.len()),
            history: Vec::with_capacity(self.schema.history_times.len()),
            pose_joints: Vec::with_capacity(self.schema.pose_joints.len()),
            // Filled per clip range after extraction (`contacts::label_range`).
            contacts: Vec::new(),
            phases: Vec::new(),
        };
        let schema = self.schema;
        for &ahead in &schema.trajectory_times {
            let future = self.sample(source, time + ahead);
            let offset = to_local * (Vec3::new(future.root.x, 0.0, future.root.z) - ground);
            let yaw = future.yaw - now.yaw;
            query.trajectory.push(TrajectoryPoint {
                position: Vec2::new(offset.x, offset.z),
                direction: {
                    let (sin, cos) = det_sin_cos(yaw);
                    Vec2::new(sin, cos)
                },
            });
        }
        for &behind in &schema.history_times {
            // A looping clip wraps into the previous lap (negative lap transform); one that
            // does not loop clamps to its start.
            let past = self.sample(source, time - behind);
            let offset = to_local * (Vec3::new(past.root.x, 0.0, past.root.z) - ground);
            query.history.push(Vec2::new(offset.x, offset.z));
        }
        let feet = schema.feet.len();
        for (index, ((&position, &a), &b)) in
            now.joints.iter().zip(&prev.joints).zip(&next.joints).enumerate()
        {
            let feature = FootFeature {
                position: to_local * (position - ground),
                velocity: to_local * velocity(a, b),
            };
            if index < feet {
                query.feet.push(feature);
            } else {
                query.pose_joints.push(feature);
            }
        }
        encode(schema, &query, out);
    }
}

/// `m` raised to an integer power (`m⁻¹` powers for negative exponents).
fn mat_pow(m: Mat4, exponent: i32) -> Mat4 {
    let base = if exponent < 0 { m.inverse() } else { m };
    let mut out = Mat4::IDENTITY;
    for _ in 0..exponent.unsigned_abs() {
        out = base * out;
    }
    out
}

/// Runtime settings of a [`MotionMatcher`]. Serializes to RON (missing fields take their
/// defaults), so a game can keep its tuning in a settings file. Reflected (spec 52 § 3,
/// ADR-7216), so an inspector or an agent edits it by path (`matcher.settings.play_rate` on a
/// [`MotionMatching`]).
#[derive(Clone, Copy, Debug, PartialEq, Reflect, serde::Serialize, serde::Deserialize)]
#[reflect(Default, PartialEq)]
#[serde(default)]
pub struct MatcherSettings {
    /// Seconds between searches (0 searches every update).
    pub search_interval: f32,
    /// Seconds after a jump before the next jump may happen.
    pub min_jump_interval: f32,
    /// Cost added to every jump candidate (in normalized squared-distance units); the playing
    /// frame's continuation does not pay it.
    pub transition_cost: f32,
    /// Seconds around the playing frame, in the same clip, that are not jump targets.
    pub exclude_window: f32,
    /// How the database is searched.
    pub method: SearchMethod,
    /// Added to the continuation's cost; negative holds the playing clip longer, positive
    /// releases it sooner (clamped to +-100).
    pub stay_bias: f32,
    /// Added to every candidate that belongs to a looping clip (clamped to +-100).
    pub loop_clip_bias: f32,
    /// Seconds during which a frame chosen as a jump target is not a candidate again.
    pub lockout_window: f32,
    /// When set, a search runs at the next update (regardless of the interval) once the
    /// Euclidean distance between the normalized trajectory features of the desired path now
    /// and at the last search exceeds this value. `None` = off.
    pub urgent_threshold: Option<f32>,
    /// Seconds since the last search before an urgent search may run (0 = the next update).
    pub min_search_gap: f32,
    /// Offset the first search interval by `(slot mod 8) / 8 · search_interval` so many
    /// characters do not search on the same frame ([`MotionMatcher::set_stagger_slot`]; the
    /// plugin's system uses the entity index).
    pub stagger: bool,
    /// Multiplier on playback time (searches and jump timers keep real time).
    pub play_rate: f32,
    /// Blend of the query's pose features between the playing frame's stored features (0, the
    /// baseline) and the host's [`LivePose`] (1).
    pub live_pose_blend: f32,
}

impl Default for MatcherSettings {
    fn default() -> Self {
        Self {
            search_interval: 0.1,
            min_jump_interval: 0.2,
            transition_cost: 0.1,
            exclude_window: 0.25,
            method: SearchMethod::Auto,
            stay_bias: 0.0,
            loop_clip_bias: 0.0,
            lockout_window: 0.0,
            urgent_threshold: None,
            min_search_gap: 0.0,
            stagger: true,
            play_rate: 1.0,
            live_pose_blend: 0.0,
        }
    }
}

impl MatcherSettings {
    /// The settings with non-finite or negative values replaced by zero (signed biases clamped
    /// to +-100, the play rate defaulting to 1 when not finite, the blend clamped to `[0, 1]`).
    pub fn sanitized(self) -> Self {
        let clean = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
        Self {
            search_interval: clean(self.search_interval),
            min_jump_interval: clean(self.min_jump_interval),
            transition_cost: clean(self.transition_cost),
            exclude_window: clean(self.exclude_window),
            method: self.method,
            stay_bias: clean_bias(self.stay_bias),
            loop_clip_bias: clean_bias(self.loop_clip_bias),
            lockout_window: clean(self.lockout_window),
            urgent_threshold: self.urgent_threshold.map(clean),
            min_search_gap: clean(self.min_search_gap),
            stagger: self.stagger,
            play_rate: if self.play_rate.is_finite() {
                self.play_rate.max(0.0)
            } else {
                1.0
            },
            live_pose_blend: if self.live_pose_blend.is_finite() {
                self.live_pose_blend.clamp(0.0, 1.0)
            } else {
                0.0
            },
        }
    }
}

/// What a [`MotionMatcher`] is playing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Playback {
    /// Index into [`MotionDatabase::clips`].
    pub clip: usize,
    /// Clip time, seconds.
    pub time: f32,
    /// The database frame nearest to `time`.
    pub frame: usize,
}

/// Why the latest search did not jump, or that it did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchReason {
    /// The search jumped (or made the first pick).
    Jumped,
    /// The continuation was at least as cheap as the best candidate.
    Continued,
    /// The best candidate beat the continuation but the minimum time between jumps had not
    /// passed.
    Held,
    /// No frame was eligible (the filter, spans, window and lock-out left nothing).
    NoCandidates,
}

impl SearchReason {
    /// The snake_case name (`jumped`, `continued`, `held`, `no_candidates`).
    pub fn name(self) -> &'static str {
        match self {
            SearchReason::Jumped => "jumped",
            SearchReason::Continued => "continued",
            SearchReason::Held => "held",
            SearchReason::NoCandidates => "no_candidates",
        }
    }
}

/// The outcome of the latest search.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchReport {
    /// The cheapest jump candidate (its cost does not include the transition cost).
    pub best: Option<SearchHit>,
    /// The playing frame's cost, or `None` when nothing played or a clip that does not loop
    /// had ended.
    pub continuation_cost: Option<f32>,
    /// True when the search jumped.
    pub jumped: bool,
    /// True when the best candidate beat the continuation but the minimum time between jumps
    /// had not passed.
    pub held: bool,
    /// Why it jumped or not.
    pub reason: SearchReason,
    /// True when the filter left nothing for the first pick and was ignored for it.
    pub filter_ignored: bool,
    /// True when the search ran early because the desired path changed past
    /// [`MatcherSettings::urgent_threshold`].
    pub urgent: bool,
    /// The method that ran.
    pub method_used: SearchMethod,
    /// Tree nodes visited and rows scanned.
    pub work: SearchWork,
}

/// A jump: where playback was and where it went.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Jump {
    /// What played before (`None` for the first pick).
    pub from: Option<Playback>,
    /// What plays now.
    pub to: Playback,
    /// The target's biased cost (without the transition cost).
    pub cost: f32,
}

/// One [`MotionMatcher::update`]'s result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatchStep {
    /// What plays after the update.
    pub playback: Playback,
    /// True when this update ran a search.
    pub searched: bool,
    /// True when playback jumped to a new frame (including the first pick): the host should
    /// switch clips and inertialize.
    pub jumped: bool,
    /// The jump, when `jumped`.
    pub jump: Option<Jump>,
    /// True when a search was due but the per-frame search budget deferred it.
    pub deferred: bool,
}

/// The character's live pose features in the character frame (raw, before normalization), for
/// [`MatcherSettings::live_pose_blend`]. Lists shorter than the schema's keep the playing
/// frame's values for the missing entries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LivePose {
    /// The root velocity, meters per second.
    pub root_velocity: Vec3,
    /// One per schema foot.
    pub feet: Vec<FootFeature>,
    /// One per schema pose joint.
    pub pose_joints: Vec<FootFeature>,
}

/// A ring buffer of the character's actual ground positions and facing, sampled every update,
/// from which the query's past (history) samples are read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroundTrack {
    // (matcher clock, ground position, yaw), oldest first.
    samples: std::collections::VecDeque<(f32, Vec3, f32)>,
    keep: f32,
}

impl GroundTrack {
    /// Samples held.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Re-expresses the recorded ground positions after an origin rebase (spec 78 § 12.2): the
    /// history features are differences of these, so samples left in the old frame would add
    /// the whole cell shift to every history offset until they expire.
    pub(crate) fn shift(&mut self, shift: Vec3) {
        let planar = Vec3::new(shift.x, 0.0, shift.z);
        for (_, position, _) in &mut self.samples {
            *position -= planar;
        }
    }

    fn record(&mut self, clock: f32, position: Vec3, yaw: f32, keep: f32) {
        if !(position.is_finite() && yaw.is_finite()) {
            return;
        }
        if self.samples.back().is_some_and(|&(at, _, _)| at >= clock) {
            self.samples.pop_back();
        }
        self.samples.push_back((clock, position, yaw));
        self.keep = keep;
        while self.samples.get(1).is_some_and(|&(at, _, _)| clock - at >= keep) {
            self.samples.pop_front();
        }
    }

    /// Moves every sample's time down by `shift` seconds (`MotionMatcher::rebase_clock`).
    fn rebase_clock(&mut self, shift: f32) {
        for (at, _, _) in &mut self.samples {
            *at -= shift;
        }
    }

    /// The ground position `behind` seconds before `clock`: interpolated between samples, and
    /// extrapolated backward at the latest velocity before the oldest sample.
    fn position_at(&self, clock: f32, behind: f32) -> Option<Vec3> {
        let &(last_at, last, _) = self.samples.back()?;
        let at = clock - behind;
        let velocity = match self.samples.len() {
            0 | 1 => Vec3::ZERO,
            n => {
                let (prev_at, prev, _) = self.samples[n - 2];
                if last_at - prev_at > TIME_EPSILON {
                    (last - prev) / (last_at - prev_at)
                } else {
                    Vec3::ZERO
                }
            }
        };
        let &(first_at, first, _) = self.samples.front()?;
        if at <= first_at {
            return Some(first - velocity * (first_at - at));
        }
        if at >= last_at {
            return Some(last);
        }
        let index = self.samples.partition_point(|&(t, _, _)| t <= at);
        let (t0, p0, _) = self.samples[index - 1];
        let (t1, p1, _) = self.samples[index];
        let u = if t1 - t0 > TIME_EPSILON {
            (at - t0) / (t1 - t0)
        } else {
            0.0
        };
        Some(p0.lerp(p1, u))
    }
}

/// The runtime half of motion matching: advances the playing clip and searches the database.
///
/// Reflected (spec 52 § 3, ADR-7216): [`MotionMatcher::settings`] and
/// [`MotionMatcher::rate_scale`] are fields an inspector or an agent edits; the filters, the live
/// pose and the runtime state are not reflected.
#[derive(Clone, Debug, Default, PartialEq, Reflect)]
#[reflect(Default)]
pub struct MotionMatcher {
    /// The settings (sanitized at every update).
    pub settings: MatcherSettings,
    /// Which tagged frames a search may choose (everything by default).
    #[reflect(ignore)]
    pub filter: SearchFilter,
    /// A second filter that applies on top of [`MotionMatcher::filter`] while set: a montage
    /// section's scope ([`scope`]). `None` = no scope.
    #[reflect(ignore)]
    pub scope: Option<SearchFilter>,
    /// The host's live pose, blended into the query by
    /// [`MatcherSettings::live_pose_blend`].
    #[reflect(ignore)]
    pub live_pose: Option<LivePose>,
    /// A multiplier on playback time set each frame by a warp stage (stride warping in rate
    /// mode); 1 when no warp runs. Combined with [`MatcherSettings::play_rate`].
    pub rate_scale: f32,
    #[reflect(ignore)]
    playing: Option<Playback>,
    #[reflect(ignore)]
    clock: f32,
    #[reflect(ignore)]
    recent_targets: Vec<(usize, f32)>,
    #[reflect(ignore)]
    since_search: f32,
    #[reflect(ignore)]
    since_jump: f32,
    #[reflect(ignore)]
    searches: u64,
    #[reflect(ignore)]
    jumps: u64,
    #[reflect(ignore)]
    deferrals: u64,
    #[reflect(ignore)]
    deferred_updates: u32,
    #[reflect(ignore)]
    stagger_slot: u8,
    #[reflect(ignore)]
    last: Option<SearchReport>,
    #[reflect(ignore)]
    ground: GroundTrack,
    // The latest search's normalized query and exclusion (for the candidate report).
    #[reflect(ignore)]
    last_query: Vec<f32>,
    // Snapshot of `last_query` while a search fills it in place, so a refused search (a
    // non-finite point) leaves the committed query unchanged.
    #[reflect(ignore)]
    query_backup: Vec<f32>,
    #[reflect(ignore)]
    last_exclude: Exclusion,
    // The desired path at the latest search (for the urgent check).
    #[reflect(ignore)]
    last_trajectory: Vec<TrajectoryPoint>,
    #[reflect(ignore)]
    search_requested: bool,
}

impl MotionMatcher {
    /// A matcher with nothing playing yet; the first update picks a frame.
    pub fn new(settings: MatcherSettings) -> Self {
        Self {
            settings,
            rate_scale: 1.0,
            ..Self::default()
        }
    }

    /// The filter searches use: [`MotionMatcher::filter`] combined with the
    /// [`MotionMatcher::scope`] (a frame must pass both).
    pub fn effective_filter(&self) -> SearchFilter {
        match self.scope {
            Some(scope) => self.filter.and(scope),
            None => self.filter,
        }
    }

    /// What plays, or `None` before the first update.
    pub fn playback(&self) -> Option<Playback> {
        self.playing
    }

    /// Searches run so far.
    pub fn searches(&self) -> u64 {
        self.searches
    }

    /// Jumps so far, including the first pick.
    pub fn jumps(&self) -> u64 {
        self.jumps
    }

    /// Searches the per-frame budget deferred so far.
    pub fn deferrals(&self) -> u64 {
        self.deferrals
    }

    /// Consecutive updates whose due search the budget deferred (0 once it ran).
    pub fn deferred_updates(&self) -> u32 {
        self.deferred_updates
    }

    /// The latest search's outcome.
    pub fn last_search(&self) -> Option<SearchReport> {
        self.last
    }

    /// Set the playing clip's time (distance matching drives time this way instead of `dt`);
    /// wraps a looping clip and clamps one that does not. Returns the new playback; `None` when
    /// nothing plays or the time is not finite.
    pub fn set_playback_time(&mut self, database: &MotionDatabase, time: f32) -> Option<Playback> {
        let playing = self.playing.as_mut()?;
        let range = database.clips.get(playing.clip)?;
        if !time.is_finite() {
            return None;
        }
        let duration = range.safe_duration();
        playing.time = if range.looping {
            time.rem_euclid(duration)
        } else {
            time.clamp(0.0, duration)
        };
        playing.frame = database
            .frame_at(playing.clip, playing.time)
            .unwrap_or(range.start);
        Some(*playing)
    }

    /// Make the next update search, whatever the interval (the agent's `search: "now"`).
    pub fn request_search(&mut self) {
        self.search_requested = true;
    }

    /// The latest search's normalized query (empty before the first search).
    pub fn last_query(&self) -> &[f32] {
        &self.last_query
    }

    /// The stagger slot (`0..8`).
    pub fn stagger_slot(&self) -> u8 {
        self.stagger_slot
    }

    /// Set the stagger slot (taken mod 8); with [`MatcherSettings::stagger`] the first search
    /// interval after the first pick is shortened by `slot / 8` of the interval.
    pub fn set_stagger_slot(&mut self, slot: u32) {
        self.stagger_slot = (slot % 8) as u8;
    }

    /// Record the character's actual ground position (Y ignored) and facing yaw for this
    /// update; the history samples of the next query are read from these. Call before
    /// [`MotionMatcher::update`]. Without any record, the query uses the playing frame's
    /// stored history.
    pub fn record_ground(&mut self, position: Vec3, yaw: f32, database: &MotionDatabase) {
        let keep = database
            .schema
            .history_times
            .iter()
            .fold(0.0f32, |a, &b| a.max(b))
            + 0.5;
        self.ground
            .record(self.clock, Vec3::new(position.x, 0.0, position.z), yaw, keep);
    }

    /// The recorded ground track.
    pub fn ground_track(&self) -> &GroundTrack {
        &self.ground
    }

    /// Moves the recorded ground track with an origin rebase (`shift` is subtracted).
    pub(crate) fn shift_ground_track(&mut self, shift: Vec3) {
        self.ground.shift(shift);
    }

    /// Forget what plays; the next update picks a frame again. The settings, filter, live
    /// pose, stagger slot and counters are kept.
    pub fn reset(&mut self) {
        let keep = Self {
            settings: self.settings,
            filter: self.filter,
            scope: self.scope,
            live_pose: self.live_pose.take(),
            rate_scale: self.rate_scale,
            searches: self.searches,
            jumps: self.jumps,
            deferrals: self.deferrals,
            stagger_slot: self.stagger_slot,
            ..Self::default()
        };
        *self = keep;
    }

    /// The query the matcher would search with now: the playing frame's pose features
    /// (blended with the live pose), the recorded history (when any) and `trajectory`; the
    /// database's mean pose before anything plays.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::FrameOutOfRange`] when the playing frame is not in `database`.
    pub fn current_query(
        &self,
        database: &MotionDatabase,
        trajectory: &[TrajectoryPoint],
    ) -> Result<MotionQuery, MotionMatchError> {
        let Some(playing) = self.playing else {
            return Ok(database.query_with_mean_pose(trajectory));
        };
        let mut query = database.query_from_frame(playing.frame, trajectory)?;
        let blend = self.settings.sanitized().live_pose_blend;
        if blend > 0.0
            && let Some(live) = &self.live_pose
        {
            query.root_velocity = query.root_velocity.lerp(live.root_velocity, blend);
            for (stored, live) in query.feet.iter_mut().zip(&live.feet) {
                stored.position = stored.position.lerp(live.position, blend);
                stored.velocity = stored.velocity.lerp(live.velocity, blend);
            }
            for (stored, live) in query.pose_joints.iter_mut().zip(&live.pose_joints) {
                stored.position = stored.position.lerp(live.position, blend);
                stored.velocity = stored.velocity.lerp(live.velocity, blend);
            }
        }
        if !database.schema.history_times.is_empty()
            && let Some(&(_, now, yaw)) = self.ground.samples.back()
        {
            let to_local = det_rotation_y(-yaw);
            for (point, &behind) in query.history.iter_mut().zip(&database.schema.history_times) {
                if let Some(past) = self.ground.position_at(self.clock, behind) {
                    let offset = to_local * (past - now);
                    *point = Vec2::new(offset.x, offset.z);
                }
            }
        }
        Ok(query)
    }

    /// The Euclidean distance between two desired paths in the normalized trajectory space
    /// (position and direction groups), as the urgent check measures it.
    pub fn trajectory_distance(
        database: &MotionDatabase,
        a: &[TrajectoryPoint],
        b: &[TrajectoryPoint],
    ) -> f32 {
        let positions = database.schema.group_range(FeatureGroup::TrajectoryPosition);
        let directions = database.schema.group_range(FeatureGroup::TrajectoryDirection);
        let mut sum = 0.0f32;
        for (i, (p, q)) in a.iter().zip(b).enumerate() {
            for (k, (x, y)) in [(p.position.x, q.position.x), (p.position.y, q.position.y)]
                .into_iter()
                .enumerate()
            {
                let s = database
                    .scale
                    .get(positions.start + 2 * i + k)
                    .copied()
                    .unwrap_or(0.0);
                let d = (x - y) * s;
                sum += d * d;
            }
            for (k, (x, y)) in [(p.direction.x, q.direction.x), (p.direction.y, q.direction.y)]
                .into_iter()
                .enumerate()
            {
                let s = database
                    .scale
                    .get(directions.start + 2 * i + k)
                    .copied()
                    .unwrap_or(0.0);
                let d = (x - y) * s;
                sum += d * d;
            }
        }
        sum.sqrt()
    }

    /// Keeps the f32 clock in `[CLOCK_REBASE_STEP, CLOCK_REBASE_AT)` (audit-v035-numerics L4):
    /// past that its spacing grows until a step no longer advances it (at 2^24 s), so lockout
    /// and history windows stopped expiring on a long-running server. The clock and every
    /// timestamp kept against it (recent targets, the ground track) move down by the same
    /// multiple of `CLOCK_REBASE_STEP`, which is exact in f32, so every difference the
    /// matcher reads is unchanged. Nothing moves before `CLOCK_REBASE_AT` seconds.
    fn rebase_clock(&mut self) {
        if self.clock < CLOCK_REBASE_AT {
            return;
        }
        let shift = ((self.clock / CLOCK_REBASE_STEP).floor() - 1.0) * CLOCK_REBASE_STEP;
        self.clock -= shift;
        for (_, at) in &mut self.recent_targets {
            *at -= shift;
        }
        self.ground.rebase_clock(shift);
    }

    /// Write the normalized search vector into [`MotionMatcher::last_query`], reusing its buffer.
    ///
    /// The vector matches [`MotionDatabase::normalize`] of [`MotionMatcher::current_query`]:
    /// the playing frame's raw features (or the database mean when nothing plays), with the
    /// desired trajectory, the recorded history and the live-pose blend patched in place. A
    /// refused search leaves the previous committed vector in place: `last_query()` is
    /// documented as the latest *successful* search's query.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::QueryShape`] when `trajectory` has the wrong length,
    /// [`MotionMatchError::FrameOutOfRange`] when the playing frame is not in `database`,
    /// [`MotionMatchError::NonFiniteQuery`] for a non-finite feature.
    fn fill_search_vector(
        &mut self,
        database: &MotionDatabase,
        trajectory: &[TrajectoryPoint],
    ) -> Result<(), MotionMatchError> {
        self.query_backup.clear();
        self.query_backup.extend_from_slice(&self.last_query);
        let result = self.fill_search_vector_in_place(database, trajectory);
        if result.is_err() {
            self.last_query.clear();
            self.last_query.extend_from_slice(&self.query_backup);
        }
        result
    }

    /// The in-place body of [`Self::fill_search_vector`].
    fn fill_search_vector_in_place(
        &mut self,
        database: &MotionDatabase,
        trajectory: &[TrajectoryPoint],
    ) -> Result<(), MotionMatchError> {
        let schema = &database.schema;
        let points = schema.trajectory_times.len();
        if trajectory.len() != points {
            return Err(MotionMatchError::QueryShape {
                part: "trajectory points",
                got: trajectory.len(),
                expected: points,
            });
        }
        let dims = database.dimensions();
        if self.last_query.len() != dims {
            self.last_query.resize(dims, 0.0);
        }
        if let Some(playing) = self.playing {
            let raw = database
                .raw_features(playing.frame)
                .ok_or(MotionMatchError::FrameOutOfRange {
                    frame: playing.frame,
                    frames: database.len(),
                })?;
            self.last_query.copy_from_slice(raw);
        } else if database.mean.len() == dims {
            self.last_query.copy_from_slice(&database.mean);
        } else {
            self.last_query.fill(0.0);
        }
        let positions = schema.group_range(FeatureGroup::TrajectoryPosition);
        let directions = schema.group_range(FeatureGroup::TrajectoryDirection);
        for (i, point) in trajectory.iter().enumerate() {
            let p = positions.start + 2 * i;
            let d = directions.start + 2 * i;
            if d + 2 > directions.end || p + 2 > self.last_query.len() {
                break;
            }
            self.last_query[p..p + 2].copy_from_slice(&point.position.to_array());
            self.last_query[d..d + 2].copy_from_slice(&point.direction.to_array());
        }
        if self.playing.is_some() {
            let blend = self.settings.sanitized().live_pose_blend;
            if blend > 0.0 && self.live_pose.is_some() {
                let root = self.live_pose.as_ref().map(|live| live.root_velocity);
                if let Some(root) = root {
                    let stored = Vec3::new(self.last_query[0], self.last_query[1], self.last_query[2]);
                    self.last_query[0..3].copy_from_slice(&stored.lerp(root, blend).to_array());
                }
                let foot_positions = schema.group_range(FeatureGroup::FootPosition);
                let foot_velocities = schema.group_range(FeatureGroup::FootVelocity);
                let feet = self.live_pose.as_ref().map(|live| live.feet.len()).unwrap_or(0);
                for i in 0..feet {
                    let live = self.live_pose.as_ref().map(|pose| pose.feet[i]);
                    let Some(live) = live else { continue };
                    let p = foot_positions.start + 3 * i;
                    let v = foot_velocities.start + 3 * i;
                    if v + 3 > foot_velocities.end || v + 3 > self.last_query.len() {
                        break;
                    }
                    let position =
                        Vec3::new(self.last_query[p], self.last_query[p + 1], self.last_query[p + 2]);
                    let velocity =
                        Vec3::new(self.last_query[v], self.last_query[v + 1], self.last_query[v + 2]);
                    self.last_query[p..p + 3]
                        .copy_from_slice(&position.lerp(live.position, blend).to_array());
                    self.last_query[v..v + 3]
                        .copy_from_slice(&velocity.lerp(live.velocity, blend).to_array());
                }
                let mut pose_at = schema.group_range(FeatureGroup::PosePosition).start;
                let mut vel_at = schema.group_range(FeatureGroup::PoseVelocity).start;
                let pose_end = schema.group_range(FeatureGroup::PosePosition).end;
                let vel_end = schema.group_range(FeatureGroup::PoseVelocity).end;
                let joints = self
                    .live_pose
                    .as_ref()
                    .map(|live| live.pose_joints.len())
                    .unwrap_or(0);
                for i in 0..joints.min(schema.pose_joints.len()) {
                    let live = self.live_pose.as_ref().map(|pose| pose.pose_joints[i]);
                    let Some(live) = live else { continue };
                    let joint = schema.pose_joints[i];
                    if joint.position {
                        if pose_at + 3 <= pose_end && pose_at + 3 <= self.last_query.len() {
                            let stored = Vec3::new(
                                self.last_query[pose_at],
                                self.last_query[pose_at + 1],
                                self.last_query[pose_at + 2],
                            );
                            self.last_query[pose_at..pose_at + 3]
                                .copy_from_slice(&stored.lerp(live.position, blend).to_array());
                        }
                        pose_at += 3;
                    }
                    if joint.velocity {
                        if vel_at + 3 <= vel_end && vel_at + 3 <= self.last_query.len() {
                            let stored = Vec3::new(
                                self.last_query[vel_at],
                                self.last_query[vel_at + 1],
                                self.last_query[vel_at + 2],
                            );
                            self.last_query[vel_at..vel_at + 3]
                                .copy_from_slice(&stored.lerp(live.velocity, blend).to_array());
                        }
                        vel_at += 3;
                    }
                }
            }
            if !schema.history_times.is_empty()
                && let Some(&(_, now, yaw)) = self.ground.samples.back()
            {
                let to_local = det_rotation_y(-yaw);
                let history = schema.group_range(FeatureGroup::TrajectoryHistory);
                let times = schema.history_times.len();
                let clock = self.clock;
                for start in (0..times).step_by(32) {
                    let end = (start + 32).min(times);
                    let mut batch = [0.0f32; 32];
                    let n = end - start;
                    batch[..n].copy_from_slice(&schema.history_times[start..end]);
                    for (k, &behind) in batch[..n].iter().enumerate() {
                        let Some(past) = self.ground.position_at(clock, behind) else {
                            continue;
                        };
                        let offset = to_local * (past - now);
                        let h = history.start + 2 * (start + k);
                        if h + 2 <= history.end && h + 2 <= self.last_query.len() {
                            self.last_query[h] = offset.x;
                            self.last_query[h + 1] = offset.z;
                        }
                    }
                }
            }
        }
        if self.last_query.iter().any(|v| !v.is_finite()) {
            return Err(MotionMatchError::NonFiniteQuery);
        }
        normalize_in_place(&mut self.last_query, &database.mean, &database.scale);
        Ok(())
    }

    /// Advance playback by `dt` seconds and search when it is time: every
    /// [`MatcherSettings::search_interval`], at once when nothing plays yet, when a clip that
    /// does not loop has reached its end, and early when the path changed past
    /// [`MatcherSettings::urgent_threshold`]. `trajectory` is the desired path, one point per
    /// [`FeatureSchema::trajectory_times`] entry, in the character frame. A non-finite or
    /// negative `dt` counts as zero.
    ///
    /// # Errors
    ///
    /// The errors of [`MotionDatabase::normalize`] (a trajectory of the wrong length or with a
    /// non-finite value). Playback still advanced; nothing jumped.
    pub fn update(
        &mut self,
        database: &MotionDatabase,
        trajectory: &[TrajectoryPoint],
        dt: f32,
    ) -> Result<MatchStep, MotionMatchError> {
        self.update_budgeted(database, trajectory, dt, true)
    }

    /// [`MotionMatcher::update`] with a search permit: when `may_search` is false a due search
    /// is deferred to a later update ([`MatchStep::deferred`]), except the first pick, which
    /// always runs so the character is never left without a clip.
    ///
    /// # Errors
    ///
    /// As [`MotionMatcher::update`].
    #[tracing::instrument(target = "regia_animation", name = "motion_matching.update", skip_all)]
    pub fn update_budgeted(
        &mut self,
        database: &MotionDatabase,
        trajectory: &[TrajectoryPoint],
        dt: f32,
        may_search: bool,
    ) -> Result<MatchStep, MotionMatchError> {
        let settings = self.settings.sanitized();
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let rate = settings.play_rate
            * if self.rate_scale.is_finite() {
                self.rate_scale.max(0.0)
            } else {
                1.0
            };
        if self.playing.is_some_and(|p| p.clip >= database.clips.len()) {
            // The host swapped in a database without this clip: start over.
            self.playing = None;
        }
        let mut ended = false;
        if let Some(playing) = &mut self.playing {
            let range = &database.clips[playing.clip];
            let time = playing.time + dt * rate;
            // The sanitized duration: a looping range of zero (or NaN) length made
            // `rem_euclid` return NaN and the playback time stayed NaN for good
            // (audit-v035-numerics L4). Valid durations are unchanged.
            let duration = range.safe_duration();
            playing.time = if range.looping {
                if duration > TIME_EPSILON {
                    time.rem_euclid(duration)
                } else {
                    0.0
                }
            } else if time >= duration - TIME_EPSILON {
                ended = true;
                duration
            } else {
                time
            };
            playing.frame = database
                .frame_at(playing.clip, playing.time)
                .unwrap_or(range.start);
        }
        self.since_search += dt;
        self.since_jump += dt;
        self.clock += dt;
        self.rebase_clock();
        let (clock, window) = (self.clock, settings.lockout_window);
        self.recent_targets.retain(|&(_, at)| clock - at < window);

        let mut urgent = false;
        let interval_due = self.since_search + TIME_EPSILON >= settings.search_interval;
        if !interval_due
            && let Some(threshold) = settings.urgent_threshold
            && self.playing.is_some()
            && self.since_search + TIME_EPSILON >= settings.min_search_gap
            && self.last_trajectory.len() == trajectory.len()
        {
            urgent = Self::trajectory_distance(database, trajectory, &self.last_trajectory) > threshold;
        }
        let due = self.playing.is_none() || ended || interval_due || urgent || self.search_requested;
        if !due {
            return Ok(self.step(false, None, false));
        }
        if !may_search && self.playing.is_some() {
            self.deferrals += 1;
            self.deferred_updates = self.deferred_updates.saturating_add(1);
            return Ok(self.step(false, None, true));
        }
        self.deferred_updates = 0;
        self.search_requested = false;
        self.fill_search_vector(database, trajectory)?;
        let (exclude, continuation) = match self.playing {
            Some(playing) => {
                let radius = (settings.exclude_window * database.schema.sample_rate).round() as usize;
                let continuation = if ended {
                    None
                } else {
                    database.cost(&self.last_query, playing.frame)
                };
                (database.exclusion_around(playing.frame, radius), continuation)
            }
            None => (Exclusion::NONE, None),
        };
        let first_pick = self.playing.is_none();
        self.since_search = 0.0;
        self.searches += 1;
        let recent = &self.recent_targets;
        let locked = |row: usize| recent.iter().any(|&(frame, _)| frame == row);
        let search = |exclude: Exclusion, filter: SearchFilter| {
            database.search_detailed(
                &self.last_query,
                exclude,
                filter,
                settings.loop_clip_bias,
                locked,
                settings.method,
            )
        };
        let filter = self.effective_filter();
        let mut outcome = search(exclude, filter);
        if outcome.hit.is_none()
            && ended
            && let Some(playing) = self.playing
        {
            // The window covers the whole clip and nothing else exists: any frame but the end
            // one beats staying at the end of a clip that does not loop.
            let only_current = Exclusion::range(playing.frame..playing.frame + 1);
            outcome = search(only_current, filter);
        }
        let mut filter_ignored = false;
        if outcome.hit.is_none() && first_pick && !filter.is_none() {
            // Nothing plays yet and the filter leaves nothing: never leave the character
            // without a clip.
            tracing::warn!(target: "regia_animation", "filter_ignored: the tag filter left no frame for the first pick");
            filter_ignored = true;
            outcome = search(exclude, SearchFilter::NONE);
        }
        let best = outcome.hit;
        let may_jump = first_pick || ended || self.since_jump + TIME_EPSILON >= settings.min_jump_interval;
        let better = match (best, continuation) {
            (Some(hit), Some(stay)) => hit.cost + settings.transition_cost < stay + settings.stay_bias,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let jumped = better && may_jump;
        let mut jump = None;
        if jumped && let Some(hit) = best {
            let frame = database.frames[hit.frame];
            let to = Playback {
                clip: frame.clip,
                time: frame.time,
                frame: hit.frame,
            };
            jump = Some(Jump {
                from: self.playing,
                to,
                cost: hit.cost,
            });
            self.playing = Some(to);
            self.since_jump = 0.0;
            self.jumps += 1;
            if settings.lockout_window > 0.0 {
                self.recent_targets.push((hit.frame, self.clock));
            }
            if first_pick && settings.stagger {
                // Stagger the next search: slot s of 8 searches s/8 of an interval early.
                self.since_search = f32::from(self.stagger_slot) / 8.0 * settings.search_interval;
            }
        }
        let reason = if jumped {
            SearchReason::Jumped
        } else if best.is_none() {
            SearchReason::NoCandidates
        } else if better {
            SearchReason::Held
        } else {
            SearchReason::Continued
        };
        self.last = Some(SearchReport {
            best,
            continuation_cost: continuation,
            jumped,
            held: better && !may_jump,
            reason,
            filter_ignored,
            urgent,
            method_used: outcome.method_used,
            work: outcome.work,
        });
        self.last_exclude = exclude;
        self.last_trajectory.clear();
        self.last_trajectory.extend_from_slice(trajectory);
        Ok(self.step(true, jump, false))
    }

    fn step(&self, searched: bool, jump: Option<Jump>, deferred: bool) -> MatchStep {
        // A built database is never empty, so the first search always picks a frame.
        let playback = self.playing.unwrap_or(Playback {
            clip: 0,
            time: 0.0,
            frame: 0,
        });
        MatchStep {
            playback,
            searched,
            jumped: jump.is_some(),
            jump,
            deferred,
        }
    }
}

/// Motion matching on an entity: a shared database, the runtime matcher and the desired
/// trajectory the game writes each frame. [`step_motion_matching`] (or the
/// [`MotionMatchPlugin`]'s system) advances it; the host plays the returned clip and time, or
/// adds a [`MotionMatchPlayer`] that does it with inertialized jumps.
///
/// Reflected (spec 52 § 3, ADR-7216): the matcher's settings, the trajectory, the ground sample and
/// `report_candidates` read and patch by path; the shared database and the latest results are not
/// reflected (the database is an asset shared by `Arc`).
#[derive(Component, Clone, Debug, Reflect)]
#[reflect(Component, from_reflect = false)]
pub struct MotionMatching {
    /// The feature database (shared between characters).
    #[reflect(ignore)]
    pub database: Arc<MotionDatabase>,
    /// The runtime matcher.
    pub matcher: MotionMatcher,
    /// The desired path, one point per trajectory time, in the character frame.
    pub trajectory: Vec<TrajectoryPoint>,
    /// The character's actual ground position and facing yaw this frame, written by the game
    /// when the schema has history samples (`None` = use the playing frame's stored history).
    pub ground: Option<(Vec3, f32)>,
    /// The last update's result.
    #[reflect(ignore)]
    pub last_step: Option<MatchStep>,
    /// The last update's error, cleared by a successful update.
    #[reflect(ignore)]
    pub last_error: Option<MotionMatchError>,
    /// The database as built, while [`MotionMatching::database`] is a weight overlay of it
    /// ([`MotionMatching::set_weight_overlay`]); `None` without an overlay.
    #[reflect(ignore)]
    pub base_database: Option<Arc<MotionDatabase>>,
    /// Candidates to keep in [`MotionMatching::last_report`] after every search (0, the
    /// default, keeps none: the report costs one extra top-k search).
    pub report_candidates: usize,
    /// The candidate report of the latest search, when [`MotionMatching::report_candidates`]
    /// is above 0.
    #[reflect(ignore)]
    pub last_report: Option<CandidateReport>,
}

impl MotionMatching {
    /// Search with other group weights at runtime (a tuning overlay; the shared database is
    /// not modified): [`MotionMatching::database`] becomes
    /// [`MotionDatabase::with_weight_overlay`] of the built database. `None`, or weights equal
    /// to the build weights, removes the overlay.
    ///
    /// # Errors
    ///
    /// [`MotionMatchError::BadWeight`]; nothing changes on error.
    pub fn set_weight_overlay(&mut self, weights: Option<FeatureWeights>) -> Result<(), MotionMatchError> {
        let base = self
            .base_database
            .clone()
            .unwrap_or_else(|| self.database.clone());
        match weights {
            Some(w) if w != base.schema().weights => {
                let overlay = base.with_weight_overlay(w)?;
                self.database = Arc::new(overlay);
                self.base_database = Some(base);
            }
            _ => {
                self.database = base;
                self.base_database = None;
            }
        }
        Ok(())
    }

    /// A component that starts by standing still (every trajectory point at the origin,
    /// facing forward).
    pub fn new(database: Arc<MotionDatabase>, settings: MatcherSettings) -> Self {
        let points = database.schema().trajectory_times.len();
        Self {
            database,
            matcher: MotionMatcher::new(settings),
            trajectory: vec![TrajectoryPoint::ahead(0.0); points],
            ground: None,
            last_step: None,
            last_error: None,
            base_database: None,
            report_candidates: 0,
            last_report: None,
        }
    }

    /// Advance by `dt` seconds with the current [`MotionMatching::trajectory`].
    ///
    /// # Errors
    ///
    /// The errors of [`MotionMatcher::update`]; also kept in [`MotionMatching::last_error`].
    pub fn step(&mut self, dt: f32) -> Result<MatchStep, MotionMatchError> {
        self.step_budgeted(dt, true)
    }

    /// [`MotionMatching::step`] with a search permit ([`MotionMatcher::update_budgeted`]).
    ///
    /// # Errors
    ///
    /// As [`MotionMatching::step`].
    pub fn step_budgeted(&mut self, dt: f32, may_search: bool) -> Result<MatchStep, MotionMatchError> {
        if dt.is_finite()
            && dt > 0.0
            && let Some((position, yaw)) = self.ground
        {
            self.matcher.record_ground(position, yaw, &self.database);
        }
        let result = self
            .matcher
            .update_budgeted(&self.database, &self.trajectory, dt, may_search);
        match &result {
            Ok(step) => {
                self.last_step = Some(*step);
                self.last_error = None;
                if step.searched && self.report_candidates > 0 {
                    self.last_report = self
                        .matcher
                        .candidate_report(&self.database, self.report_candidates.min(32));
                }
            }
            Err(error) => self.last_error = Some(error.clone()),
        }
        result
    }
}

/// Sent once per jump (including the first pick) by the [`MotionMatchPlugin`]'s system.
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub struct JumpEvent {
    /// The character.
    pub entity: Entity,
    /// What played before (`None` for the first pick).
    pub from: Option<Playback>,
    /// What plays now.
    pub to: Playback,
    /// The target's biased cost.
    pub cost: f32,
    /// True when the target frame belongs to a mirrored range.
    pub mirrored: bool,
}

/// A cap on motion-matching searches per fixed simulation step across every character (0 =
/// unlimited). Characters over the cap defer their search to a later step, the longest-deferred first,
/// ties by entity bits, so the order is deterministic.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MotionMatchBudget {
    /// Most searches per fixed simulation step; 0 = unlimited.
    pub max_searches_per_frame: u32,
}

/// Marker resource: the [`MotionMatchPlugin`] is installed, so [`step_motion_matching`] (if a
/// host also registered it by hand) does nothing and the plugin's system steps the matchers.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct MotionMatchPluginInstalled;

/// Registers motion matching: the [`TrajectoryPredictor`] and montage-scope systems, then
/// [`run_motion_matching`] (stagger slots from the entity index, the [`MotionMatchBudget`],
/// [`JumpEvent`]s), the [`MotionMatchPlayer`] and foot-lock ([`MotionMatchFootLocks`]) systems in
/// `FixedUpdate`, then the animator presentation stage in `Update`. Opt-in:
/// [`crate::AnimationPlugin`] does not add it.
/// Adding it twice, or next to a hand-registered [`step_motion_matching`], is harmless.
#[derive(Clone, Copy, Debug, Default)]
pub struct MotionMatchPlugin;

impl regia_app::Plugin for MotionMatchPlugin {
    fn build(&self, app: &mut regia_app::App) {
        if app.world().contains_resource::<MotionMatchPluginInstalled>() {
            return;
        }
        app.insert_resource(MotionMatchPluginInstalled)
            .init_resource::<MotionMatchBudget>()
            .add_message::<JumpEvent>()
            .add_systems(
                FixedUpdate,
                (
                    (predict::run_trajectory_predictors, scope::run_montage_scopes),
                    run_motion_matching,
                    player::run_motion_match_players,
                    contacts::run_foot_locks,
                )
                    .chain(),
            );
        // Motion databases as assets and the Animator stage (ADR-2409): both act only on
        // entities that carry a `MotionDatabaseRef` or an `AnimatorMotionMatch`.
        app.init_resource::<registry::MotionDatabases>()
            .register_type::<registry::MotionDatabaseRef>()
            .register_type::<regia_asset::AssetRef<registry::MotionDatabaseAsset>>()
            .register_type::<animator::AnimatorMotionMatch>()
            // Spec 52 § 3 (ADR-7216): the component, its settings and the player.
            .register_type::<MotionMatching>()
            .register_type::<MotionMatcher>()
            .register_type::<MatcherSettings>()
            .register_type::<SearchMethod>()
            .register_type::<TrajectoryPoint>()
            .register_type::<player::MotionMatchPlayer>()
            .register_type::<player::PlayerWarps>()
            .register_type::<player::WarpInputs>()
            .register_type::<warp::StrideWarpSettings>()
            .register_type::<warp::StrideMode>()
            .register_type::<warp::OrientationWarpSettings>()
            .add_systems(
                FixedUpdate,
                registry::resolve_motion_database_refs.before(run_motion_matching),
            )
            .add_systems(
                Update,
                animator::drive_animators_from_motion_matching.after(crate::systems::evaluate_animators),
            );
    }
}

/// Advance every [`MotionMatching`] by the frame time. Not registered by
/// [`crate::AnimationPlugin`]: a host that uses motion matching adds it to `Update` before the
/// system that plays the matched clip, or installs the [`MotionMatchPlugin`] instead (then this
/// system does nothing).
#[tracing::instrument(target = "regia_animation", skip_all)]
pub fn step_motion_matching(
    time: Res<regia_app::Time>,
    plugin: Option<Res<MotionMatchPluginInstalled>>,
    mut matchers: Query<&mut MotionMatching, Without<regia_scene::layers::LayerParked>>,
) {
    let _span = tracing::debug_span!("step_motion_matching").entered();
    if plugin.is_some() {
        return;
    }
    let dt = time.delta_secs();
    for mut matching in &mut matchers {
        if let Err(error) = matching.step(dt) {
            tracing::warn!(target: "regia_animation", "motion matching: {error}");
        }
    }
}

/// The [`MotionMatchPlugin`]'s matcher system: assigns each matcher its stagger slot (entity
/// index mod 8), applies the [`MotionMatchBudget`] (longest-deferred first, ties by entity
/// bits) and sends a [`JumpEvent`] per jump.
#[tracing::instrument(target = "regia_animation", skip_all)]
pub fn run_motion_matching(
    time: Res<regia_app::Time>,
    budget: Option<Res<MotionMatchBudget>>,
    mut matchers: Query<(Entity, &mut MotionMatching), Without<regia_scene::layers::LayerParked>>,
    mut jumps: MessageWriter<JumpEvent>,
) {
    let _span = tracing::debug_span!("run_motion_matching").entered();
    let dt = time.delta_secs();
    let cap = budget.map_or(0, |b| b.max_searches_per_frame);
    let mut order: Vec<(u32, u64, Entity)> = matchers
        .iter()
        .map(|(entity, m)| (m.matcher.deferred_updates(), entity.to_bits(), entity))
        .collect();
    order.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut searched = 0u32;
    for (_, _, entity) in order {
        let Ok((_, mut matching)) = matchers.get_mut(entity) else {
            continue;
        };
        matching.matcher.set_stagger_slot(entity.index_u32());
        let may_search = cap == 0 || searched < cap;
        match matching.step_budgeted(dt, may_search) {
            Ok(step) => {
                if step.searched {
                    searched += 1;
                }
                if let Some(jump) = step.jump {
                    let mirrored = matching
                        .database
                        .clips()
                        .get(jump.to.clip)
                        .is_some_and(|c| c.mirrored);
                    jumps.write(JumpEvent {
                        entity,
                        from: jump.from,
                        to: jump.to,
                        cost: jump.cost,
                        mirrored,
                    });
                }
            }
            Err(error) => tracing::warn!(target: "regia_animation", "motion matching: {error}"),
        }
    }
}

#[cfg(test)]
mod tests;
