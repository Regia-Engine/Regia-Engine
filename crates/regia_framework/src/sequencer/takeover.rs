// SPDX-License-Identifier: MPL-2.0
//! The cinematic camera takeover (ADR-1614): a [`SequencePlayer`] with a shot list drives a camera,
//! blending in from the live gameplay pose and back out to it.
//!
//! # Rule
//!
//! - A player *claims* its camera ([`TakeoverSettings::camera`](super::player::TakeoverSettings),
//!   or the camera with the highest `order`) while it is active and its clock is inside the shot
//!   list ([`SequencePlayer::active_shot`]).
//! - Of the players claiming one camera, the most recently started wins. Where no player has an
//!   active shot, the camera falls back to the next claimant, or blends back to gameplay.
//! - The camera gets a [`CameraTakeover`]. Its weight rises by `delta / blend_in` while a player
//!   wins and falls by `delta / blend_out` (the last winner's) when none does, counted in whole
//!   nanoseconds of gameplay time so 30 Hz and 60 Hz runs agree at shared instants; the pose is the
//!   gameplay pose mixed towards the shot pose by smoothstep of the weight: position and field of
//!   view linearly, orientation along the shortest arc. A zero blend time cuts. At weight zero
//!   the component is removed and the camera is gameplay's again.
//! - A legacy [`CameraRig`](crate::camera::CameraRig) keeps updating underneath: each frame the
//!   takeover restores the rig's own pose before [`CameraSet::Update`], so the rig's lag and boom
//!   state never see the cinematic pose and the blend out lands on where gameplay is now. A
//!   camera without a rig keeps the pose it had when the takeover began.
//! - A hard change calls the renderer's camera cut ([`RuntimeRenderer`]): a shot change or loop
//!   wrap while the shots are visible, a switch of winning player, and a zero-time blend in or out.
//! - The pose is written in [`CameraSet::Cinematic`], after the shot player
//!   ([`ShotDriveSet`](crate::shots::ShotDriveSet), so on a camera both drive the sequence wins) and
//!   before [`CameraSet::Director`], so director shakes play on top of the cinematic pose.
//!
//! Depth of field from the shots is written to the camera's `PhysicalCamera` while a shot holds
//! the camera; the gameplay lens settings come back when it lets go. The winning shot's fade goes
//! to the compositor's fade layer through [`CinematicFade`].
//!
//! A spec-35 camera runner (not built yet) takes the same blend as a transient-stack push; until
//! then this component is the one code path.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det_nlerp};
use regia_render::{Camera, CutExposure, DepthOfFieldMode, FadeState, PhysicalCamera, RuntimeRenderer};
use regia_scene::Transform;
use regia_scene::streaming::{OriginOffset, OriginShifted};

use super::player::{SequencePlayer, SequencerSet};
use crate::camera::sensor::ShakeRestoreSet;
use crate::camera::{CameraSet, CinematicCamera};
use crate::shots::ShotSample;
use crate::shots::player::pick_camera;

/// A camera pose: where it is, where it looks and how wide it sees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TakeoverPose {
    /// World position, metres.
    pub translation: Vec3,
    /// World orientation.
    pub rotation: Quat,
    /// Vertical field of view, radians.
    pub fov_y: f32,
}

impl TakeoverPose {
    /// The pose of a shot sample.
    #[must_use]
    pub fn of_sample(sample: &ShotSample) -> Self {
        Self {
            translation: sample.eye,
            rotation: sample.rotation,
            fov_y: sample.fov_y_degrees.to_radians(),
        }
    }

    /// This pose with `translation` moved by `-offset`: from the world meters a sequence is authored
    /// in to local space (`OriginOffset`, spec 78 § 12.2).
    #[must_use]
    pub fn in_local(mut self, offset: Vec3) -> Self {
        self.translation -= offset;
        self
    }

    /// `self` mixed towards `to` by `t` (`0` gives `self`, `1` gives `to`, both exactly):
    /// position and field of view linearly, orientation along the shortest arc.
    #[must_use]
    pub fn mix(self, to: Self, t: f32) -> Self {
        if t.is_nan() || t <= 0.0 {
            return self;
        }
        if t >= 1.0 {
            return to;
        }
        Self {
            translation: self.translation.lerp(to.translation, t),
            // Orientation along the shorter arc by pinned nlerp (add, multiply, one sqrt), not
            // `slerp`'s libm `acos`/`sin`, so the blend is bit-identical across platforms
            // (spec 35 § 12.13).
            rotation: det_nlerp(self.rotation, to.rotation, t),
            fov_y: self.fov_y + (to.fov_y - self.fov_y) * t,
        }
    }
}

/// The eased share of the shot pose at blend progress `weight` (`0..=1`): smoothstep, so the
/// camera leaves the gameplay pose and arrives at the shot with zero velocity.
#[must_use]
pub fn blend_curve(weight: f32) -> f32 {
    let u = if weight.is_finite() {
        weight.clamp(0.0, 1.0)
    } else {
        0.0
    };
    u * u * (3.0 - 2.0 * u)
}

/// `true` for a usable blend time: finite and above zero (anything else cuts).
fn positive(seconds: f32) -> bool {
    seconds.is_finite() && seconds > 0.0
}

/// Blend progress in whole nanoseconds of a span: the weight is `progress / span`. Integer steps
/// make two 60 Hz frames land exactly where one 30 Hz frame does, like the player's clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Blend {
    progress: u64,
    span: u64,
}

impl Blend {
    fn weight(self) -> f32 {
        if self.span == 0 {
            return 0.0;
        }
        (self.progress as f64 / self.span as f64) as f32
    }

    /// Re-expresses the current weight on a span of `seconds` (`span` stays above zero; a
    /// non-positive or non-finite time is a one-nanosecond span that the next step crosses: a cut).
    fn respan(&mut self, seconds: f32) {
        let span = if positive(seconds) {
            super::player::seconds_to_nanos(f64::from(seconds)).max(1)
        } else {
            1
        };
        if span == self.span {
            return;
        }
        let progress = if self.span == 0 {
            0
        } else {
            let scaled = u128::from(self.progress) * u128::from(span);
            u64::try_from((scaled + u128::from(self.span) / 2) / u128::from(self.span)).unwrap_or(span)
        };
        self.span = span;
        self.progress = progress.min(span);
    }

