// SPDX-License-Identifier: MPL-2.0
//! Playing a [`ShotSequence`]: the [`ShotPlayer`] resource holds the clock and the loaded
//! sequence, and [`ShotPlugin`]'s system moves the camera to the sampled pose each frame.
//!
//! The clock advances by the engine's gameplay [`Time`] delta, so it follows pause, time scale
//! and the fixed steps of `play.step` and nothing else: there is no wall-clock read. Seeking and
//! sampling never depend on how the time got there.
//!
//! A shot change (including a seek into another shot, a loop wrap and the first frame after a
//! load) is a cut: the player counts it and asks the renderer to drop its temporal history
//! ([`regia_render::FrameRenderer::camera_cut`]) so no frame blends the old shot into the new.
//!
//! The fade (`ShotSample::fade`, `fade_color`) is drawn by the compositor: the first time a sample
//! has a fade, [`ShotPlugin`]'s fade system installs the `regia_render` fade layer into
//! [`WindowOverlay`] and keeps it set to the sampled fade every frame
//! (window, screenshot with `window_layers`, and render-out draw it; ADR-1522). With no
//! `WindowOverlay` resource nothing is drawn and the fade stays data.

use regia_app::prelude::*;
use regia_ecs::spawn_order::SpawnOrder;
use regia_render::{
    Camera, CutExposure, DepthOfFieldMode, FadeHandle, FadeState, PhysicalCamera, RuntimeRenderer,
    WindowOverlay, install_fade,
};
use regia_scene::Transform;

use super::asset::ShotSequence;
use super::sample::ShotSample;
use crate::camera::{CameraSet, CinematicCamera};

/// The set holding the system that writes the sampled shot pose to the camera. Order work that
/// must see the shot pose `.after(ShotDriveSet)`. It runs in [`CameraSet::Cinematic`], after
/// [`CameraSet::Update`] and before [`CameraSet::Director`], so the camera plugin's shakes are
/// added on top of the shot (ADR-1614, ADR-1619).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShotDriveSet;

/// The loaded shot sequence and its playback state. Insert it with [`ShotPlugin`].
///
/// # Example
///
/// ```
/// use regia_framework::shots::{ShotPlayer, ShotSequence};
///
/// let sequence = ShotSequence::from_ron(r#"(version: 1, shots: [
///     (name: "a", duration: 2.0, camera: (path: (interp: Linear,
///         keys: [(time: 0.0, position: (0.0, 0.0, 0.0)), (time: 2.0, position: (4.0, 0.0, 0.0))],
///         look_at: Point((0.0, 0.0, -5.0))))),
/// ])"#).expect("valid");
/// let mut player = ShotPlayer::default();
/// player.load(sequence);
/// player.play();
/// player.advance(1.0);
/// assert_eq!(player.sample().map(|s| s.eye.x), Some(2.0));
/// ```
#[derive(Resource, Debug, Default)]
pub struct ShotPlayer {
    sequence: Option<ShotSequence>,
    time: f64,
    playing: bool,
    looping: bool,
    camera: Option<Entity>,
    driven: Option<Entity>,
    /// The camera driven last, kept through `clear` so the release frame lets go of that camera
    /// (not whichever camera is on top by then); dropped by `set_camera` and `load`.
    kept: Option<Entity>,
    sample: Option<ShotSample>,
    last_shot: Option<usize>,
    cuts: u64,
    pending_cut: bool,
    dirty: bool,
    dof_applied: bool,
    /// The player inserted the camera's `PhysicalCamera` (the camera had none): it is removed
    /// again when the depth of field ends, so the leftover lens does not override the host's
    /// post settings (motion blur off, depth of field off) for the rest of the session.
    lens_inserted: bool,
    /// Counts `load` and `clear` (not `replace_sequence`): a new list in the player, so an editor
    /// document that named the old list's file no longer applies (ADR-7495).
    loads: u64,
}

impl ShotPlayer {
    /// Loads a sequence, stopped at time zero. Replaces any loaded one. The first frame of the
    /// new sequence is a cut.
    pub fn load(&mut self, sequence: ShotSequence) {
        self.loads += 1;
        self.sequence = Some(sequence);
        self.time = 0.0;
        self.playing = false;
        self.last_shot = None;
        self.kept = None;
        self.refresh(0);
    }

    /// Swaps in an edited version of the loaded sequence, keeping the clock and the play state.
    /// A seek past the new end clamps to it. Not a cut unless the active shot changed.
    pub fn replace_sequence(&mut self, sequence: ShotSequence) {
        self.time = self.time.min(sequence.duration());
        self.sequence = Some(sequence);
        self.refresh(0);
    }

    /// Unloads the sequence and stops. The camera keeps its last pose.
    pub fn clear(&mut self) {
        *self = Self {
            camera: self.camera,
            kept: self.driven.or(self.kept),
            dof_applied: self.dof_applied,
            lens_inserted: self.lens_inserted,
            // One more drive frame lets go of the camera (lens, director marker).
            dirty: true,
            loads: self.loads + 1,
            ..Self::default()
        };
    }

    /// The loaded sequence.
    #[must_use]
    pub fn sequence(&self) -> Option<&ShotSequence> {
        self.sequence.as_ref()
    }

    /// The sample at the current time (`None` with no sequence loaded).
    #[must_use]
    pub fn sample(&self) -> Option<&ShotSample> {
        self.sample.as_ref()
    }

    /// The playback clock, seconds into the sequence.
    #[must_use]
    pub fn time(&self) -> f64 {
        self.time
    }

