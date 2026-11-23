// SPDX-License-Identifier: MPL-2.0
//! The motion-matching player (`docs/specs/52-motion-matching.md` § 2.9): samples the matched
//! clip at the matcher's time, smooths every jump with an inertialized transition and reports
//! the frame's root motion.
//!
//! On a jump the player starts its [`Inertializer`] with the pose that would have been shown
//! without the jump as the source and the target clip's pose as the target, so the output on
//! the jump frame equals the source pose and the offset then decays to zero. The blend lasts
//! `lerp(duration_min, duration_max, saturate(cost / cost_ref))`: near-identical poses blend
//! quickly, large jumps longer. A jump within the same clip of at most two database frames uses
//! `duration_min` and the spring decay; any other jump uses the quintic decay.
//!
//! The root joint (when it has no parent) is placed at the character-frame origin in the output
//! pose: its ground translation and yaw are removed, and the frame's movement is reported as a
//! [`RootMotionDelta`] instead, taken within one clip at one time step (wrapping a looping clip
//! into its previous lap), so a jump never adds a spurious delta.

// Source: D. Bollo, "Inertialization: High-Performance Animation Transitions", GDC 2018 talk
// (offset decay with velocity carry, as implemented in `crate::inertialize`); the hand-off from
// a nearest-frame search to the blend follows S. Clavet, "Motion Matching and The Road to
// Next-Gen Animation", GDC 2016. Original code; nothing ported.

use std::sync::Arc;

use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::*;

use super::warp::{
    ArcLengthTable, DistanceMatcher, OrientationWarpSettings, StrideMode, StrideRequest, StrideWarp,
    StrideWarpSettings, apply_orientation_warp, apply_stride_pose_with_globals, orientation_delta,
};
use super::{Extractor, MatchStep, MotionClip, MotionDatabase, MotionMatcher, MotionMatching, Playback};
use crate::clip::AnimationClip;
use crate::foot_ik::FootIkLeg;
use crate::inertialize::{DecayMethod, Inertializer, Switch};
use crate::num::{det_atan2, det_rotation_y, det_sin_cos};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// The root's movement over one frame, in the character frame of the previous sample (`z`
/// forward, `x` right; `y` up).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RootMotionDelta {
    /// Translation, meters.
    pub translation: Vec3,
    /// Turn about +Y, radians (wrapped to `(-pi, pi]`).
    pub yaw: f32,
}

/// The player's opt-in warp stage (`docs/specs/52-motion-matching.md` § 2.10): stride,
/// orientation and distance matching, applied to the matched, inertialized pose. Every part is
/// off by default (strength 0, no distance-matched clips) and never changes search results.
#[derive(Clone, Debug, PartialEq, Reflect)]
#[reflect(Default)]
pub struct PlayerWarps {
    /// Stride warping settings. In [`StrideMode::Rate`] the ratio scales the matcher's playback
    /// rate (from the next frame); in [`StrideMode::Pose`] it moves the feet with
    /// [`PlayerWarps::stride_legs`].
    pub stride: StrideWarpSettings,
    /// Leg chains for the pose side of stride warping.
    #[reflect(ignore)]
    pub stride_legs: Vec<FootIkLeg>,
    /// The hips joint the legs hang from (lowered, never raised, when a leg would overextend).
    pub stride_hips: u32,
    /// Largest foot offset along the movement direction the stride warp scales, meters
    /// (default 0.5).
    pub stride_length: f32,
    /// Orientation warping settings (spine chain and weights inside).
    pub orientation: OrientationWarpSettings,
    /// The joint that turns the lower body (usually the pelvis or the root).
    pub orientation_pelvis: u32,
    /// Database clips (by index) that are distance matched: stops, pivots, landings (besides the
    /// clips the database itself flags, [`super::ClipRange::distance_matched`]).
    pub distance_clips: Vec<usize>,
    /// Ground speed below which a frame counts as stopped (default 0.05 m/s).
    pub stop_speed_threshold: f32,
    /// The arc-length table ends at the last frame instead of the first stopped one.
    pub stop_at_end: bool,
}

impl Default for PlayerWarps {
    fn default() -> Self {
        Self {
            stride: StrideWarpSettings::default(),
            stride_legs: Vec::new(),
            stride_hips: 0,
            stride_length: 0.5,
            orientation: OrientationWarpSettings::default(),
            orientation_pelvis: 0,
            distance_clips: Vec::new(),
            stop_speed_threshold: 0.05,
            stop_at_end: false,
        }
    }
}

/// What the game tells the warp stage each frame (all optional; `None` leaves that warp idle).
#[derive(Clone, Copy, Debug, Default, PartialEq, Reflect)]
#[reflect(Default, PartialEq)]
pub struct WarpInputs {
    /// The character's actual ground speed, meters per second (stride warping).
    pub actual_speed: Option<f32>,
    /// The desired movement heading in the character frame, radians (0 = forward, positive
    /// toward +X) (orientation warping).
    pub desired_heading: Option<f32>,
    /// Remaining ground distance to the stop or landing point, meters (distance matching).
    pub distance_to_target: Option<f32>,
}