    /// One frame towards `1` over `seconds` (a cut when the time is not positive).
    fn rise(&mut self, delta: u64, seconds: f32) {
        self.respan(seconds);
        self.progress = if positive(seconds) {
            self.progress.saturating_add(delta).min(self.span)
        } else {
            self.span
        };
    }

    /// One frame towards `0` over `seconds` (a cut when the time is not positive).
    fn fall(&mut self, delta: u64, seconds: f32) {
        self.respan(seconds);
        self.progress = if positive(seconds) {
            self.progress.saturating_sub(delta)
        } else {
            0
        };
    }
}

/// The gameplay lens settings a takeover puts back when it lets go.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SavedLens {
    had_lens: bool,
    depth_of_field: Option<DepthOfFieldMode>,
    focal_distance: f32,
    aperture_f_stops: f32,
}

impl SavedLens {
    fn of(lens: Option<&PhysicalCamera>) -> Self {
        match lens {
            Some(lens) => Self {
                had_lens: true,
                depth_of_field: lens.depth_of_field,
                focal_distance: lens.focal_distance,
                aperture_f_stops: lens.aperture_f_stops,
            },
            None => Self {
                had_lens: false,
                depth_of_field: None,
                focal_distance: 0.0,
                aperture_f_stops: 0.0,
            },
        }
    }
}

/// On a camera a sequence holds (added and removed by the takeover; read it, do not insert it).
#[derive(Component, Debug, Clone, Default)]
pub struct CameraTakeover {
    owner: Option<Entity>,
    claimed: bool,
    blend: Blend,
    blend_out: f32,
    gameplay: Option<TakeoverPose>,
    cinematic: Option<TakeoverPose>,
    lens: Option<SavedLens>,
    lens_driven: bool,
    cuts: u64,
}

impl CameraTakeover {
    /// The player whose shots the camera shows (or blends back from).
    #[must_use]
    pub fn owner(&self) -> Option<Entity> {
        self.owner
    }

    /// Blend progress, `0` (gameplay) to `1` (the shots), before the ease.
    #[must_use]
    pub fn weight(&self) -> f32 {
        self.blend.weight()
    }

    /// The share of the shot pose in the camera's pose ([`blend_curve`] of the weight).
    #[must_use]
    pub fn blend(&self) -> f32 {
        blend_curve(self.blend.weight())
    }

    /// The gameplay pose under the takeover this frame (the rig's pose, or the held one).
    #[must_use]
    pub fn gameplay_pose(&self) -> Option<TakeoverPose> {
        self.gameplay
    }

    /// The shot pose the camera blends towards (held during the blend out).
    #[must_use]
    pub fn cinematic_pose(&self) -> Option<TakeoverPose> {
        self.cinematic
    }

    /// Renderer camera cuts this takeover asked for.
    #[must_use]
    pub fn cuts(&self) -> u64 {
        self.cuts
    }
}

/// The fade of the most recently started player that holds a camera; the shot player's fade
/// system draws it when the [`ShotPlayer`](crate::shots::ShotPlayer) resource has nothing loaded.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct CinematicFade(pub FadeState);

impl Default for CinematicFade {
    fn default() -> Self {
        Self(FadeState::NONE)
    }
}

/// The camera a player claims this frame, if any.
fn claimed_camera(
    player: &SequencePlayer,
    live: impl Fn(Entity) -> bool,
    top: Option<Entity>,
) -> Option<Entity> {
    player.active_shot()?;
    player.takeover.camera.filter(|&camera| live(camera)).or(top)
}

/// A camera as the prepare step sees it.
type PreparedCamera = (
    Entity,
    &'static mut Transform,
    &'static mut Camera,
    Option<&'static CameraTakeover>,
    Has<CinematicCamera>,
);

/// A camera as the takeover drives it.
type HeldCamera = (
    Entity,
    &'static mut Transform,
    &'static mut Camera,
    Option<&'static mut PhysicalCamera>,
    Option<&'static mut CameraTakeover>,
);

/// Keeps the poses a takeover saved in the shifted local space: without this a held camera would go
/// back to its old gameplay pose kilometers away (spec 78 § 12.2). A rebase moves roots only: a
/// child camera's pose is relative to its parent and stays.
fn shift_takeover_poses(
    mut shifted: MessageReader<OriginShifted>,
    mut cameras: Query<&mut CameraTakeover, Without<ChildOf>>,
) {
    for message in shifted.read() {
        let shift = Vec3::from_array(message.rebase.shift);
        for mut takeover in &mut cameras {
            if let Some(pose) = &mut takeover.gameplay {
                pose.translation -= shift;
            }
            if let Some(pose) = &mut takeover.cinematic {
                pose.translation -= shift;
            }
        }
    }
}

/// Before the rigs: puts each held camera back on its gameplay pose (so a rig updates from its
/// own state) and marks the cameras a player starts to claim.
#[tracing::instrument(skip_all, target = "regia_framework", name = "sequencer_takeover_prepare")]
fn prepare_takeover(
    players: Query<&SequencePlayer>,
    mut cameras: Query<PreparedCamera>,
    mut commands: Commands,
    camera_order: Option<Res<regia_ecs::spawn_order::SpawnOrder<Camera>>>,
) {
    for (_, mut transform, mut camera, takeover, ..) in &mut cameras {
        if let Some(pose) = takeover.and_then(|t| t.gameplay) {
            transform.translation = pose.translation;
            transform.rotation = pose.rotation;
            camera.fov_y = pose.fov_y;
        }
    }
    if players.is_empty() {
        return;
    }
    let top = pick_camera(
        cameras
            .iter()
            .map(|(entity, _, camera, ..)| (entity, camera.order)),
        camera_order.as_deref(),
    );
    for player in &players {
        let Some(entity) = claimed_camera(player, |e| cameras.contains(e), top) else {
            continue;
        };
        let Ok((_, _, _, takeover, marked)) = cameras.get(entity) else {
            continue;
        };
        if takeover.is_none() {
            commands.entity(entity).try_insert(CameraTakeover::default());
        }
        // Re-marked when the shot player removed the marker from a camera this player holds.
        if !marked {
            commands.entity(entity).try_insert(CinematicCamera);
        }
    }
}

/// One player's claim on a camera this frame.
struct Claim {
    camera: Entity,
    stamp: u64,
    player: Entity,
    sample: ShotSample,
    blend_in: f32,
    blend_out: f32,
    shot_changed: bool,
    rise_by: Option<u64>,
}