    /// `true` while the clock is running.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// `true` when the clock sits at the end of the sequence.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.sequence.as_ref().is_some_and(|s| self.time >= s.duration())
    }

    /// `true` when playback wraps to the start at the end.
    #[must_use]
    pub fn is_looping(&self) -> bool {
        self.looping
    }

    /// Wrap to the start at the end (each wrap is a cut) instead of stopping.
    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    /// The camera entity the sequence drives; `None` picks the camera with the highest `order`
    /// each frame the player drives (ADR-5416).
    #[must_use]
    pub fn camera(&self) -> Option<Entity> {
        self.camera
    }

    /// Chooses the camera entity to drive (`None` = the camera with the highest `order`).
    pub fn set_camera(&mut self, camera: Option<Entity>) {
        self.camera = camera;
        self.kept = None;
        self.dirty = true;
    }

    /// The camera entity the player last wrote a pose to (`None` before the first drive and after
    /// [`clear`](Self::clear)). While set, that camera feels shakes as if it had a default
    /// [`ShakeSensor`](crate::camera::sensor::ShakeSensor), unless it carries its own.
    #[must_use]
    pub fn driven_camera(&self) -> Option<Entity> {
        self.driven
    }

    /// Cuts counted since the player was created or cleared.
    #[must_use]
    pub fn cuts(&self) -> u64 {
        self.cuts
    }

    /// How many times a list was loaded or cleared: [`load`](Self::load) and
    /// [`clear`](Self::clear) count, [`replace_sequence`](Self::replace_sequence) (an edit of the
    /// loaded list) does not. A tool that keeps a file path for the loaded list compares this
    /// to know the list is still the one it opened (ADR-7495).
    #[must_use]
    pub fn load_count(&self) -> u64 {
        self.loads
    }

    /// Starts or resumes the clock; from the end of a non-looping sequence it restarts at zero.
    pub fn play(&mut self) {
        if self.sequence.is_none() {
            return;
        }
        if self.is_finished() && !self.looping {
            self.time = 0.0;
            self.refresh(0);
        }
        self.playing = true;
    }

    /// Stops the clock where it is.
    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Stops the clock and returns to time zero.
    pub fn stop(&mut self) {
        self.playing = false;
        self.seek(0.0);
    }

    /// Moves the clock to `time` seconds (clamped to the sequence). Works while playing or paused.
    pub fn seek(&mut self, time: f64) {
        let Some(sequence) = &self.sequence else { return };
        self.time = if time.is_finite() {
            time.clamp(0.0, sequence.duration())
        } else {
            0.0
        };
        self.refresh(0);
    }

    /// Moves the clock to frame `frame` of the sequence's authoring frame rate.
    pub fn seek_frame(&mut self, frame: u64) {
        let Some(sequence) = &self.sequence else { return };
        let time = super::sample::time_of_frame(frame, sequence.fps);
        self.seek(time);
    }

    /// Advances the clock by `dt` seconds if it is running. Non-finite or negative steps are
    /// ignored. Reaching the end stops the clock on the last frame, or wraps when looping.
    pub fn advance(&mut self, dt: f64) {
        if !self.playing || !dt.is_finite() || dt < 0.0 {
            return;
        }
        let Some(sequence) = &self.sequence else { return };
        let total = sequence.duration();
        self.time += dt;
        let mut wraps = 0u32;
        if self.time >= total {
            if self.looping && total > 0.0 {
                // Every crossed loop is a cut; the count is capped like the sequencer's marker replay.
                wraps = ((self.time / total).floor() as u32).clamp(1, MAX_LOOP_CUTS);
                self.time %= total;
            } else {
                self.time = total;
                self.playing = false;
            }
        }
        self.refresh(wraps);
    }

    /// Re-samples at the current time and records a cut when the active shot changed, and one per
    /// crossed loop when the clock wrapped.
    fn refresh(&mut self, wraps: u32) {
        let sample = self.sequence.as_ref().and_then(|s| s.sample(self.time));
        match &sample {
            Some(s) => {
                if wraps > 0 || self.last_shot != Some(s.shot) {
                    self.cuts += u64::from(wraps.max(1));
                    self.pending_cut = true;
                }
                self.last_shot = Some(s.shot);
            }
            None => self.last_shot = None,
        }
        self.sample = sample;
        self.dirty = true;
    }

    /// `true` once after each cut, for whoever tells the renderer.
    pub fn take_pending_cut(&mut self) -> bool {
        std::mem::take(&mut self.pending_cut)
    }
}

/// The camera that wins when no entity is named: highest `order`, an `order` tie to the
/// earliest-spawned camera ([`SpawnOrder`], ADR-1347 — entity ids come from parallel `Commands`
/// and are not repeatable).
pub(crate) fn pick_camera(
    cameras: impl Iterator<Item = (Entity, i32)>,
    order: Option<&SpawnOrder<Camera>>,
) -> Option<Entity> {
    cameras
        .min_by_key(|&(entity, cam_order)| {
            (
                std::cmp::Reverse(cam_order),
                order.map_or((u64::MAX, entity.index().index(), entity.to_bits()), |o| {
                    o.key(entity)
                }),
            )
        })
        .map(|(entity, _)| entity)
}

/// The camera a shot player drives: the pose, the lens, and whether it is marked cinematic (so it
/// feels shakes, ADR-1619).
type DrivenCamera = (
    Entity,
    &'static mut Transform,
    &'static mut Camera,
    Option<&'static mut PhysicalCamera>,
    Has<CinematicCamera>,
);