/// Plays what a [`MotionMatching`] on the same entity picks, with inertialized jumps. Opt-in:
/// add it next to the [`MotionMatching`] and install the [`super::MotionMatchPlugin`]. The
/// output is [`MotionMatchPlayer::pose`] (local joint transforms) and
/// [`MotionMatchPlayer::root_motion`]; the host applies them (for example by writing the pose
/// into its skeleton instance and moving the character controller by the delta).
///
/// Reflected (spec 52 § 3, ADR-7216): the blend range, `strip_root`, the warp settings (without
/// the leg chains) and the warp inputs read and patch by path; the clips and the skeleton (assets
/// shared by `Arc`) and the playback state are not reflected.
#[derive(Component, Clone, Debug, Reflect)]
#[reflect(Component, from_reflect = false)]
pub struct MotionMatchPlayer {
    /// The clips given to the database build, in entry order ([`super::ClipRange::source`];
    /// the database's clip order when it has no mirrored entries).
    #[reflect(ignore)]
    pub clips: Arc<[AnimationClip]>,
    /// The skeleton the clips animate.
    #[reflect(ignore)]
    pub skeleton: Arc<Skeleton>,
    /// Shortest blend, seconds (default 0.1).
    pub duration_min: f32,
    /// Longest blend, seconds (default 0.25).
    pub duration_max: f32,
    /// The jump cost that maps to `duration_max`; `None` = the database's median jump cost
    /// ([`MotionDatabase::median_jump_cost`]).
    pub duration_cost_ref: Option<f32>,
    /// Remove the root's ground translation and yaw from the output pose (default true; only
    /// applies when the schema's root joint has no parent).
    pub strip_root: bool,
    #[reflect(ignore)]
    bind: Pose,
    #[reflect(ignore)]
    pose: Pose,
    #[reflect(ignore)]
    previous: Option<Pose>,
    #[reflect(ignore)]
    inertializer: Inertializer,
    #[reflect(ignore)]
    root_motion: RootMotionDelta,
    #[reflect(ignore)]
    pending_root_motion: RootMotionDelta,
    #[reflect(ignore)]
    jumps_seen: u64,
    #[reflect(ignore)]
    last_blend: Option<(f32, DecayMethod)>,
    /// The warp stage's settings.
    pub warps: PlayerWarps,
    /// The warp stage's inputs for this frame.
    pub warp_inputs: WarpInputs,
    #[reflect(ignore)]
    stride: StrideWarp,
    #[reflect(ignore)]
    distance: DistanceMatcher,
    #[reflect(ignore)]
    distance_clip: Option<usize>,
    #[reflect(ignore)]
    arc_tables: Vec<Option<ArcLengthTable>>,
    #[reflect(ignore)]
    last_orientation_delta: f32,
    /// Sample target reused by [`MotionMatchPlayer::advance`].
    #[reflect(ignore)]
    sample_buf: Pose,
    /// Third pose buffer, used on the first advance so later frames rotate three poses.
    #[reflect(ignore)]
    pose_spare: Pose,
    /// Jump-frame source and previous-target poses, reused after the first jump.
    #[reflect(ignore)]
    jump_source: Pose,
    #[reflect(ignore)]
    jump_prev: Pose,
    /// Unstripped clip sample used to read the root joint.
    #[reflect(ignore)]
    root_pose: Pose,
    /// Root joint matrices reused across frames (index 0 is the latest root matrix
    /// when the root has no parent).
    #[reflect(ignore)]
    root_globals: Vec<Mat4>,
    /// Mirror partners, copied only when the database's list changes.
    #[reflect(ignore)]
    root_partners: Vec<u32>,
    /// Object-space matrices reused by stride warps and stored foot positions.
    #[reflect(ignore)]
    globals_scratch: Vec<Mat4>,
    /// Object-space foot positions of the latest output pose, schema foot order.
    #[reflect(ignore)]
    foot_positions: Vec<Vec3>,
}

impl MotionMatchPlayer {
    /// A player over `clips` (database clip order) on `skeleton`, with the default blend range.
    pub fn new(clips: Arc<[AnimationClip]>, skeleton: Arc<Skeleton>) -> Self {
        let pose = skeleton.bind_pose_pose();
        let sample_buf = pose.clone();
        let pose_spare = pose.clone();
        let jump_source = pose.clone();
        let jump_prev = pose.clone();
        let root_pose = pose.clone();
        Self {
            clips,
            skeleton,
            duration_min: 0.1,
            duration_max: 0.25,
            duration_cost_ref: None,
            strip_root: true,
            bind: pose.clone(),
            pose,
            previous: None,
            inertializer: Inertializer::default(),
            root_motion: RootMotionDelta::default(),
            pending_root_motion: RootMotionDelta::default(),
            jumps_seen: 0,
            last_blend: None,
            warps: PlayerWarps::default(),
            warp_inputs: WarpInputs::default(),
            stride: StrideWarp::default(),
            distance: DistanceMatcher::default(),
            distance_clip: None,
            arc_tables: Vec::new(),
            last_orientation_delta: 0.0,
            sample_buf,
            pose_spare,
            jump_source,
            jump_prev,
            root_pose,
            root_globals: Vec::new(),
            root_partners: Vec::new(),
            globals_scratch: Vec::new(),
            foot_positions: Vec::new(),
        }
    }

    /// Capacities of the pose scratch and the root-matrix scratch `advance` reuses.
    ///
    /// Both stay flat after the second step when the player keeps its buffers.
    #[cfg(test)]
    fn scratch_caps(&self) -> (usize, usize) {
        (
            self.sample_buf.translations.capacity(),
            self.root_globals.capacity(),
        )
    }

    /// The smoothed stride ratio (1 when the stride warp is off).
    pub fn stride_ratio(&self) -> f32 {
        self.stride.ratio()
    }