fn mode_of(bokeh: bool) -> DepthOfFieldMode {
    if bokeh {
        DepthOfFieldMode::Bokeh
    } else {
        DepthOfFieldMode::Gaussian
    }
}

/// Writes the shot's depth of field while a shot holds the camera, and the saved gameplay lens
/// once it no longer does.
fn drive_lens(
    takeover: &mut CameraTakeover,
    lens: Option<Mut<PhysicalCamera>>,
    dof: Option<crate::shots::DofSample>,
    entity: Entity,
    commands: &mut Commands,
) {
    match (dof, lens) {
        (Some(dof), Some(mut lens)) => {
            lens.depth_of_field = Some(mode_of(dof.bokeh));
            lens.focal_distance = dof.focus_distance;
            lens.aperture_f_stops = dof.aperture;
            takeover.lens_driven = true;
        }
        (Some(dof), None) => {
            commands.entity(entity).try_insert(PhysicalCamera {
                motion_blur_samples: 0,
                depth_of_field: Some(mode_of(dof.bokeh)),
                focal_distance: dof.focus_distance,
                aperture_f_stops: dof.aperture,
                ..PhysicalCamera::default()
            });
            takeover.lens_driven = true;
        }
        (None, lens) => {
            if !takeover.lens_driven {
                return;
            }
            takeover.lens_driven = false;
            let Some(saved) = takeover.lens else {
                return;
            };
            if saved.had_lens {
                if let Some(mut lens) = lens {
                    lens.depth_of_field = saved.depth_of_field;
                    lens.focal_distance = saved.focal_distance;
                    lens.aperture_f_stops = saved.aperture_f_stops;
                }
            } else {
                // The takeover inserted the lens: remove it, or its zero motion-blur samples and
                // empty depth of field override the host's post settings from then on.
                commands.entity(entity).try_remove::<PhysicalCamera>();
            }
        }
    }
}

/// In [`CameraSet::Cinematic`]: picks each held camera's winner, advances the blend, writes the
/// pose, field of view and depth of field, asks for cuts and lets go at weight zero.
#[tracing::instrument(skip_all, target = "regia_framework", name = "sequencer_takeover")]
fn drive_takeover(
    time: Res<Time>,
    (origin, camera_order): (
        Option<Res<OriginOffset>>,
        Option<Res<regia_ecs::spawn_order::SpawnOrder<Camera>>>,
    ),
    mut players: Query<(Entity, &mut SequencePlayer)>,
    mut cameras: Query<HeldCamera>,
    renderer: Option<ResMut<RuntimeRenderer>>,
    mut fade: ResMut<CinematicFade>,
    mut commands: Commands,
) {
    let delta = u64::try_from(time.delta().as_nanos()).unwrap_or(u64::MAX);
    let offset = origin
        .as_deref()
        .map_or(Vec3::ZERO, |o| Vec3::from_array(o.0.map(|axis| axis as f32)));
    let mut order: Vec<Entity> = players.iter().map(|(entity, _)| entity).collect();
    order.sort_unstable_by_key(|entity| entity.to_bits());
    let top = pick_camera(
        cameras
            .iter()
            .map(|(entity, _, camera, ..)| (entity, camera.order)),
        camera_order.as_deref(),
    );
    let mut claims: Vec<Claim> = Vec::new();
    for entity in order {
        let Ok((entity, mut player)) = players.get_mut(entity) else {
            continue;
        };
        player.held_camera = None;
        let shot_changed = player.take_shot_change();
        let Some(camera) = claimed_camera(&player, |e| cameras.contains(e), top) else {
            continue;
        };
        let Some(sample) = player.active_shot().cloned() else {
            continue;
        };
        claims.push(Claim {
            camera,
            stamp: player.start_stamp().unwrap_or(0),
            player: entity,
            sample,
            blend_in: player.takeover.blend_in,
            blend_out: player.takeover.blend_out,
            shot_changed,
            rise_by: player
                .acquire_offset()
                .map(|offset| u64::try_from(offset.as_nanos()).unwrap_or(u64::MAX)),
        });
    }

    let mut cut = false;
    let mut shown_fade: Option<(u64, FadeState)> = None;
    let mut held: Vec<(Entity, Entity, f32)> = Vec::new();
    for (entity, mut transform, mut camera, lens, takeover) in &mut cameras {
        let Some(mut takeover) = takeover else {
            continue;
        };
        // The pose carries no shake here: last frame's shake was taken off before the rigs
        // (`ShakeRestoreSet`, ADR-1619), so a shake running when the takeover begins is not baked
        // into the saved gameplay pose.
        let live = TakeoverPose {
            translation: transform.translation,
            rotation: transform.rotation,
            fov_y: camera.fov_y,
        };
        if takeover.lens.is_none() {
            takeover.lens = Some(SavedLens::of(lens.as_deref()));
        }
        takeover.gameplay = Some(live);
        let previous = takeover.blend.weight();
        let winner = claims
            .iter()
            .filter(|claim| claim.camera == entity)
            .max_by_key(|claim| (claim.stamp, claim.player.to_bits()));
        let hard = match winner {
            Some(claim) => {
                let switched = takeover.owner.is_some_and(|owner| owner != claim.player);
                // Claimed again during a blend out: the shot pose jumps back in.
                let reclaimed = !takeover.claimed;
                takeover.claimed = true;
                takeover.owner = Some(claim.player);
                takeover.blend_out = claim.blend_out;
                takeover.cinematic = Some(TakeoverPose::of_sample(&claim.sample).in_local(offset));
                takeover
                    .blend
                    .rise(claim.rise_by.map_or(delta, |by| by.min(delta)), claim.blend_in);
                let weight = takeover.blend.weight();
                let hard = (previous > 0.0 && (switched || reclaimed || claim.shot_changed))
                    || (previous < 1.0 && weight >= 1.0 && !positive(claim.blend_in));
                if shown_fade.is_none_or(|(stamp, _)| claim.stamp > stamp) {
                    shown_fade = Some((
                        claim.stamp,
                        FadeState {
                            amount: claim.sample.fade,
                            color: claim.sample.fade_color,
                        },
                    ));
                }
                held.push((claim.player, entity, weight));
                hard
            }
            None => {
                takeover.claimed = false;
                // On the frame the owner's shots ended by its clock, hold the shot's final pose
                // and count the blend out from that instant, not from the frame boundary.
                let mut fall_by = delta;
                if let Some(owner) = takeover.owner
                    && let Ok((_, player)) = players.get(owner)
                    && let Some(overshoot) = player.release_overshoot()
                {
                    fall_by = u64::try_from(overshoot.as_nanos()).unwrap_or(u64::MAX).min(delta);
                    if let Some(sample) = player.shot_sample() {
                        takeover.cinematic = Some(TakeoverPose::of_sample(sample).in_local(offset));
                    }
                }
                let blend_out = takeover.blend_out;
                takeover.blend.fall(fall_by, blend_out);
                previous > 0.0 && takeover.blend.weight() <= 0.0 && !positive(blend_out)
            }
        };
        if hard {
            cut = true;
            takeover.cuts += 1;
        }

        let pose = match takeover.cinematic {
            Some(shot) => live.mix(shot, blend_curve(takeover.blend.weight())),
            None => live,
        };
        transform.translation = pose.translation;
        transform.rotation = pose.rotation;
        camera.fov_y = pose.fov_y;

        let dof = winner.and_then(|claim| claim.sample.dof);
        drive_lens(&mut takeover, lens, dof, entity, &mut commands);

        if winner.is_none() && takeover.blend.weight() <= 0.0 {
            commands
                .entity(entity)
                .try_remove::<(CameraTakeover, CinematicCamera)>();
        }
    }
    for (player, camera, weight) in held {
        if let Ok((_, mut player)) = players.get_mut(player) {
            player.held_camera = Some((camera, weight));
        }
    }
    fade.0 = shown_fade.map_or(FadeState::NONE, |(_, state)| state);
    if cut && let Some(mut renderer) = renderer {
        renderer.0.camera_cut_with(CutExposure::Snap);
    }
}