/// Advances the clock and writes the sampled pose, lens and depth of field to the camera.
#[tracing::instrument(skip_all, target = "regia_framework", name = "shots_drive_camera")]
fn drive_camera(
    time: Res<Time>,
    origin: Option<Res<regia_scene::streaming::OriginOffset>>,
    mut player: ResMut<ShotPlayer>,
    renderer: Option<ResMut<RuntimeRenderer>>,
    mut commands: Commands,
    mut cameras: Query<DrivenCamera>,
    camera_order: Option<Res<SpawnOrder<Camera>>>,
) {
    player.advance(f64::from(time.delta_secs()));
    // A stopped clock still holds its shot on the top camera (`camera()`: "`None` picks the top
    // camera each frame"): with no camera named, a camera that now outranks the driven one (a
    // higher order spawned, or the driven camera despawned) gets a drive frame although the
    // clock stands still. Before this, a paused player only re-picked when something else made
    // it dirty, so the shot stayed on the old camera (integfix-misc-reds, ADR-5959).
    if !player.dirty && player.camera.is_none() && player.sample.is_some() {
        let top = pick_camera(
            cameras.iter().map(|(e, _, c, ..)| (e, c.order)),
            camera_order.as_deref(),
        );
        if top.is_some() && top != player.driven {
            player.dirty = true;
        }
    }
    if !player.dirty {
        return;
    }
    // The explicitly named camera wins; with none named the player re-picks the top camera every
    // frame it drives (framework3 review medium 3, ADR-5416: the documented contract, and the
    // spawn-order tiebreak makes each re-pick deterministic across runs). The release frame after
    // `clear` (no sample) lets go of the camera that was driven, not whichever camera is on top now
    // (corr-framework-b F6).
    let target = player
        .camera
        .filter(|&e| cameras.contains(e))
        .or_else(|| {
            player
                .sample
                .is_none()
                .then_some(player.kept)
                .flatten()
                .filter(|&e| cameras.contains(e))
        })
        .or_else(|| {
            pick_camera(
                cameras.iter().map(|(e, _, c, ..)| (e, c.order)),
                camera_order.as_deref(),
            )
        });
    let Some(entity) = target else { return };
    let Ok((_, mut transform, mut camera, lens, cinematic)) = cameras.get_mut(entity) else {
        return;
    };
    player.dirty = false;
    if player.sample.is_some() {
        player.driven = Some(entity);
        player.kept = Some(entity);
    }

    if player.take_pending_cut()
        && let Some(mut renderer) = renderer
    {
        renderer.0.camera_cut_with(CutExposure::Snap);
    }

    let Some(sample) = player.sample.clone() else {
        // Cleared while a sequence's depth of field was on: switch it off once.
        if player.dof_applied {
            release_lens(&mut player, lens, entity, &mut commands);
        }
        // Cleared: the camera stops feeling shakes as a cinematic camera. The shake it carried
        // was already taken off this frame (`ShakeRestoreSet`, before the rigs; ADR-1619).
        if cinematic {
            commands.entity(entity).try_remove::<CinematicCamera>();
        }
        return;
    };
    // The camera feels shakes from the next frame on (ADR-1614, ADR-1619).
    if !cinematic {
        commands.entity(entity).try_insert(CinematicCamera);
    }
    // A non-finite pose (a sequence built in code past `ShotSequence::validate`) is not written:
    // the camera keeps its last pose rather than turning NaN (ADR-1660).
    if sample.eye.is_finite() && sample.rotation.is_finite() {
        // Shot lists are authored in world meters; the camera is local (spec 78 § 12.2).
        let offset = origin.as_deref().map_or(regia_core::math::Vec3::ZERO, |o| {
            regia_core::math::Vec3::from_array(o.0.map(|axis| axis as f32))
        });
        transform.translation = sample.eye - offset;
        transform.rotation = sample.rotation;
    }
    camera.fov_y = sample.fov_y_degrees.to_radians();

    match (sample.dof, lens) {
        (Some(dof), Some(mut lens)) => {
            lens.depth_of_field = Some(mode_of(dof.bokeh));
            lens.focal_distance = dof.focus_distance;
            lens.aperture_f_stops = dof.aperture;
            player.dof_applied = true;
        }
        (Some(dof), None) => {
            // No motion blur: the sequence only asked for depth of field.
            commands.entity(entity).insert(PhysicalCamera {
                motion_blur_samples: 0,
                depth_of_field: Some(mode_of(dof.bokeh)),
                focal_distance: dof.focus_distance,
                aperture_f_stops: dof.aperture,
                ..PhysicalCamera::default()
            });
            player.dof_applied = true;
            player.lens_inserted = true;
        }
        (None, lens @ Some(_)) => {
            if player.dof_applied {
                release_lens(&mut player, lens, entity, &mut commands);
            }
        }
        (None, None) => {
            player.dof_applied = false;
            player.lens_inserted = false;
        }
    }
}

/// Ends the sequence's depth of field: a lens the player inserted is removed (the camera had
/// none before), a lens the camera had keeps everything but its depth of field.
fn release_lens(
    player: &mut ShotPlayer,
    lens: Option<Mut<PhysicalCamera>>,
    entity: Entity,
    commands: &mut Commands,
) {
    if player.lens_inserted {
        commands.entity(entity).try_remove::<PhysicalCamera>();
    } else if let Some(mut lens) = lens {
        lens.depth_of_field = None;
    }
    player.dof_applied = false;
    player.lens_inserted = false;
}

/// The fade layer's state for a sample (`None`: nothing loaded, so no fade).
#[must_use]
pub fn fade_state(sample: Option<&ShotSample>) -> FadeState {
    sample.map_or(FadeState::NONE, |s| FadeState {
        amount: s.fade,
        color: s.fade_color,
    })
}

/// The handle of the compositor fade layer the shot player installed (present once a sequence has
/// shown a fade). Hosts can read the amount actually being drawn with `.0.get()`.
#[derive(Resource, Clone, Debug)]
pub struct ShotFadeLayer(pub FadeHandle);

/// Keeps the compositor's fade layer at the player's current fade. The layer is installed on the
/// first visible fade, so an app that never fades gets no extra overlay layer.
#[tracing::instrument(skip_all, target = "regia_framework", name = "shots_drive_fade")]
fn drive_fade(
    player: Res<ShotPlayer>,
    cinematic: Option<Res<crate::sequencer::CinematicFade>>,
    overlay: Option<ResMut<WindowOverlay>>,
    layer: Option<Res<ShotFadeLayer>>,
    mut commands: Commands,
) {
    // The shot player's own sequence wins; with none loaded, a sequence takeover's fade shows.
    let state = match (player.sample(), cinematic) {
        (Some(sample), _) => fade_state(Some(sample)),
        (None, Some(cinematic)) => cinematic.0,
        (None, None) => FadeState::NONE,
    };
    let handle = match layer {
        Some(layer) => layer.0.clone(),
        None => {
            if !state.is_visible() {
                return;
            }
            let Some(mut overlay) = overlay else { return };
            let handle = install_fade(&mut overlay);
            commands.insert_resource(ShotFadeLayer(handle.clone()));
            handle
        }
    };
    if handle.get() != state {
        handle.set(state);
    }
}