    /// The playback-rate multiplier the stride warp asks of the matcher (1 unless
    /// [`StrideMode::Rate`]).
    pub fn rate_scale(&self) -> f32 {
        self.stride.rate_multiplier(self.warps.stride.mode)
    }

    /// The orientation warp's turn of the latest advance, radians.
    pub fn orientation_delta(&self) -> f32 {
        self.last_orientation_delta
    }

    /// The arc-length table of a distance-matched clip, built on first use: from the database's
    /// stored arrays when it has them ([`MotionDatabase::cooked_arc_lengths`], ADR-7817), else
    /// by sampling the root path at the database frames (the same table bit for bit). `None`
    /// for a clip that is not distance matched.
    pub fn arc_table(&mut self, database: &MotionDatabase, clip: usize) -> Option<&ArcLengthTable> {
        let flagged = database.clips().get(clip).is_some_and(|c| c.distance_matched);
        if !flagged && !self.warps.distance_clips.contains(&clip) {
            return None;
        }
        if self.arc_tables.len() < database.clips().len() {
            self.arc_tables.resize(database.clips().len(), None);
        }
        if self.arc_tables.get(clip)?.is_none() {
            let range = database.clips().get(clip)?;
            let times: Vec<f32> = range
                .range()
                .filter_map(|f| database.frame(f).map(|r| r.time))
                .collect();
            let (threshold, stop_at_end) = (self.warps.stop_speed_threshold, self.warps.stop_at_end);
            self.arc_tables[clip] = match database.cooked_arc_lengths(clip) {
                Some(arrays) => ArcLengthTable::from_arrays(arrays, &times, threshold, stop_at_end),
                None => {
                    let source = MotionClip {
                        clip: self.clips.get(range.source)?,
                        looping: range.looping,
                    };
                    let mut extractor = Extractor::new(&self.skeleton, database.schema());
                    extractor.partners = database.mirror_partners().to_vec();
                    extractor.mirrored = range.mirrored;
                    let positions: Vec<Vec3> =
                        times.iter().map(|&t| extractor.sample(source, t).root).collect();
                    ArcLengthTable::from_root_positions(&positions, &times, threshold, stop_at_end)
                }
            };
        }
        self.arc_tables.get(clip)?.as_ref()
    }

    /// Distance matching: when `playback`'s clip is distance matched and the game supplied
    /// [`WarpInputs::distance_to_target`], the clip time whose remaining path equals it (held
    /// while the distance grows); `None` otherwise. The plugin's system writes it back into the
    /// matcher before [`MotionMatchPlayer::advance`].
    pub fn distance_matched_time(&mut self, database: &MotionDatabase, playback: Playback) -> Option<f32> {
        let distance = self.warp_inputs.distance_to_target?;
        if self.distance_clip != Some(playback.clip) {
            self.distance.reset();
            self.distance_clip = Some(playback.clip);
        }
        // Build the table first (that call borrows self), then take it out so the
        // distance matcher can borrow it without cloning the sample list.
        self.arc_table(database, playback.clip)?;
        let table = self.arc_tables.get_mut(playback.clip)?.take()?;
        let time = self.distance.update(&table, distance);
        self.arc_tables[playback.clip] = Some(table);
        Some(time)
    }

    /// The stride and orientation stages on `pose` (the matched, inertialized pose).
    fn apply_warps(&mut self, database: &MotionDatabase, playback: Playback, dt: f32, pose: &mut Pose) {
        let velocity = database
            .raw_features(playback.frame)
            .map_or(Vec3::ZERO, |raw| Vec3::new(raw[0], raw[1], raw[2]));
        // The cooked clip-speed array when present; the same value from `velocity` otherwise.
        let clip_speed = database.clip_speed(playback.frame).unwrap_or(0.0);
        let clip_heading = if clip_speed > 1e-6 {
            det_atan2(velocity.x, velocity.z)
        } else {
            0.0
        };
        let settings = self.warps.stride;
        let actual = self.warp_inputs.actual_speed.unwrap_or(clip_speed);
        let _ = self.stride.update(&settings, actual, clip_speed, dt);
        if settings.mode == StrideMode::Pose && !self.warps.stride_legs.is_empty() {
            let (sin, cos) = det_sin_cos(clip_heading);
            let direction = Vec3::new(sin, 0.0, cos);
            apply_stride_pose_with_globals(
                &self.skeleton,
                pose,
                &self.warps.stride_legs,
                StrideRequest {
                    hips: self.warps.stride_hips,
                    movement_dir: direction,
                    ratio: self.stride.stride_ratio(StrideMode::Pose),
                    stride_length: self.warps.stride_length,
                },
                &mut self.globals_scratch,
            );
        }
        self.last_orientation_delta = match self.warp_inputs.desired_heading {
            Some(desired) => orientation_delta(&self.warps.orientation, desired, clip_heading),
            None => 0.0,
        };
        if self.last_orientation_delta != 0.0 {
            apply_orientation_warp(
                &self.skeleton,
                pose,
                self.warps.orientation_pelvis,
                &self.warps.orientation,
                self.last_orientation_delta,
            );
        }
    }

    /// The output pose of the latest [`MotionMatchPlayer::advance`].
    pub fn pose(&self) -> &Pose {
        &self.pose
    }

    /// The root movement of the latest advance.
    pub fn root_motion(&self) -> RootMotionDelta {
        self.root_motion
    }