/// `true` while the cinematic fade is not clear, so it is reset even after every player and
/// takeover is gone (a level unload in the middle of a fade to black).
fn fade_showing(fade: Res<CinematicFade>) -> bool {
    fade.0 != FadeState::NONE
}

/// Adds the takeover systems (called by the sequencer plugin's `install`).
pub(crate) fn install(app: &mut App) {
    regia_scene::streaming::add_to_app(app);
    app.init_resource::<CinematicFade>()
        .add_systems(PostUpdate, shift_takeover_poses.before(prepare_takeover))
        .add_systems(
            PostUpdate,
            prepare_takeover
                .run_if(any_with_component::<SequencePlayer>.or_else(any_with_component::<CameraTakeover>))
                .after(SequencerSet::Apply)
                // After the shake is taken off, so the restored gameplay pose is the last word.
                .after(ShakeRestoreSet)
                .before(CameraSet::Update)
                .before(PostUpdateSet::TransformPropagate),
        )
        .add_systems(
            PostUpdate,
            drive_takeover
                .run_if(
                    any_with_component::<SequencePlayer>
                        .or_else(any_with_component::<CameraTakeover>)
                        .or_else(fade_showing),
                )
                .in_set(CameraSet::Cinematic)
                // On a camera both drive, a sequence's shots win over the `render.sequence` shot player.
                .after(crate::shots::ShotDriveSet)
                .after(CameraSet::Update)
                .before(CameraSet::Director)
                .before(PostUpdateSet::TransformPropagate),
        );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::asset::fixtures::{float_track, sequence};
    use super::super::player::{EndPolicy, SequencerPlugin};
    use super::*;
    use crate::camera::{
        CameraDirector, CameraMode, CameraPlugin, CameraRig, CameraRuntime, ChaseCamera, Shake, ThirdPerson,
    };
    use crate::shots::asset::fixtures as shots;
    use crate::shots::asset::{LookAt, PathKey};
    use crate::shots::{ShotPlayer, ShotSequence};
    use regia_core::math::det_quat_from_yaw_pitch_roll;

    const STEP: Duration = Duration::from_nanos(16_666_667);
    const DT: f32 = 1.0 / 60.0;

    /// Pin: the takeover midpoint orients with the pinned nlerp (add, multiply, one square root),
    /// not the platform libm through `slerp`; the endpoints stay exact (spec 35 § 12.13).
    #[test]
    fn mix_pins_the_midpoint_quaternion_and_keeps_the_endpoints_exact() {
        let from = TakeoverPose {
            translation: Vec3::ZERO,
            rotation: det_quat_from_yaw_pitch_roll(0.0, 0.0, 0.0),
            fov_y: 1.0,
        };
        let to = TakeoverPose {
            translation: Vec3::new(2.0, 4.0, -1.0),
            rotation: det_quat_from_yaw_pitch_roll(std::f32::consts::FRAC_PI_2, 0.0, 0.0),
            fov_y: 1.2,
        };
        let mid = from.mix(to, 0.5);
        assert_eq!(mid.rotation.x.to_bits(), 0);
        assert_eq!(mid.rotation.y.to_bits(), 0x3ec3_ef15);
        assert_eq!(mid.rotation.z.to_bits(), 0);
        assert_eq!(mid.rotation.w.to_bits(), 0x3f6c_835e);
        assert_eq!(mid.translation.x, 1.0);
        assert_eq!(mid.translation.y, 2.0);
        assert_eq!(mid.translation.z, -0.5);
        assert!((mid.fov_y - 1.1).abs() < 1.0e-7);
        // Endpoints and out-of-range weights return the inputs unchanged, exactly.
        assert_eq!(from.mix(to, 0.0).rotation, from.rotation);
        assert_eq!(from.mix(to, 1.0).rotation, to.rotation);
        assert_eq!(from.mix(to, f32::NAN).rotation, from.rotation);
        assert_eq!(from.mix(to, -1.0).translation, from.translation);
        assert_eq!(from.mix(to, 2.0).translation, to.translation);
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
        app
    }

    /// A shot list with one static shot of `seconds` at `eye`, looking at the origin.
    fn still(name: &str, eye: [f32; 3], seconds: f32) -> crate::shots::Shot {
        let mut shot = shots::shot(name, seconds);
        shot.camera.path.keys = vec![PathKey {
            time: 0.0,
            position: eye,
        }];
        shot.camera.path.look_at = LookAt::Point([0.0, 0.0, 0.0]);
        shot
    }

    fn stills(list: Vec<crate::shots::Shot>) -> ShotSequence {
        shots::sequence(list)
    }

    fn gameplay_camera(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((
                Transform {
                    translation: Vec3::new(0.0, 0.0, 10.0),
                    ..Transform::default()
                },
                Camera::default(),
            ))
            .id()
    }

    fn spawn_player(
        app: &mut App,
        shots: ShotSequence,
        blend_in: f32,
        blend_out: f32,
        length: f32,
    ) -> Entity {
        let mut player =
            SequencePlayer::new(sequence(vec![float_track("t", "ui", "opacity", length)], vec![]))
                .with_shots(shots);
        player.takeover.blend_in = blend_in;
        player.takeover.blend_out = blend_out;
        player.play();
        app.world_mut().spawn(player).id()
    }

    fn pose(app: &App, camera: Entity) -> Transform {
        *app.world().get::<Transform>(camera).expect("camera")
    }

    /// Spec 78 § 12.2: the shot stays where it was authored, in world meters, and the saved gameplay
    /// pose moves with local space, so letting go does not send the camera back 1 km.
    #[test]
    fn a_rebase_keeps_the_shot_in_the_world_and_the_saved_pose_in_local_space() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        let player = spawn_player(
            &mut app,
            stills(vec![still("a", [0.0, 30.0, 30.0], 30.0)]),
            0.0,
            0.0,
            30.0,
        );
        app.update();
        app.update();
        assert!((pose(&app, camera).translation - Vec3::new(0.0, 30.0, 30.0)).length() < 1.0e-3);

        let shift = [1024.0, 0.0, 0.0];
        app.insert_resource(OriginOffset([1024.0, 0.0, 0.0]));
        app.world_mut().write_message(OriginShifted {
            rebase: regia_scene::floating_origin::Rebase {
                cell_delta: [1, 0, 0],
                shift,
            },
            origin_cell: [1, 0, 0],
        });
        app.world_mut()
            .get_mut::<Transform>(camera)
            .expect("camera")
            .translation
            .x -= shift[0];
        app.update();
        assert!((pose(&app, camera).translation - Vec3::new(-1024.0, 30.0, 30.0)).length() < 1.0e-3);

        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .stop();
        app.update();
        app.update();
        assert!(
            (pose(&app, camera).translation - Vec3::new(-1024.0, 0.0, 10.0)).length() < 1.0e-3,
            "{:?}",
            pose(&app, camera).translation
        );
    }

    /// A rebase moves roots only, so a child camera's saved pose, relative to its parent, stays.
    #[test]
    fn a_rebase_leaves_a_child_cameras_saved_pose_alone() {
        let pose = TakeoverPose {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: Default::default(),
            fov_y: 1.0,
        };
        let held = || CameraTakeover {
            gameplay: Some(pose),
            ..CameraTakeover::default()
        };
        let mut app = App::new();
        app.add_message::<OriginShifted>()
            .add_systems(Update, shift_takeover_poses);
        let root = app.world_mut().spawn(held()).id();
        let child = app.world_mut().spawn((held(), ChildOf(root))).id();
        app.world_mut().write_message(OriginShifted {
            rebase: regia_scene::floating_origin::Rebase {
                cell_delta: [1, 0, 0],
                shift: [1024.0, 0.0, 0.0],
            },
            origin_cell: [1, 0, 0],
        });
        app.update();
        let saved = |entity| {
            app.world()
                .get::<CameraTakeover>(entity)
                .and_then(|t| t.gameplay)
                .expect("saved")
                .translation
        };
        assert_eq!(saved(root).x, 1.0 - 1024.0);
        assert_eq!(saved(child).x, 1.0);
    }

    #[test]
    fn the_blend_curve_and_the_mix_hit_their_ends_exactly() {
        assert_eq!(blend_curve(0.0), 0.0);
        assert_eq!(blend_curve(1.0), 1.0);
        assert_eq!(blend_curve(0.5), 0.5);
        assert_eq!(blend_curve(f32::NAN), 0.0);
        assert_eq!(blend_curve(7.0), 1.0);
        let a = TakeoverPose {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            fov_y: 1.0,
        };
        let b = TakeoverPose {
            translation: Vec3::new(10.0, 0.0, 0.0),
            rotation: Quat::from_rotation_y(1.0),
            fov_y: 0.5,
        };
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        assert_eq!(a.mix(b, f32::NAN), a);
        let half = a.mix(b, 0.5);
        assert!((half.translation.x - 5.0).abs() < 1.0e-5);
        assert!((half.fov_y - 0.75).abs() < 1.0e-6);
        let mut blend = Blend::default();
        assert_eq!(blend.weight(), 0.0);
        blend.rise(100_000_000, 0.0);
        assert_eq!(blend.weight(), 1.0, "zero time snaps in");
        blend.fall(100_000_000, -1.0);
        assert_eq!(blend.weight(), 0.0, "a bad time snaps out");
        blend.rise(250_000_000, 1.0);
        assert_eq!(blend.weight(), 0.25);
        blend.fall(0, 0.5);
        assert_eq!(blend.weight(), 0.25, "a new span keeps the weight");
        blend.fall(100_000_000, 0.5);
        assert!((blend.weight() - 0.05).abs() < 1.0e-6);
        blend.rise(5_000_000_000, 0.5);
        assert_eq!(blend.weight(), 1.0, "clamped");
    }

    #[test]
    fn takeover_blends_in_without_a_jump_and_back_out_exactly() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        app.update();
        let gameplay = pose(&app, camera);
        let player = spawn_player(
            &mut app,
            stills(vec![still("a", [20.0, 5.0, 0.0], 10.0)]),
            1.0,
            0.5,
            1.0,
        );
        let shot = TakeoverPose::of_sample(
            &app.world()
                .get::<SequencePlayer>(player)
                .and_then(|p| p.shot_sample().cloned())
                .expect("shot sample"),
        );
        let distance = shot.translation.distance(gameplay.translation);
        // The fastest a smoothstep blend over 1 s moves per 60 Hz frame (1.5 x the mean speed).
        let max_step = 1.5 * DT * distance + 1.0e-3;
        let mut last = gameplay.translation;
        for frame in 0..90 {
            app.update();
            let now = pose(&app, camera).translation;
            let moved = now.distance(last);
            assert!(
                moved <= max_step,
                "frame {frame}: moved {moved} m (limit {max_step})"
            );
            if frame == 0 {
                assert!(moved < 0.05, "no jump at takeover: {moved} m");
            }
            last = now;
        }
        let held = pose(&app, camera);
        assert_eq!(
            held.translation, shot.translation,
            "fully on the shot after blend_in"
        );
        assert_eq!(held.rotation, shot.rotation);
        let takeover = app.world().get::<CameraTakeover>(camera).expect("held");
        assert_eq!(takeover.owner(), Some(player));
        assert_eq!(takeover.cuts(), 0, "a blend is not a cut");
        assert_eq!(
            app.world()
                .get::<SequencePlayer>(player)
                .expect("player")
                .held_camera(),
            Some((camera, 1.0))
        );

        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .stop();
        for _ in 0..40 {
            app.update();
            let now = pose(&app, camera).translation;
            assert!(now.distance(last) <= 1.5 * 2.0 * DT * distance + 1.0e-3);
            last = now;
        }
        let back = pose(&app, camera);
        assert_eq!(
            back.translation, gameplay.translation,
            "exactly back on the gameplay pose"
        );
        assert_eq!(back.rotation, gameplay.rotation);
        assert!(
            app.world().get::<CameraTakeover>(camera).is_none(),
            "let go at weight zero"
        );
        assert!(app.world().get::<CinematicCamera>(camera).is_none());
    }

    #[test]
    fn zero_blend_times_cut_in_and_out() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        let player = spawn_player(
            &mut app,
            stills(vec![
                still("a", [3.0, 0.0, 0.0], 1.0),
                still("b", [0.0, 3.0, 0.0], 1.0),
            ]),
            0.0,
            0.0,
            2.0,
        );
        app.update();
        app.update();
        let at_a = pose(&app, camera).translation;
        assert_eq!(at_a, Vec3::new(3.0, 0.0, 0.0), "snapped onto the shot");
        let cuts = |app: &App| {
            app.world()
                .get::<CameraTakeover>(camera)
                .map(CameraTakeover::cuts)
        };
        assert_eq!(cuts(&app), Some(1), "the snap in is a cut");
        for _ in 0..60 {
            app.update();
        }
        assert_eq!(pose(&app, camera).translation, Vec3::new(0.0, 3.0, 0.0));
        assert_eq!(cuts(&app), Some(2), "the shot change is a cut");
        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .stop();
        app.update();
        assert_eq!(
            pose(&app, camera).translation,
            Vec3::new(0.0, 0.0, 10.0),
            "snapped out"
        );
    }

    #[test]
    fn the_most_recent_start_wins_and_control_falls_back() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        let first = spawn_player(
            &mut app,
            stills(vec![still("a", [1.0, 0.0, 0.0], 10.0)]),
            0.0,
            0.0,
            10.0,
        );
        app.update();
        app.update();
        assert_eq!(pose(&app, camera).translation, Vec3::new(1.0, 0.0, 0.0));
        // The second player's shots last 0.5 s of its 2 s sequence.
        let second = spawn_player(
            &mut app,
            stills(vec![still("b", [2.0, 0.0, 0.0], 0.5)]),
            0.0,
            0.0,
            2.0,
        );
        app.update();
        assert_eq!(
            pose(&app, camera).translation,
            Vec3::new(2.0, 0.0, 0.0),
            "the later start wins"
        );
        let takeover = app.world().get::<CameraTakeover>(camera).expect("held");
        assert_eq!(takeover.owner(), Some(second));
        let cuts = takeover.cuts();
        for _ in 0..40 {
            app.update();
        }
        assert_eq!(
            pose(&app, camera).translation,
            Vec3::new(1.0, 0.0, 0.0),
            "past its shots the camera falls back to the earlier player"
        );
        let takeover = app.world().get::<CameraTakeover>(camera).expect("held");
        assert_eq!(takeover.owner(), Some(first));
        assert_eq!(takeover.cuts(), cuts + 1, "the switch back is a cut");
        assert_eq!(
            app.world()
                .get::<SequencePlayer>(second)
                .expect("second")
                .held_camera(),
            None
        );
    }

    #[test]
    fn a_named_camera_is_driven_instead_of_the_top_one() {
        let mut app = app();
        let low = gameplay_camera(&mut app);
        let high = app
            .world_mut()
            .spawn((
                Transform::default(),
                Camera {
                    order: 3,
                    ..Camera::default()
                },
            ))
            .id();
        let mut player = SequencePlayer::new(sequence(vec![], vec![])).with_shots(stills(vec![still(
            "a",
            [4.0, 0.0, 0.0],
            1.0,
        )]));
        player.takeover.blend_in = 0.0;
        player.takeover.camera = Some(low);
        player.play();
        app.world_mut().spawn(player);
        app.update();
        app.update();
        assert_eq!(pose(&app, low).translation, Vec3::new(4.0, 0.0, 0.0));
        assert_eq!(
            pose(&app, high).translation,
            Vec3::ZERO,
            "the top camera is untouched"
        );
    }

    #[test]
    fn a_disabled_takeover_or_a_stopped_player_leaves_the_camera_alone() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        let mut player = SequencePlayer::new(sequence(vec![], vec![])).with_shots(stills(vec![still(
            "a",
            [4.0, 0.0, 0.0],
            1.0,
        )]));
        player.takeover.enabled = false;
        player.play();
        app.world_mut().spawn(player);
        app.world_mut().spawn(
            SequencePlayer::new(sequence(vec![], vec![])).with_shots(stills(vec![still(
                "b",
                [5.0, 0.0, 0.0],
                1.0,
            )])),
        );
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(pose(&app, camera).translation, Vec3::new(0.0, 0.0, 10.0));
        assert!(app.world().get::<CameraTakeover>(camera).is_none());
    }

    /// A chase rig following a target that moves along +X at 2 m/s.
    fn rig_app(cutscene: bool) -> (App, Entity) {
        let mut app = app();
        let target = app.world_mut().spawn(Transform::default()).id();
        let camera = app
            .world_mut()
            .spawn((
                Transform::default(),
                Camera::default(),
                CameraRig {
                    mode: CameraMode::ThirdPerson(ThirdPerson {
                        boom_length: 5.0,
                        lag_speed: 4.0,
                        rotation_lag: 6.0,
                        ..ThirdPerson::default()
                    }),
                    ..CameraRig::default()
                },
                ChaseCamera { target },
            ))
            .id();
        app.add_systems(
            Update,
            move |time: Res<Time>, mut q: Query<&mut Transform, Without<CameraRig>>| {
                if let Ok(mut t) = q.get_mut(target) {
                    t.translation.x += 2.0 * time.delta_secs();
                }
            },
        );
        if cutscene {
            let mut player = SequencePlayer::new(sequence(vec![], vec![])).with_shots(stills(vec![still(
                "a",
                [0.0, 30.0, 30.0],
                1.0,
            )]));
            player.takeover.blend_in = 0.25;
            player.takeover.blend_out = 0.25;
            player.end = EndPolicy::Once;
            player.play();
            app.world_mut().spawn(player);
        }
        (app, camera)
    }

    #[test]
    fn a_rig_keeps_running_underneath_so_the_blend_out_lands_on_live_gameplay() {
        let (mut with, camera_with) = rig_app(true);
        let (mut without, camera_without) = rig_app(false);
        let mut differed = false;
        for _ in 0..120 {
            with.update();
            without.update();
            differed |= pose(&with, camera_with).translation != pose(&without, camera_without).translation;
        }
        assert!(differed, "the cutscene was on screen");
        assert!(
            with.world().get::<CameraTakeover>(camera_with).is_none(),
            "released"
        );
        let (a, b) = (pose(&with, camera_with), pose(&without, camera_without));
        assert_eq!(
            a.translation, b.translation,
            "the rig's own state was never disturbed"
        );
        assert_eq!(a.rotation, b.rotation);
    }

    #[test]
    fn a_director_shake_is_visible_during_a_held_shot_and_gone_after() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        spawn_player(
            &mut app,
            stills(vec![still("a", [6.0, 2.0, 0.0], 10.0)]),
            0.0,
            0.0,
            10.0,
        );
        app.update();
        app.update();
        let shot = pose(&app, camera);
        app.world_mut().resource_mut::<CameraDirector>().shake(Shake {
            amplitude: 0.5,
            duration_secs: 0.5,
            ..Shake::default()
        });
        let mut largest = 0.0f32;
        for _ in 0..10 {
            app.update();
            largest = largest.max(pose(&app, camera).translation.distance(shot.translation));
        }
        assert!(largest > 0.01, "the shake shows on the cinematic pose: {largest}");
        for _ in 0..60 {
            app.update();
        }
        let settled = pose(&app, camera);
        assert!(
            settled.translation.distance(shot.translation) < 1.0e-5,
            "no shake left"
        );
        assert!(settled.rotation.angle_between(shot.rotation) < 1.0e-4);
    }

    #[test]
    fn despawning_the_player_blends_the_camera_back() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        let player = spawn_player(
            &mut app,
            stills(vec![still("a", [8.0, 0.0, 0.0], 10.0)]),
            0.0,
            0.25,
            10.0,
        );
        app.update();
        app.update();
        assert_eq!(pose(&app, camera).translation, Vec3::new(8.0, 0.0, 0.0));
        app.world_mut().despawn(player);
        app.update();
        let partway = pose(&app, camera).translation;
        assert!(partway.x > 0.0 && partway.x < 8.0, "blending out: {partway:?}");
        for _ in 0..20 {
            app.update();
        }
        assert_eq!(pose(&app, camera).translation, Vec3::new(0.0, 0.0, 10.0));
        assert!(app.world().get::<CameraTakeover>(camera).is_none());
        assert_eq!(app.world().resource::<CinematicFade>().0, FadeState::NONE);
    }

    #[test]
    fn a_sequence_wins_over_the_shot_player_on_a_shared_camera() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        {
            let mut shot_player = app.world_mut().resource_mut::<ShotPlayer>();
            shot_player.load(stills(vec![still("resource", [0.0, 9.0, 0.0], 10.0)]));
            shot_player.play();
        }
        spawn_player(
            &mut app,
            stills(vec![still("seq", [7.0, 0.0, 0.0], 10.0)]),
            0.0,
            0.0,
            10.0,
        );
        for _ in 0..5 {
            app.update();
            // Every frame after the takeover starts shows the sequence, never the shot player.
            if app
                .world()
                .get::<CameraTakeover>(camera)
                .is_some_and(|t| t.weight() >= 1.0)
            {
                assert_eq!(pose(&app, camera).translation, Vec3::new(7.0, 0.0, 0.0));
            }
        }
        assert_eq!(pose(&app, camera).translation, Vec3::new(7.0, 0.0, 0.0));
    }

    #[test]
    fn shot_depth_of_field_is_driven_while_held_and_the_gameplay_lens_comes_back() {
        let mut app = app();
        let lens = PhysicalCamera {
            depth_of_field: None,
            focal_distance: 3.0,
            aperture_f_stops: 8.0,
            ..PhysicalCamera::default()
        };
        let camera = app
            .world_mut()
            .spawn((Transform::default(), Camera::default(), lens))
            .id();
        let mut shot = still("a", [0.0, 0.0, 5.0], 1.0);
        shot.camera.dof = Some(crate::shots::DepthOfFieldTracks {
            focus_on_target: true,
            ..crate::shots::DepthOfFieldTracks::default()
        });
        let player = spawn_player(&mut app, stills(vec![shot]), 0.0, 0.0, 1.0);
        app.update();
        app.update();
        let held = *app.world().get::<PhysicalCamera>(camera).expect("lens");
        assert_eq!(held.depth_of_field, Some(DepthOfFieldMode::Gaussian));
        assert!(
            (held.focal_distance - 5.0).abs() < 1.0e-4,
            "focus on the look-at point: {}",
            held.focal_distance
        );
        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .stop();
        app.update();
        let back = *app.world().get::<PhysicalCamera>(camera).expect("lens");
        assert_eq!(back, lens, "the gameplay lens settings are restored");
    }

    /// audit-seq (High): a camera with no lens got one for the shot's depth of field and kept
    /// it (zero motion-blur samples, depth of field off) after the takeover let go.
    #[test]
    fn a_lens_the_takeover_inserted_is_removed_when_it_lets_go() {
        let mut app = app();
        let camera = app
            .world_mut()
            .spawn((Transform::default(), Camera::default()))
            .id();
        let mut shot = still("a", [0.0, 0.0, 5.0], 1.0);
        shot.camera.dof = Some(crate::shots::DepthOfFieldTracks {
            focus_on_target: true,
            ..crate::shots::DepthOfFieldTracks::default()
        });
        let player = spawn_player(&mut app, stills(vec![shot]), 0.0, 0.0, 1.0);
        app.update();
        app.update();
        let held = *app
            .world()
            .get::<PhysicalCamera>(camera)
            .expect("inserted for the shot");
        assert_eq!(held.depth_of_field, Some(DepthOfFieldMode::Gaussian));
        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .stop();
        app.update();
        assert!(
            app.world().get::<PhysicalCamera>(camera).is_none(),
            "the inserted lens is removed"
        );
    }

    /// The camera pose at every frame of a takeover that blends in and out, at a fixed step.
    fn takeover_trace(step: Duration, frames: usize, every: usize, delay: f64) -> Vec<[u32; 7]> {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(step));
        let camera = gameplay_camera(&mut app);
        let mut shot = shots::shot("pan", 1.0);
        shot.camera.path.look_at = LookAt::Point([0.0, 0.0, -10.0]);
        let mut player = SequencePlayer::new(sequence(vec![], vec![])).with_shots(stills(vec![shot]));
        player.takeover.blend_in = 0.3;
        player.takeover.blend_out = 0.3;
        player.end = EndPolicy::Once;
        player.start_delay = delay;
        player.play();
        app.world_mut().spawn(player);
        let mut out = Vec::new();
        for frame in 1..=frames {
            app.update();
            if frame % every == 0 {
                let t = pose(&app, camera);
                let r = t.rotation;
                out.push([
                    t.translation.x.to_bits(),
                    t.translation.y.to_bits(),
                    t.translation.z.to_bits(),
                    r.x.to_bits(),
                    r.y.to_bits(),
                    r.z.to_bits(),
                    r.w.to_bits(),
                ]);
            }
        }
        out
    }

    #[test]
    fn a_takeover_replays_identically_at_30_and_60_hz() {
        let thirty = takeover_trace(Duration::from_nanos(33_333_334), 60, 1, 0.0);
        let sixty = takeover_trace(Duration::from_nanos(16_666_667), 120, 2, 0.0);
        assert_eq!(
            thirty, sixty,
            "bit-identical camera poses at every shared instant"
        );
        assert_ne!(thirty[3], thirty[20], "the camera moved");
        // A start delay that ends inside a frame: the blend in starts at that instant.
        let thirty = takeover_trace(Duration::from_nanos(33_333_334), 60, 1, 0.02);
        let sixty = takeover_trace(Duration::from_nanos(16_666_667), 120, 2, 0.02);
        assert_eq!(thirty, sixty, "the same with a start delay");
    }

    #[test]
    fn a_fade_left_on_screen_is_cleared_after_everything_is_gone() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        let mut shot = still("a", [1.0, 0.0, 0.0], 4.0);
        shot.fade_out = 4.0;
        let player = spawn_player(&mut app, stills(vec![shot]), 0.0, 0.0, 4.0);
        for _ in 0..60 {
            app.update();
        }
        assert!(app.world().resource::<CinematicFade>().0.amount > 0.1);
        app.world_mut().despawn(player);
        app.world_mut().despawn(camera);
        app.update();
        app.update();
        assert_eq!(app.world().resource::<CinematicFade>().0, FadeState::NONE);
    }

    #[test]
    fn a_restart_and_a_reclaim_during_the_blend_out_are_cuts() {
        let mut app = app();
        let camera = gameplay_camera(&mut app);
        let player = spawn_player(
            &mut app,
            stills(vec![still("a", [3.0, 0.0, 0.0], 0.5)]),
            0.0,
            1.0,
            0.5,
        );
        for _ in 0..60 {
            app.update();
        }
        let cuts = |app: &App| {
            app.world()
                .get::<CameraTakeover>(camera)
                .map_or(0, CameraTakeover::cuts)
        };
        let before = cuts(&app);
        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .play();
        app.update();
        assert_eq!(
            cuts(&app),
            before + 1,
            "a restart of a held single shot jumps: a cut"
        );
        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .stop();
        app.update();
        app.update();
        let blending = app
            .world()
            .get::<CameraTakeover>(camera)
            .expect("blending out")
            .weight();
        assert!(blending > 0.0 && blending < 1.0, "{blending}");
        let before = cuts(&app);
        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .play();
        app.update();
        assert_eq!(cuts(&app), before + 1, "claimed again mid blend out: a cut");
    }

    #[test]
    fn a_shake_running_when_the_takeover_starts_is_not_baked_into_gameplay() {
        let mut app = app();
        let start = Transform {
            translation: Vec3::new(0.0, 2.0, 10.0),
            ..Transform::default()
        };
        // A camera the director already shakes (a rig, or one a shot player drove before).
        let camera = app
            .world_mut()
            .spawn((
                start,
                Camera::default(),
                CameraRuntime::default(),
                CinematicCamera,
            ))
            .id();
        app.world_mut().resource_mut::<CameraDirector>().shake(Shake {
            amplitude: 0.5,
            duration_secs: 0.3,
            ..Shake::default()
        });
        for _ in 0..5 {
            app.update();
        }
        assert!(
            pose(&app, camera).translation.distance(start.translation) > 1.0e-3,
            "shaking"
        );
        let player = spawn_player(
            &mut app,
            stills(vec![still("a", [5.0, 0.0, 0.0], 10.0)]),
            0.0,
            0.0,
            10.0,
        );
        for _ in 0..30 {
            app.update();
        }
        app.world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player")
            .stop();
        app.update();
        assert!(app.world().get::<CameraTakeover>(camera).is_none(), "released");
        let back = pose(&app, camera);
        assert!(
            back.translation.distance(start.translation) < 1.0e-5,
            "the gameplay pose came back clean: {:?}",
            back.translation
        );
        assert!(back.rotation.angle_between(start.rotation) < 1.0e-4);
    }

    #[test]
    fn the_winning_fade_reaches_the_compositor_state() {
        let mut app = app();
        gameplay_camera(&mut app);
        let mut shot = still("a", [1.0, 0.0, 0.0], 4.0);
        shot.fade_in = 2.0;
        shot.fade_color = [0.1, 0.2, 0.3];
        spawn_player(&mut app, stills(vec![shot]), 0.0, 0.0, 4.0);
        for _ in 0..60 {
            app.update();
        }
        let fade = app.world().resource::<CinematicFade>().0;
        assert!(
            fade.amount > 0.4 && fade.amount < 0.6,
            "half way through a 2 s fade in: {fade:?}"
        );
        assert_eq!(fade.color, [0.1, 0.2, 0.3]);
        assert!(app.world().resource::<ShotPlayer>().sample().is_none());
    }
}