fn mode_of(bokeh: bool) -> DepthOfFieldMode {
    if bokeh {
        DepthOfFieldMode::Bokeh
    } else {
        DepthOfFieldMode::Gaussian
    }
}

/// Adds the [`ShotPlayer`] resource and the system that drives the camera from it, in
/// `PostUpdate` after the camera rigs, before the director's shakes ([`CameraSet::Cinematic`]) and
/// before transform propagation.
///
/// Do not put a [`CameraRig`](crate::camera::CameraRig) on the camera a sequence drives: the rig
/// and the shot would fight over the pose each frame. A sequence player's takeover
/// (`crate::sequencer::SequencePlayer`) blends a rigged camera into shots and back instead.
/// `AgentPlugin` adds this plugin's systems itself
/// ([`install`]), so `render.sequence` works in any agent-enabled app.
pub struct ShotPlugin;

impl Plugin for ShotPlugin {
    fn build(&self, app: &mut App) {
        install(app);
    }
}

/// Idempotent form of [`ShotPlugin`]: does nothing when the player is already installed.
pub fn install(app: &mut App) {
    if app.world().contains_resource::<ShotPlayer>() {
        return;
    }
    // Order ties between cameras by spawn arrival, not entity id (ADR-1347,
    // framework3 determinism audit).
    regia_ecs::spawn_order::track_spawn_order::<Camera>(app.world_mut());
    app.init_resource::<ShotPlayer>().add_systems(
        PostUpdate,
        drive_fade
            .after(drive_camera)
            .after(CameraSet::Cinematic)
            .before(PostUpdateSet::TransformPropagate),
    );
    // Before the director, so its shakes land on the shot pose (ADR-1614; before that fix the
    // shot pose overwrote every shake).
    app.add_systems(
        PostUpdate,
        drive_camera
            .in_set(ShotDriveSet)
            .in_set(CameraSet::Cinematic)
            .after(CameraSet::Update)
            .before(CameraSet::Director)
            .before(PostUpdateSet::TransformPropagate),
    );
}