    /// Drain all fixed-substep motion since the presentation stage's previous call.
    pub(crate) fn take_pending_root_motion(&mut self) -> RootMotionDelta {
        std::mem::take(&mut self.pending_root_motion)
    }

    /// True while a jump's blend is running.
    pub fn is_blending(&self) -> bool {
        self.inertializer.is_active()
    }

    /// Jumps the player has blended (including the first pick, which shows the target at once).
    pub fn jumps_seen(&self) -> u64 {
        self.jumps_seen
    }

    /// The latest jump's blend duration and decay.
    pub fn last_blend(&self) -> Option<(f32, DecayMethod)> {
        self.last_blend
    }

    /// The blend duration for a jump of cost `cost` (see the module docs).
    pub fn blend_duration(&self, cost: f32, cost_ref: f32) -> f32 {
        let lo = finite_or(self.duration_min, 0.1).max(0.0);
        let hi = finite_or(self.duration_max, 0.25).max(lo);
        let t = if cost_ref > 0.0 && cost.is_finite() {
            (cost / cost_ref).clamp(0.0, 1.0)
        } else {
            1.0
        };
        lo + (hi - lo) * t
    }

    /// Sample `playback` into `out` (bind pose underneath, root stripped when configured).
    fn sample(&self, database: &MotionDatabase, playback: Playback, out: &mut Pose) {
        out.clone_from(&self.bind);
        let range = database.clips().get(playback.clip);
        let source = range.map_or(playback.clip, |r| r.source);
        if let Some(clip) = self.clips.get(source) {
            clip.sample_pose(playback.time, out);
        }
        if range.is_some_and(|r| r.mirrored) {
            super::mirror_pose(out, database.mirror_partners());
        }
        let root = database.schema().root_joint as usize;
        let parentless = self.skeleton.parents.get(root).is_some_and(Option::is_none);
        if self.strip_root && parentless && root < out.len() {
            let rotation = out.rotations[root];
            let facing = rotation * database.schema().forward;
            let yaw = if facing.x * facing.x + facing.z * facing.z > 1e-12 {
                det_atan2(facing.x, facing.z)
            } else {
                0.0
            };
            out.rotations[root] = (det_rotation_y(-yaw) * rotation).normalize();
            out.translations[root].x = 0.0;
            out.translations[root].z = 0.0;
        }
    }

    /// Advance by one frame of `dt` seconds after `matcher` produced `step` (the step's
    /// playback and jump drive the output).
    #[tracing::instrument(target = "regia_animation", name = "motion_matching.player", skip_all)]
    pub fn advance(&mut self, database: &MotionDatabase, matcher: &MotionMatcher, step: &MatchStep, dt: f32) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let settings = matcher.settings.sanitized();
        let rate_scale = if matcher.rate_scale.is_finite() {
            matcher.rate_scale.max(0.0)
        } else {
            1.0
        };
        let played = dt * settings.play_rate * rate_scale;
        let playback = step.playback;
        let mut target = std::mem::take(&mut self.sample_buf);
        self.sample(database, playback, &mut target);