/// Most loop crossings one step counts as cuts (the sequencer replays the same number of loops).
const MAX_LOOP_CUTS: u32 = 8;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_core::math::Vec3;
    use regia_ecs::ecs::world::CommandQueue;

    use super::super::asset::fixtures::{sequence, shot};
    use super::super::asset::{DepthOfFieldTracks, LookAt};
    use super::*;

    fn player(shots: Vec<super::super::asset::Shot>) -> ShotPlayer {
        let mut player = ShotPlayer::default();
        player.load(sequence(shots));
        player
    }

    /// w8 (ADR-8940): `load_count` counts loads and clears, not edits, so a tool holding the
    /// list as a document can tell another list put in its place from an edit of its own.
    #[test]
    fn loads_count_loads_and_clears_not_edits() {
        let mut p = ShotPlayer::default();
        assert_eq!(p.load_count(), 0);
        p.load(sequence(vec![shot("a", 2.0)]));
        assert_eq!(p.load_count(), 1);
        p.replace_sequence(sequence(vec![shot("a", 3.0)]));
        assert_eq!(p.load_count(), 1, "an edit is not a load");
        p.clear();
        assert_eq!(p.load_count(), 2, "a clear counts and survives the reset");
        p.load(sequence(vec![shot("b", 1.0)]));
        assert_eq!(p.load_count(), 3);
    }

    #[test]
    fn the_clock_runs_only_while_playing_and_stops_at_the_end() {
        let mut p = player(vec![shot("a", 2.0)]);
        p.advance(1.0);
        assert_eq!(p.time(), 0.0, "stopped clocks do not advance");
        p.play();
        p.advance(0.5);
        p.advance(f64::NAN);
        p.advance(-3.0);
        assert_eq!(p.time(), 0.5, "bad steps are ignored");
        p.advance(5.0);
        assert_eq!(p.time(), 2.0);
        assert!(p.is_finished() && !p.is_playing());
        assert!(p.sample().expect("sample").finished);
        p.play();
        assert_eq!(p.time(), 0.0, "play from the end restarts");
        assert!(p.is_playing());
    }

    #[test]
    fn a_shot_change_is_a_cut_and_a_seek_back_is_a_cut() {
        let mut p = player(vec![shot("a", 2.0), shot("b", 2.0)]);
        assert_eq!(p.cuts(), 1, "the first frame is a cut");
        assert!(p.take_pending_cut());
        assert!(!p.take_pending_cut(), "reported once");
        p.play();
        p.advance(1.0);
        assert_eq!(p.cuts(), 1, "inside a shot");
        p.advance(1.0);
        assert_eq!(p.sample().expect("sample").shot, 1);
        assert_eq!(p.cuts(), 2, "crossing the boundary");
        p.seek(0.5);
        assert_eq!(p.cuts(), 3, "seeking into another shot");
        p.seek(0.75);
        assert_eq!(p.cuts(), 3, "seeking within a shot");
    }

    #[test]
    fn looping_wraps_and_cuts() {
        let mut p = player(vec![shot("a", 2.0)]);
        p.set_looping(true);
        p.play();
        p.take_pending_cut();
        p.advance(2.5);
        assert!((p.time() - 0.5).abs() < 1.0e-12);
        assert!(p.is_playing());
        assert!(p.take_pending_cut(), "a wrap on one shot is still a cut");
    }

    #[test]
    fn a_stall_across_several_loops_counts_every_crossing() {
        let mut p = player(vec![shot("a", 2.0)]);
        p.set_looping(true);
        p.play();
        let before = p.cuts();
        p.advance(4.5);
        assert!((p.time() - 0.5).abs() < 1.0e-12);
        assert_eq!(p.cuts(), before + 2, "two loops crossed, two cuts");
        p.advance(2.0 * 100.0);
        assert_eq!(p.cuts(), before + 2 + 8, "capped at the sequencer's eight loops");
    }

    #[test]
    fn seeking_and_replacing_clamp_into_range() {
        let mut p = player(vec![shot("a", 2.0), shot("b", 2.0)]);
        p.seek(100.0);
        assert_eq!(p.time(), 4.0);
        p.seek(f64::NAN);
        assert_eq!(p.time(), 0.0);
        p.seek_frame(75);
        assert_eq!(p.time(), 2.5, "30 fps");
        p.seek(3.5);
        p.replace_sequence(sequence(vec![shot("a", 2.0)]));
        assert_eq!(p.time(), 2.0, "clamped to the shorter sequence");
        p.clear();
        assert!(p.sequence().is_none() && p.sample().is_none());
        p.seek(1.0);
        p.play();
        assert!(!p.is_playing(), "nothing to play");
    }

    #[test]
    fn identical_runs_give_identical_samples() {
        let run = || {
            let mut p = player(vec![shot("a", 2.0), shot("b", 1.0)]);
            p.play();
            let mut out = Vec::new();
            for _ in 0..200 {
                p.advance(1.0 / 60.0);
                out.push(p.sample().cloned());
            }
            out
        };
        assert_eq!(run(), run());
    }

    fn app_with_camera(step: Duration) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShotPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(step));
        let camera = app
            .world_mut()
            .spawn((Transform::default(), Camera::default()))
            .id();
        (app, camera)
    }

    #[test]
    fn the_system_moves_the_camera_to_the_sampled_pose() {
        let (mut app, camera) = app_with_camera(Duration::from_millis(250));
        let mut seq = sequence(vec![shot("a", 4.0)]);
        seq.shots[0].camera.fov = super::super::asset::Track(vec![super::super::asset::ScalarKey {
            time: 0.0,
            value: 60.0,
            ease: super::super::asset::Ease::Linear,
        }]);
        app.world_mut().resource_mut::<ShotPlayer>().load(seq.clone());
        app.update();
        {
            let pose = app.world().get::<Transform>(camera).expect("transform");
            assert_eq!(
                pose.translation,
                Vec3::ZERO,
                "paused at time zero, first frame applied"
            );
            let fov = app.world().get::<Camera>(camera).expect("camera").fov_y;
            assert!((fov - 60.0f32.to_radians()).abs() < 1.0e-6);
        }
        app.world_mut().resource_mut::<ShotPlayer>().play();
        for _ in 0..4 {
            app.update();
        }
        let time = app.world().resource::<ShotPlayer>().time();
        assert!(time > 0.0, "the clock advanced with game time");
        let expected = seq.sample(time).expect("sample");
        let pose = app.world().get::<Transform>(camera).expect("transform");
        assert_eq!(pose.translation, expected.eye);
        assert_eq!(pose.rotation, expected.rotation);
    }

    /// Spec 78 § 12.2: the authored eye is in world meters and the camera is local.
    #[test]
    fn a_rebase_keeps_the_shot_in_world_meters() {
        let (mut app, camera) = app_with_camera(Duration::from_millis(250));
        app.insert_resource(regia_scene::streaming::OriginOffset([1024.0, 0.0, -2048.0]));
        let seq = sequence(vec![shot("a", 4.0)]);
        app.world_mut().resource_mut::<ShotPlayer>().load(seq.clone());
        app.update();
        let eye = seq.sample(0.0).expect("sample").eye;
        let pose = app.world().get::<Transform>(camera).expect("transform");
        assert_eq!(pose.translation, eye - Vec3::new(1024.0, 0.0, -2048.0));
    }

    #[test]
    fn a_non_finite_sample_never_reaches_the_camera_transform() {
        // A sequence built in code skips `validate`: its Catmull-Rom phantom point overflows and
        // the sample turns NaN mid-segment. The camera keeps a finite pose (ADR-1660).
        let (mut app, camera) = app_with_camera(Duration::from_millis(250));
        let mut seq = sequence(vec![shot("a", 1.0)]);
        let path = &mut seq.shots[0].camera.path;
        path.interp = super::super::asset::PathInterp::CatmullRom;
        path.keys[0].position = [2.0e38, 0.0, 0.0];
        path.keys[1].position = [0.0, 0.0, 0.0];
        assert!(
            !seq.sample(0.5).expect("sample").eye.is_finite(),
            "the unvalidated path does overflow"
        );
        app.world_mut().resource_mut::<ShotPlayer>().load(seq);
        app.world_mut().resource_mut::<ShotPlayer>().play();
        for _ in 0..3 {
            app.update();
            let pose = app.world().get::<Transform>(camera).expect("transform");
            assert!(
                pose.translation.is_finite() && pose.rotation.is_finite(),
                "{pose:?}"
            );
        }
    }

    #[test]
    fn the_system_sets_and_clears_depth_of_field_without_motion_blur() {
        let (mut app, camera) = app_with_camera(Duration::from_millis(100));
        let mut seq = sequence(vec![shot("a", 4.0)]);
        seq.shots[0].camera.path.look_at = LookAt::Point([5.0, 0.0, 0.0]);
        seq.shots[0].camera.dof = Some(DepthOfFieldTracks {
            focus_on_target: true,
            ..DepthOfFieldTracks::default()
        });
        app.world_mut().resource_mut::<ShotPlayer>().load(seq);
        app.update();
        app.update(); // the lens is inserted through commands
        let lens = *app.world().get::<PhysicalCamera>(camera).expect("lens inserted");
        assert_eq!(lens.depth_of_field, Some(DepthOfFieldMode::Gaussian));
        assert_eq!(lens.motion_blur_samples, 0);
        assert!(
            (lens.focal_distance - 5.0).abs() < 1.0e-4,
            "{}",
            lens.focal_distance
        );

        app.world_mut()
            .resource_mut::<ShotPlayer>()
            .replace_sequence(sequence(vec![shot("a", 4.0)]));
        app.update();
        // audit-seq (High): the lens the player inserted used to stay (depth of field off),
        // and its zero motion-blur samples kept the host's motion blur off for the session.
        assert!(
            app.world().get::<PhysicalCamera>(camera).is_none(),
            "the new shot has no depth of field: the inserted lens is gone"
        );
    }

    /// The inserted lens also goes when the player is cleared mid-shot, and a camera that had
    /// its own lens keeps it (only the depth of field is switched off).
    #[test]
    fn an_inserted_lens_is_removed_on_clear_and_an_own_lens_is_kept() {
        let dof_sequence = || {
            let mut seq = sequence(vec![shot("a", 4.0)]);
            seq.shots[0].camera.path.look_at = LookAt::Point([5.0, 0.0, 0.0]);
            seq.shots[0].camera.dof = Some(DepthOfFieldTracks {
                focus_on_target: true,
                ..DepthOfFieldTracks::default()
            });
            seq
        };
        let (mut app, camera) = app_with_camera(Duration::from_millis(100));
        app.world_mut().resource_mut::<ShotPlayer>().load(dof_sequence());
        app.update();
        app.update();
        assert!(app.world().get::<PhysicalCamera>(camera).is_some(), "inserted");
        app.world_mut().resource_mut::<ShotPlayer>().clear();
        app.update();
        assert!(
            app.world().get::<PhysicalCamera>(camera).is_none(),
            "removed on clear"
        );

        let (mut app, camera) = app_with_camera(Duration::from_millis(100));
        let own = PhysicalCamera {
            motion_blur_samples: 4,
            ..PhysicalCamera::default()
        };
        app.world_mut().entity_mut(camera).insert(own);
        app.world_mut().resource_mut::<ShotPlayer>().load(dof_sequence());
        app.update();
        app.update();
        app.world_mut().resource_mut::<ShotPlayer>().clear();
        app.update();
        let kept = *app.world().get::<PhysicalCamera>(camera).expect("own lens kept");
        assert_eq!(kept.depth_of_field, None);
        assert_eq!(kept.motion_blur_samples, 4);
    }

    /// Two order-0 cameras with ids reserved in the opposite order of spawn arrival: the shot
    /// drives the earlier-spawned camera on every run, whatever the entity ids (framework3-audit
    /// finding 3).
    #[test]
    fn equal_order_cameras_keep_the_earlier_spawn() {
        for b_reserved_first in [true, false] {
            let (mut app, fixture_cam) = app_with_camera(Duration::from_millis(100));
            // Only the two queued cameras compete: drop the fixture camera.
            app.world_mut().despawn(fixture_cam);
            let mut seq = sequence(vec![shot("a", 4.0)]);
            seq.shots[0].camera.path.keys[0].position = [1.0, 2.0, 3.0];
            app.world_mut().resource_mut::<ShotPlayer>().load(seq);
            let camera = || (Transform::default(), Camera::default());
            let mut first = CommandQueue::default();
            let mut second = CommandQueue::default();
            let (earlier, later) = if b_reserved_first {
                let later = Commands::new(&mut second, app.world()).spawn(camera()).id();
                let earlier = Commands::new(&mut first, app.world()).spawn(camera()).id();
                (earlier, later)
            } else {
                let earlier = Commands::new(&mut first, app.world()).spawn(camera()).id();
                let later = Commands::new(&mut second, app.world()).spawn(camera()).id();
                (earlier, later)
            };
            first.apply(app.world_mut());
            second.apply(app.world_mut());
            app.update();
            assert_eq!(
                app.world().resource::<ShotPlayer>().driven_camera(),
                Some(earlier),
                "the earlier spawn wins the order tie (b_reserved_first: {b_reserved_first})"
            );
            assert_eq!(
                app.world()
                    .get::<Transform>(earlier)
                    .expect("earlier")
                    .translation,
                Vec3::new(1.0, 2.0, 3.0),
                "the pose is written onto the earlier-spawned camera"
            );
            assert_eq!(
                app.world().get::<Transform>(later).expect("later").translation,
                Vec3::ZERO,
                "the other camera keeps its pose"
            );
        }
    }

    #[test]
    fn naming_no_camera_again_returns_to_the_top_order_camera() {
        // `set_camera(None)` documents "the camera with the highest order": it must not stay on
        // the camera that was named before, and a newly loaded sequence picks again.
        let (mut app, low) = app_with_camera(Duration::from_millis(100));
        let high = app
            .world_mut()
            .spawn((
                Transform::default(),
                Camera {
                    order: 5,
                    ..Camera::default()
                },
            ))
            .id();
        let seq = sequence(vec![shot("a", 4.0)]);
        app.world_mut().resource_mut::<ShotPlayer>().load(seq.clone());
        app.world_mut().resource_mut::<ShotPlayer>().set_camera(Some(low));
        app.update();
        assert_eq!(app.world().resource::<ShotPlayer>().driven_camera(), Some(low));
        app.world_mut().resource_mut::<ShotPlayer>().set_camera(None);
        app.update();
        assert_eq!(app.world().resource::<ShotPlayer>().driven_camera(), Some(high));
        // A camera of higher order arrives; a new sequence picks it.
        let top = app
            .world_mut()
            .spawn((
                Transform::default(),
                Camera {
                    order: 9,
                    ..Camera::default()
                },
            ))
            .id();
        app.update();
        // A paused player with no camera named re-picks the top camera when one outranks the
        // driven camera (`camera()`: "`None` picks the top camera each frame"; ADR-5959). integ-v048c
        // had pinned the old behavior (no drive frame while paused, f4c70af68); integ-v049 takes
        // integfix-misc-reds' fix and this test's original expectation back (ADR-5993).
        assert_eq!(
            app.world().resource::<ShotPlayer>().driven_camera(),
            Some(top),
            "with no camera named the top camera takes the shot"
        );
        app.world_mut().resource_mut::<ShotPlayer>().load(seq);
        app.update();
        assert_eq!(app.world().resource::<ShotPlayer>().driven_camera(), Some(top));
    }

    /// A paused shot whose driven camera is despawned moves to the next top camera on the next
    /// frame instead of pointing at a dead entity (ADR-5959).
    #[test]
    fn a_paused_shot_moves_on_when_its_camera_is_despawned() {
        let (mut app, low) = app_with_camera(Duration::from_millis(100));
        let high = app
            .world_mut()
            .spawn((
                Transform::default(),
                Camera {
                    order: 5,
                    ..Camera::default()
                },
            ))
            .id();
        let mut seq = sequence(vec![shot("a", 4.0)]);
        seq.shots[0].camera.path.keys[0].position = [1.0, 2.0, 3.0];
        app.world_mut().resource_mut::<ShotPlayer>().load(seq.clone());
        app.update();
        assert_eq!(app.world().resource::<ShotPlayer>().driven_camera(), Some(high));
        app.world_mut().despawn(high);
        app.update();
        assert_eq!(
            app.world().resource::<ShotPlayer>().driven_camera(),
            Some(low),
            "the remaining top camera takes the paused shot"
        );
        let expected = seq.sample(0.0).expect("sample").eye;
        let eye = app.world().get::<Transform>(low).expect("low").translation;
        assert!(eye.distance(expected) < 1.0e-4, "{eye} vs {expected}");
        // Nothing changes: no further drive frame (the player stays clean).
        app.update();
        assert_eq!(app.world().resource::<ShotPlayer>().driven_camera(), Some(low));
    }

    #[test]
    fn clearing_releases_the_camera_that_was_driven() {
        let (mut app, low) = app_with_camera(Duration::from_millis(100));
        app.world_mut().spawn((
            Transform::default(),
            Camera {
                order: 5,
                ..Camera::default()
            },
        ));
        app.world_mut().resource_mut::<ShotPlayer>().set_camera(Some(low));
        app.world_mut()
            .resource_mut::<ShotPlayer>()
            .load(sequence(vec![shot("a", 4.0)]));
        app.update();
        app.update();
        assert!(app.world().get::<CinematicCamera>(low).is_some());
        app.world_mut().resource_mut::<ShotPlayer>().set_camera(None);
        app.world_mut().resource_mut::<ShotPlayer>().clear();
        app.update();
        assert!(
            app.world().get::<CinematicCamera>(low).is_none(),
            "the release frame lets go of the driven camera, not the top-order one"
        );
    }

    #[test]
    fn the_top_order_camera_is_driven_by_default() {
        let (mut app, low) = app_with_camera(Duration::from_millis(100));
        let high = app
            .world_mut()
            .spawn((
                Transform::default(),
                Camera {
                    order: 5,
                    ..Camera::default()
                },
            ))
            .id();
        let mut seq = sequence(vec![shot("a", 4.0)]);
        seq.shots[0].camera.path.keys[0].position = [1.0, 2.0, 3.0];
        app.world_mut().resource_mut::<ShotPlayer>().load(seq);
        app.update();
        assert_eq!(
            app.world().get::<Transform>(high).expect("high").translation,
            Vec3::new(1.0, 2.0, 3.0)
        );
        assert_eq!(
            app.world().get::<Transform>(low).expect("low").translation,
            Vec3::ZERO
        );
        // Naming the other camera redirects it.
        app.world_mut().resource_mut::<ShotPlayer>().set_camera(Some(low));
        app.update();
        assert_eq!(
            app.world().get::<Transform>(low).expect("low").translation,
            Vec3::new(1.0, 2.0, 3.0)
        );
    }

    /// framework3 review medium 3: with no named camera the player re-picks the top camera each
    /// frame it drives — a camera that appears above the driven one mid-shot takes the shot over
    /// (the documented contract), and `set_camera(Some(_))` followed by `set_camera(None)`
    /// re-picks the top camera instead of sticking with the previously driven or named one.
    #[test]
    fn the_top_camera_hands_over_when_a_higher_order_camera_appears() {
        let (mut app, low) = app_with_camera(Duration::from_millis(100));
        let mut seq = sequence(vec![shot("a", 4.0)]);
        seq.shots[0].camera.path.keys[0].position = [1.0, 2.0, 3.0];
        app.world_mut().resource_mut::<ShotPlayer>().load(seq.clone());
        app.world_mut().resource_mut::<ShotPlayer>().play();
        app.update();
        assert_eq!(
            app.world().resource::<ShotPlayer>().driven_camera(),
            Some(low),
            "the order-0 fixture camera drives first"
        );
        // The host spawns a camera above the driven one mid-shot.
        let high = app
            .world_mut()
            .spawn((
                Transform::default(),
                Camera {
                    order: 5,
                    ..Camera::default()
                },
            ))
            .id();
        app.update();
        assert_eq!(
            app.world().resource::<ShotPlayer>().driven_camera(),
            Some(high),
            "the documented top-camera contract hands the shot over"
        );
        let time = app.world().resource::<ShotPlayer>().time();
        let expected = seq.sample(time).expect("sample").eye;
        let eye = app.world().get::<Transform>(high).expect("high").translation;
        assert!(
            eye.distance(expected) < 1.0e-4,
            "the pose is written onto the new top camera: {eye} vs {expected}"
        );
        // Naming another camera redirects it; naming none re-picks the top camera.
        app.world_mut().resource_mut::<ShotPlayer>().set_camera(Some(low));
        app.update();
        assert_eq!(
            app.world().resource::<ShotPlayer>().driven_camera(),
            Some(low),
            "the named camera wins"
        );
        app.world_mut().resource_mut::<ShotPlayer>().set_camera(None);
        app.update();
        assert_eq!(
            app.world().resource::<ShotPlayer>().driven_camera(),
            Some(high),
            "set_camera(None) re-picks the top camera, not the sticky one"
        );
    }

    #[test]
    fn a_director_shake_shows_on_top_of_a_playing_shot() {
        // Before ADR-1614 the shot pose was written after the director and every shake was lost.
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, crate::camera::CameraPlugin, ShotPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        let camera = app
            .world_mut()
            .spawn((Transform::default(), Camera::default()))
            .id();
        let seq = sequence(vec![shot("a", 8.0)]);
        {
            let mut player = app.world_mut().resource_mut::<ShotPlayer>();
            player.load(seq.clone());
            player.play();
        }
        app.update();
        app.update();
        assert!(
            app.world().get::<CinematicCamera>(camera).is_some(),
            "marked for the director"
        );
        app.world_mut()
            .resource_mut::<crate::camera::CameraDirector>()
            .shake(crate::camera::Shake {
                amplitude: 0.5,
                duration_secs: 0.4,
                ..crate::camera::Shake::default()
            });
        let mut largest = 0.0f32;
        for _ in 0..10 {
            app.update();
            let time = app.world().resource::<ShotPlayer>().time();
            let expected = seq.sample(time).expect("sample").eye;
            let eye = app.world().get::<Transform>(camera).expect("camera").translation;
            largest = largest.max(eye.distance(expected));
        }
        assert!(largest > 0.01, "the shake is visible during the shot: {largest}");
        for _ in 0..40 {
            app.update();
        }
        let time = app.world().resource::<ShotPlayer>().time();
        let expected = seq.sample(time).expect("sample");
        let pose = app.world().get::<Transform>(camera).expect("camera");
        assert!(
            pose.translation.distance(expected.eye) < 1.0e-5,
            "no offset once the shake ends"
        );
        assert!(pose.rotation.angle_between(expected.rotation) < 1.0e-4);
    }

    #[test]
    fn clearing_lets_go_of_the_director_marker_and_its_shake() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, crate::camera::CameraPlugin, ShotPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        let camera = app
            .world_mut()
            .spawn((Transform::default(), Camera::default()))
            .id();
        let seq = sequence(vec![shot("a", 8.0)]);
        {
            let mut player = app.world_mut().resource_mut::<ShotPlayer>();
            player.load(seq.clone());
            player.play();
        }
        app.update();
        app.update();
        app.world_mut()
            .resource_mut::<crate::camera::CameraDirector>()
            .shake(crate::camera::Shake {
                amplitude: 0.5,
                duration_secs: 5.0,
                ..crate::camera::Shake::default()
            });
        for _ in 0..5 {
            app.update();
        }
        let time = app.world().resource::<ShotPlayer>().time();
        let shot_eye = seq.sample(time).expect("sample").eye;
        let shaken = app.world().get::<Transform>(camera).expect("camera").translation;
        assert!(shaken.distance(shot_eye) > 1.0e-3, "shaking");
        app.world_mut().resource_mut::<ShotPlayer>().clear();
        app.update();
        app.update();
        assert!(
            app.world().get::<CinematicCamera>(camera).is_none(),
            "the director lets it go"
        );
        let eye = app.world().get::<Transform>(camera).expect("camera").translation;
        assert!(
            eye.distance(shot_eye) < 1.0e-4,
            "the last shot pose without the shake: {eye:?}"
        );
    }

    #[test]
    fn installing_twice_is_harmless() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        install(&mut app);
        install(&mut app);
        assert!(app.world().contains_resource::<ShotPlayer>());
    }

    fn app_with_overlay(step: Duration) -> App {
        let (mut app, _) = app_with_camera(step);
        app.init_resource::<WindowOverlay>();
        app
    }

    #[test]
    fn fade_state_follows_the_sample() {
        assert_eq!(fade_state(None), FadeState::NONE);
        let mut seq = sequence(vec![shot("a", 4.0)]);
        seq.shots[0].fade_in = 2.0;
        seq.shots[0].fade_color = [0.2, 0.4, 0.6];
        let half = seq.sample(1.0).expect("sample");
        assert_eq!(
            fade_state(Some(&half)),
            FadeState {
                amount: 0.5,
                color: [0.2, 0.4, 0.6]
            }
        );
    }

    #[test]
    fn the_fade_layer_is_installed_on_the_first_fade_and_tracks_the_sample() {
        let mut app = app_with_overlay(Duration::from_millis(500));
        let mut seq = sequence(vec![shot("a", 4.0)]);
        seq.shots[0].fade_in = 2.0;
        seq.shots[0].fade_out = 1.0;
        {
            let mut player = app.world_mut().resource_mut::<ShotPlayer>();
            player.load(seq.clone());
            player.seek(4.0);
        }
        app.update();
        assert_eq!(
            app.world().resource::<WindowOverlay>().layer_names(),
            [regia_render::FADE_LAYER],
            "the end of the shot is faded out"
        );
        let handle = app.world().resource::<ShotFadeLayer>().0.clone();
        assert_eq!(handle.get().amount, 1.0);
        app.world_mut().resource_mut::<ShotPlayer>().seek(2.5);
        app.update();
        assert_eq!(
            handle.get().amount,
            0.0,
            "the middle of the shot shows the picture"
        );
        app.world_mut().resource_mut::<ShotPlayer>().seek(1.0);
        app.update();
        assert_eq!(handle.get().amount, 0.5);
        app.world_mut().resource_mut::<ShotPlayer>().clear();
        app.update();
        assert_eq!(handle.get(), FadeState::NONE, "no sequence, no fade");
    }

    #[test]
    fn a_sequence_without_fades_adds_no_layer_and_no_overlay_is_fine() {
        let mut app = app_with_overlay(Duration::from_millis(100));
        app.world_mut()
            .resource_mut::<ShotPlayer>()
            .load(sequence(vec![shot("a", 4.0)]));
        app.update();
        assert!(app.world().resource::<WindowOverlay>().layer_names().is_empty());
        assert!(app.world().get_resource::<ShotFadeLayer>().is_none());

        let (mut bare, _) = app_with_camera(Duration::from_millis(100));
        let mut seq = sequence(vec![shot("a", 4.0)]);
        seq.shots[0].fade_in = 1.0;
        bare.world_mut().resource_mut::<ShotPlayer>().load(seq);
        bare.update();
        assert!(
            bare.world().get_resource::<ShotFadeLayer>().is_none(),
            "no overlay resource: data only"
        );
    }
}