        let switch = step
            .jump
            .and_then(|jump| jump.from.map(|from| (jump, from)))
            .filter(|_| self.previous.is_some());
        if let Some((jump, from)) = switch {
            self.jumps_seen += 1;
            // The pose that would have been shown without the jump.
            let mut source = std::mem::take(&mut self.jump_source);
            self.sample(database, from, &mut source);
            self.inertializer.advance(dt);
            self.inertializer.apply(&mut source);
            let mut target_prev = std::mem::take(&mut self.jump_prev);
            let back = Playback {
                time: back_time(database, playback, played),
                ..playback
            };
            self.sample(database, back, &mut target_prev);
            let step_frames = 2.0 / database.schema().sample_rate;
            let small_skip =
                from.clip == playback.clip && (from.time - playback.time).abs() <= step_frames + 1e-5;
            let (duration, method) = if small_skip {
                (self.blend_duration(0.0, 1.0), DecayMethod::Spring)
            } else {
                let cost_ref = self
                    .duration_cost_ref
                    .filter(|c| c.is_finite() && *c > 0.0)
                    .unwrap_or_else(|| database.median_jump_cost());
                (self.blend_duration(jump.cost, cost_ref), DecayMethod::Quintic)
            };
            let previous = self.previous.take();
            self.inertializer.start(
                method,
                duration,
                Switch {
                    source: &source,
                    source_prev: previous.as_ref(),
                    target: &target,
                    target_prev: Some(&target_prev),
                    dt,
                },
            );
            self.last_blend = Some((duration, method));
            self.jump_source = source;
            self.jump_prev = target_prev;
            if let Some(old) = previous {
                self.pose_spare = old;
            }
        } else if step.jump.is_some() {
            // The first pick (or a jump before any output): show the target at once.
            self.jumps_seen += 1;
            self.inertializer.stop();
        } else {
            self.inertializer.advance(dt);
        }
        self.inertializer.apply(&mut target);
        self.apply_warps(database, playback, dt, &mut target);
        self.root_motion = self.root_delta(database, playback, played);
        // Each delta is in the previous sample's character frame. Compose them in fixed-tick
        // order so the presentation stage can drain the whole interval exactly once.
        self.pending_root_motion.translation +=
            det_rotation_y(self.pending_root_motion.yaw) * self.root_motion.translation;
        self.pending_root_motion.yaw = wrap_angle(self.pending_root_motion.yaw + self.root_motion.yaw);
        // The last frame's output carries the source velocity into the next jump.
        // Rotate pose / previous / sample_buf so the next frame samples into a kept buffer.
        let output = std::mem::replace(&mut self.pose, target);
        let older = self.previous.replace(output);
        self.sample_buf = match older {
            Some(pose) => pose,
            None => std::mem::take(&mut self.pose_spare),
        };
        self.store_output_feet(database);
    }

    /// Root movement over the last `played` seconds, read from the root joint only.
    ///
    /// A parentless root's local matrix is its object-space matrix, so the delta does not
    /// build a full-skeleton transform list. Looping clips still apply the lap transform
    /// `C^k` with `C = G_root(duration) · G_root(0)⁻¹`.
    fn root_delta(&mut self, database: &MotionDatabase, playback: Playback, played: f32) -> RootMotionDelta {
        let Some(range) = database.clips().get(playback.clip) else {
            return RootMotionDelta::default();
        };
        let Some(clip) = self.clips.get(range.source) else {
            return RootMotionDelta::default();
        };
        if played <= 0.0 || !(clip.duration.is_finite() && clip.duration > 0.0) {
            return RootMotionDelta::default();
        }
        let (now_root, now_yaw) = self.root_sample(database, playback.clip, playback.time);
        let earlier = if range.looping {
            playback.time - played
        } else {
            (playback.time - played).max(0.0)
        };
        let (before_root, before_yaw) = self.root_sample(database, playback.clip, earlier);
        let delta = RootMotionDelta {
            translation: det_rotation_y(-before_yaw) * (now_root - before_root),
            yaw: wrap_angle(now_yaw - before_yaw),
        };
        if delta.translation.is_finite() && delta.yaw.is_finite() {
            delta
        } else {
            RootMotionDelta::default()
        }
    }

    /// Object-space root position and yaw at an unwrapped clip time.
    fn root_sample(&mut self, database: &MotionDatabase, clip_index: usize, time: f32) -> (Vec3, f32) {
        let (looping, mirrored, duration, root, forward, source) = {
            let Some(range) = database.clips().get(clip_index) else {
                return (Vec3::ZERO, 0.0);
            };
            let Some(clip) = self.clips.get(range.source) else {
                return (Vec3::ZERO, 0.0);
            };
            let schema = database.schema();
            let joints = self.skeleton.len();
            let out_of_range = std::iter::once(schema.root_joint)
                .chain(schema.feet.iter().copied())
                .chain(schema.pose_joints.iter().map(|joint| joint.joint))
                .any(|joint| joint as usize >= joints);
            if out_of_range {
                return (Vec3::ZERO, 0.0);
            }
            let duration = if clip.duration.is_finite() {
                clip.duration.max(0.0)
            } else {
                0.0
            };
            (
                range.looping,
                range.mirrored,
                duration,
                schema.root_joint as usize,
                schema.forward,
                range.source,
            )
        };
        if mirrored {
            let partners = database.mirror_partners();
            if self.root_partners.as_slice() != partners {
                self.root_partners.clear();
                self.root_partners.extend_from_slice(partners);
            }
        }
        let (local, lap) = if looping {
            let laps = if duration > 0.0 {
                (time / duration).floor()
            } else {
                0.0
            };
            ((time - laps * duration).clamp(0.0, duration), laps as i32)
        } else {
            (time.clamp(0.0, duration), 0)
        };
        let mut matrix = self.root_matrix_at(source, local, mirrored, root);
        if lap != 0 {
            let end = self.root_matrix_at(source, duration, mirrored, root);
            let start = self.root_matrix_at(source, 0.0, mirrored, root);
            matrix = super::mat_pow(end * start.inverse(), lap) * matrix;
        }
        let facing = matrix.transform_vector3(forward);
        let yaw = if facing.x * facing.x + facing.z * facing.z > 1e-12 {
            det_atan2(facing.x, facing.z)
        } else {
            0.0
        };
        (matrix.w_axis.truncate(), yaw)
    }

    /// Object-space matrix of `root` at a clip-local time, reusing the root sample buffers.
    fn root_matrix_at(&mut self, source: usize, time: f32, mirrored: bool, root: usize) -> Mat4 {
        let MotionMatchPlayer {
            clips,
            bind,
            root_pose,
            root_partners,
            skeleton,
            root_globals,
            ..
        } = self;
        root_pose.clone_from(bind);
        if let Some(clip) = clips.get(source) {
            clip.sample_pose(time, root_pose);
        }
        if mirrored && !root_partners.is_empty() {
            super::mirror_pose(root_pose, root_partners);
        }
        compose_joint(skeleton, root_pose, root, root_globals)
    }

    /// Object-space foot positions of the latest output pose, in schema foot order.
    ///
    /// Empty before the first [`MotionMatchPlayer::advance`].
    #[must_use]
    pub fn output_feet(&self) -> &[Vec3] {
        &self.foot_positions
    }

    /// Remember each schema foot's object-space position on the output pose.
    fn store_output_feet(&mut self, database: &MotionDatabase) {
        let count = database.schema().feet.len();
        if self.foot_positions.len() != count {
            self.foot_positions.resize(count, Vec3::NAN);
        }
        if count == 0 {
            return;
        }
        compose_all(&self.skeleton, &self.pose, &mut self.globals_scratch);
        for (index, &joint) in database.schema().feet.iter().enumerate() {
            self.foot_positions[index] = self
                .globals_scratch
                .get(joint as usize)
                .map(|matrix| matrix.w_axis.truncate())
                .unwrap_or(Vec3::NAN);
        }
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// The clip time `played` seconds before `playback` (wrapping a looping clip, clamping one that
/// does not).
fn back_time(database: &MotionDatabase, playback: Playback, played: f32) -> f32 {
    let Some(range) = database.clips().get(playback.clip) else {
        return playback.time;
    };
    let time = playback.time - played;
    if range.looping {
        time.rem_euclid(range.duration)
    } else {
        time.max(0.0)
    }
}

/// Wrap an angle to `(-pi, pi]`.
fn wrap_angle(angle: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let wrapped = (angle + PI).rem_euclid(TAU) - PI;
    if wrapped <= -PI { wrapped + TAU } else { wrapped }
}

/// Object-space matrix of one joint. A parentless joint's local matrix is the result, so a
/// root with no parent does not walk the rest of the skeleton. The buffer keeps its capacity.
fn compose_joint(skeleton: &Skeleton, pose: &Pose, joint: usize, globals: &mut Vec<Mat4>) -> Mat4 {
    let n = skeleton.len().min(pose.len());
    if joint >= n {
        return Mat4::IDENTITY;
    }
    let local_of = |index: usize| {
        Mat4::from_scale_rotation_translation(
            pose.scales[index],
            pose.rotations[index],
            pose.translations[index],
        )
    };
    let parent_of = |index: usize| match skeleton.parents.get(index).copied().flatten() {
        Some(parent) if (parent as usize) < index => Some(parent as usize),
        _ => None,
    };
    if parent_of(joint).is_none() {
        let matrix = local_of(joint);
        if globals.is_empty() {
            globals.push(matrix);
        } else {
            globals[0] = matrix;
        }
        return matrix;
    }
    if globals.len() < skeleton.len() {
        globals.resize(skeleton.len(), Mat4::IDENTITY);
    }
    let mut chain = [0usize; 64];
    let mut depth = 0usize;
    let mut cursor = joint;
    let mut deep = false;
    loop {
        if depth == chain.len() {
            deep = true;
            break;
        }
        chain[depth] = cursor;
        depth += 1;
        match parent_of(cursor) {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    if deep {
        let last = joint.min(n - 1);
        for index in 0..=last {
            let local = local_of(index);
            globals[index] = match parent_of(index) {
                Some(parent) => globals[parent] * local,
                None => local,
            };
        }
        return globals[joint];
    }
    let mut parent = Mat4::IDENTITY;
    for slot in (0..depth).rev() {
        let index = chain[slot];
        let global = parent * local_of(index);
        globals[index] = global;
        parent = global;
    }
    globals[joint]
}

/// Full forward kinematics into `globals`, matching [`Skeleton::globals`] with no base.
fn compose_all(skeleton: &Skeleton, pose: &Pose, globals: &mut Vec<Mat4>) {
    let n = skeleton.len();
    if globals.len() != n {
        globals.resize(n, Mat4::IDENTITY);
    }
    let count = n.min(pose.len());
    for index in 0..count {
        let local = Mat4::from_scale_rotation_translation(
            pose.scales[index],
            pose.rotations[index],
            pose.translations[index],
        );
        globals[index] = match skeleton.parents.get(index).copied().flatten() {
            Some(parent) if (parent as usize) < index => globals[parent as usize] * local,
            _ => local,
        };
    }
}

/// The [`super::MotionMatchPlugin`]'s player system: advances every [`MotionMatchPlayer`] from
/// its entity's [`MotionMatching`] step of this frame, writes a distance-matched time back into
/// the matcher, and hands the stride warp's rate to the matcher for the next frame. A parked
/// rig (`LayerParked`) is skipped like its matcher: replaying the matcher's last step every tick
/// piled its root motion up and released it in one frame on unpark.
#[tracing::instrument(target = "regia_animation", skip_all)]
pub fn run_motion_match_players(
    time: Res<regia_app::Time>,
    mut players: Query<
        (&mut MotionMatching, &mut MotionMatchPlayer),
        Without<regia_scene::layers::LayerParked>,
    >,
) {
    let dt = time.delta_secs();
    for (mut matching, mut player) in &mut players {
        step_player(&mut matching, &mut player, dt);
    }
}

/// One frame of a player after its matcher stepped (what [`run_motion_match_players`] does per
/// entity; public so hosts and tests can drive it without the plugin).
pub fn step_player(matching: &mut MotionMatching, player: &mut MotionMatchPlayer, dt: f32) {
    let Some(mut step) = matching.last_step else {
        return;
    };
    let database = matching.database.clone();
    if let Some(time) = player.distance_matched_time(&database, step.playback)
        && let Some(playback) = matching.matcher.set_playback_time(&database, time)
    {
        step.playback = playback;
    }
    player.advance(&database, &matching.matcher, &step, dt);
    matching.matcher.rate_scale = player.rate_scale();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit (corr-animation): the player system had no `LayerParked` filter, so a parked rig
    /// replayed its matcher's last step every fixed tick and piled up root motion that the
    /// unparked stage then released in one frame.
    #[test]
    fn a_parked_player_does_not_advance() {
        use std::sync::Arc;
        use std::time::Duration;

        use regia_core::math::{Quat, Vec3};

        use super::super::{FeatureSchema, MatchStep, MatcherSettings, MotionClip, MotionDatabase, Playback};
        use crate::clip::{AnimationClip, JointTrack, TrackCurve};
        use crate::pose::translated;
        use crate::skeleton::Skeleton;

        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "foot_l".into(), "foot_r".into()],
            vec![None, Some(0), Some(0)],
            vec![
                translated(Vec3::ZERO),
                translated(Vec3::new(-0.1, 0.0, 0.0)),
                translated(Vec3::new(0.1, 0.0, 0.0)),
            ],
        )
        .expect("skeleton");
        let times: Vec<f32> = (0..=60).map(|k| k as f32 / 60.0).collect();
        let mut clip = AnimationClip::empty("walk", 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: times.iter().map(|&t| Vec3::new(0.0, 0.0, 1.5 * t)).collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let clips: Arc<[AnimationClip]> = Arc::from(vec![clip]);
        let db = MotionDatabase::build(
            &skeleton,
            &[MotionClip {
                clip: &clips[0],
                looping: true,
            }],
            FeatureSchema::new(0, vec![1, 2]),
        )
        .expect("database");
        let frame = db.frame_at(0, 0.5).expect("frame");
        let mut matching = MotionMatching::new(Arc::new(db), MatcherSettings::default());
        matching.last_step = Some(MatchStep {
            playback: Playback {
                clip: 0,
                time: 0.5,
                frame,
            },
            searched: false,
            jumped: false,
            jump: None,
            deferred: false,
        });
        let player = MotionMatchPlayer::new(clips, Arc::new(skeleton));

        let mut app = regia_app::App::new();
        app.init_resource::<regia_app::Time>();
        app.add_systems(regia_app::Update, run_motion_match_players);
        let rig = app
            .world_mut()
            .spawn((
                matching,
                player,
                regia_scene::layers::LayerParked {
                    restore: regia_scene::components::Visibility::Inherited,
                    inherited: false,
                },
            ))
            .id();
        let tick = |app: &mut regia_app::App| {
            app.world_mut()
                .resource_mut::<regia_app::Time>()
                .advance(Duration::from_secs_f64(1.0 / 60.0));
            app.update();
            app.world()
                .get::<MotionMatchPlayer>(rig)
                .map(MotionMatchPlayer::root_motion)
                .unwrap_or_default()
        };
        for _ in 0..10 {
            let parked = tick(&mut app);
            assert_eq!(parked.translation, Vec3::ZERO, "a parked player advanced");
        }
        // The control: unparked, the same rig does move.
        app.world_mut()
            .entity_mut(rig)
            .remove::<regia_scene::layers::LayerParked>();
        let live = tick(&mut app);
        assert!(live.translation.length() > 0.0, "{live:?}");
    }

    #[test]
    fn advance_reuses_pose_and_root_scratch() {
        use std::sync::Arc;

        use regia_core::math::{Quat, Vec3};

        use super::super::{
            Extractor, FeatureSchema, MatchStep, MatcherSettings, MotionClip, MotionDatabase, MotionMatcher,
            Playback,
        };
        use crate::clip::{AnimationClip, JointTrack, TrackCurve};
        use crate::pose::translated;
        use crate::skeleton::Skeleton;

        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "foot_l".into(), "foot_r".into()],
            vec![None, Some(0), Some(0)],
            vec![
                translated(Vec3::ZERO),
                translated(Vec3::new(-0.1, 0.0, 0.0)),
                translated(Vec3::new(0.1, 0.0, 0.0)),
            ],
        )
        .expect("skeleton");
        let duration = 1.0;
        let times: Vec<f32> = (0..=60).map(|k| k as f32 / 60.0).collect();
        let mut clip = AnimationClip::empty("walk", duration);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: times.iter().map(|&t| Vec3::new(0.0, 0.0, 1.5 * t)).collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        for (joint, x) in [(1u32, -0.1f32), (2, 0.1)] {
            clip.tracks.push(JointTrack {
                joint,
                translations: TrackCurve::Static(Vec3::new(x, 0.0, 0.0)),
                rotations: TrackCurve::Static(Quat::IDENTITY),
                scales: TrackCurve::Static(Vec3::ONE),
            });
        }
        let clips: Arc<[AnimationClip]> = Arc::from(vec![clip]);
        let db = MotionDatabase::build(
            &skeleton,
            &[MotionClip {
                clip: &clips[0],
                looping: false,
            }],
            FeatureSchema::new(0, vec![1, 2]),
        )
        .expect("database");
        let matcher = MotionMatcher::new(MatcherSettings::default());
        let mut player = MotionMatchPlayer::new(clips.clone(), Arc::new(skeleton));
        let dt = 1.0 / 60.0;
        let mut time = 0.0f32;
        let mut caps_at_step_2 = None;
        for step_index in 0..64 {
            time = (time + dt).min(duration);
            let frame = db.frame_at(0, time).expect("frame");
            let step = MatchStep {
                playback: Playback { clip: 0, time, frame },
                searched: false,
                jumped: false,
                jump: None,
                deferred: false,
            };
            player.advance(&db, &matcher, &step, dt);
            if step_index == 1 {
                let caps = player.scratch_caps();
                assert!(
                    caps.0 >= 3,
                    "pose scratch must hold the skeleton after step 2, capacity {}",
                    caps.0
                );
                assert!(
                    caps.1 >= 1,
                    "root globals scratch must be allocated after step 2, capacity {}",
                    caps.1
                );
                caps_at_step_2 = Some(caps);
            }
        }
        assert_eq!(
            player.scratch_caps(),
            caps_at_step_2.expect("step 2"),
            "scratch capacities stay flat from step 2 through step 64"
        );

        let mut extractor = Extractor::new(&player.skeleton, db.schema());
        let source = MotionClip {
            clip: &clips[0],
            looping: false,
        };
        let now = extractor.sample(source, time);
        let before = extractor.sample(source, time - dt);
        let expected = det_rotation_y(-before.yaw) * (now.root - before.root);
        let actual = player.root_motion();
        assert_eq!(
            actual.translation.to_array().map(f32::to_bits),
            expected.to_array().map(f32::to_bits),
            "root translation matches the full-skeleton sample"
        );
        assert_eq!(actual.yaw.to_bits(), wrap_angle(now.yaw - before.yaw).to_bits());
        let pose = player.pose();
        assert_eq!(pose.translations[0].x.to_bits(), 0.0f32.to_bits());
        assert_eq!(pose.translations[0].z.to_bits(), 0.0f32.to_bits());
        assert!(pose.translations[1].abs_diff_eq(Vec3::new(-0.1, 0.0, 0.0), 1e-5));
        assert!(pose.translations[2].abs_diff_eq(Vec3::new(0.1, 0.0, 0.0), 1e-5));
    }

    #[test]
    fn angles_wrap_into_the_half_open_range() {
        use std::f32::consts::PI;
        assert!((wrap_angle(3.0 * PI) - PI).abs() < 1e-5);
        assert!((wrap_angle(-PI) - PI).abs() < 1e-5);
        assert!((wrap_angle(0.5) - 0.5).abs() < 1e-6);
        assert!((wrap_angle(-0.5 - 2.0 * PI) + 0.5).abs() < 1e-5);
    }

    #[test]
    fn foot_lock_targets_match_globals_of_the_player_pose() {
        use std::sync::Arc;

        use regia_core::math::{Quat, Vec3};

        use super::super::{
            ContactSettings, FeatureSchema, MatchStep, MatcherSettings, MotionClip, MotionDatabase,
            MotionMatcher, Playback,
        };
        use crate::clip::{AnimationClip, JointTrack, TrackCurve};
        use crate::pose::translated;
        use crate::skeleton::Skeleton;

        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "foot_l".into(), "foot_r".into()],
            vec![None, Some(0), Some(0)],
            vec![
                translated(Vec3::ZERO),
                translated(Vec3::new(-0.1, 0.0, 0.0)),
                translated(Vec3::new(0.1, 0.0, 0.0)),
            ],
        )
        .expect("skeleton");
        let clip = {
            let mut clip = AnimationClip::empty("walk", 1.0);
            let times: Vec<f32> = (0..=60).map(|k| k as f32 / 60.0).collect();
            clip.tracks.push(JointTrack {
                joint: 0,
                translations: TrackCurve::Animated {
                    times: times.clone(),
                    values: times.iter().map(|&t| Vec3::new(0.0, 0.0, 1.5 * t)).collect(),
                    exact_rotation: false,
                },
                rotations: TrackCurve::Static(Quat::IDENTITY),
                scales: TrackCurve::Static(Vec3::ONE),
            });
            for (joint, side) in [(1u32, 1.0f32), (2, -1.0)] {
                clip.tracks.push(JointTrack {
                    joint,
                    translations: TrackCurve::Animated {
                        times: times.clone(),
                        values: times
                            .iter()
                            .map(|&t| {
                                let lift = (t * std::f32::consts::TAU).sin().max(0.0);
                                Vec3::new(-0.1 * side, 0.05 * lift, 0.2 * (t * std::f32::consts::TAU).sin())
                            })
                            .collect(),
                        exact_rotation: false,
                    },
                    rotations: TrackCurve::Static(Quat::IDENTITY),
                    scales: TrackCurve::Static(Vec3::ONE),
                });
            }
            clip
        };
        let clips: Arc<[AnimationClip]> = Arc::from(vec![clip]);
        let mut schema = FeatureSchema::new(0, vec![1, 2]);
        schema.contacts = Some(ContactSettings::default());
        let db = MotionDatabase::build(
            &skeleton,
            &[MotionClip {
                clip: &clips[0],
                looping: true,
            }],
            schema,
        )
        .expect("database");
        let matcher = MotionMatcher::new(MatcherSettings::default());
        let mut player = MotionMatchPlayer::new(Arc::clone(&clips), Arc::new(skeleton));
        let dt = 1.0 / 60.0;
        let mut caps = None;
        for step_index in 0..8 {
            let time = dt * (step_index + 1) as f32;
            let frame = db.frame_at(0, time).expect("frame");
            let step = MatchStep {
                playback: Playback { clip: 0, time, frame },
                searched: false,
                jumped: false,
                jump: None,
                deferred: false,
            };
            player.advance(&db, &matcher, &step, dt);
            let feet = super::super::player_feet(&db, &player);
            assert_eq!(player.output_feet(), feet.as_slice());
            let mut bits = Vec::new();
            super::super::contacts::fill_contact_bits(&db, frame, &mut bits);
            assert_eq!(Some(bits), db.contacts(frame));
            if step_index == 1 {
                assert!(player.globals_scratch.capacity() >= player.skeleton.len());
                assert!(player.foot_positions.capacity() >= 2);
                caps = Some((
                    player.globals_scratch.capacity(),
                    player.foot_positions.capacity(),
                ));
            }
        }
        assert_eq!(
            (
                player.globals_scratch.capacity(),
                player.foot_positions.capacity()
            ),
            caps.expect("step 2")
        );
        let contacts = db.contacts(0).expect("contacts");
        assert_eq!(contacts.len(), 2);
    }
}
