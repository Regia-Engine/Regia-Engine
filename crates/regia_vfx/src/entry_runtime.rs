// SPDX-License-Identifier: MPL-2.0
//! ECS driver for the entry effect: an [`EntryStreak`] component on any entity, one system that
//! evaluates it from its frame counter and appends the ribbons to [`VfxTrails`].
//!
//! The time is `frame / 60`, a pure function of the frame number, so a fixed-step host renders
//! the same streak on every run. The system does nothing until the runtime VFX layer exists, so
//! the trail list cannot grow in an app that never uploads it.

use regia_app::prelude::*;
use regia_core::math::{Vec3, det_sin_cos};

use crate::entry::{EntryProfile, EntryScratch, EntryTier, EntryTrack, EntryView, entry_vertex_bound};
use crate::gpu::TrailVertex;
use crate::plugin::{VfxView, tick_vfx_scene};
use crate::runtime::{VfxGpuHandle, VfxTrails, upload_runtime_frame};

/// Fixed rate the frame counter converts to seconds at.
pub const ENTRY_FRAME_RATE: f32 = 60.0;

/// An entry effect along a track.
#[derive(Component, Debug, Clone)]
pub struct EntryStreak {
    /// The path of the entering body.
    pub track: EntryTrack,
    /// Look and physics knobs.
    pub profile: EntryProfile,
    /// Cost level; hosts pick [`EntryTier::Cheap`] on Low, Deck and Mobile.
    pub tier: EntryTier,
    /// Frames played so far; set it to seek. Advances by one per update while `playing`.
    pub frame: u32,
    /// Whether `frame` advances.
    pub playing: bool,
    /// Smallest apparent width of the head and dark body, pixels.
    pub min_px: f32,
    /// Ribbons from the last paused frame. A paused streak replays them instead of evaluating
    /// again while the frame, view, profile and track are unchanged.
    replay: StreakReplay,
}

/// Cached ribbons for one paused streak.
#[derive(Clone, Debug)]
struct StreakReplay {
    vertices: Vec<TrailVertex>,
    frame: u32,
    valid: bool,
    tier: EntryTier,
    view: EntryView,
    profile: EntryProfile,
    track_stamp: u64,
}

impl Default for StreakReplay {
    fn default() -> Self {
        Self {
            vertices: Vec::new(),
            frame: 0,
            valid: false,
            tier: EntryTier::Full,
            view: EntryView {
                camera: Vec3::ZERO,
                focal_px: 1.0,
                min_px: 1.0,
                shutter_s: 1.0 / ENTRY_FRAME_RATE,
                far_m: 1.0,
            },
            profile: EntryProfile::default(),
            track_stamp: 0,
        }
    }
}

impl EntryStreak {
    /// A streak at frame 0 with default knobs, playing.
    pub fn new(track: EntryTrack) -> Self {
        Self {
            track,
            profile: EntryProfile::default(),
            tier: EntryTier::Full,
            frame: 0,
            playing: true,
            min_px: 2.0,
            replay: StreakReplay::default(),
        }
    }

    /// Seconds into the shot (`frame / 60`).
    pub fn time_s(&self) -> f32 {
        self.frame as f32 / ENTRY_FRAME_RATE
    }
}

/// Adds the entry-streak driver.
pub struct EntryStreakPlugin;

impl Plugin for EntryStreakPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VfxTrails>().add_systems(
            Update,
            push_entry_streaks
                .after(tick_vfx_scene)
                .before(upload_runtime_frame),
        );
    }
}

fn push_entry_streaks(
    handle: Option<Res<VfxGpuHandle>>,
    time: Option<Res<Time>>,
    view: Res<VfxView>,
    mut trails: ResMut<VfxTrails>,
    mut streaks: Query<&mut EntryStreak>,
    mut scratch: Local<EntryScratch>,
) {
    if handle.is_none() {
        return;
    }
    let _span = tracing::trace_span!("entry_streaks").entered();
    let focal = focal_px(view.height, view.camera.fov_y);
    // The gameplay pause holds every streak on its frame (ADR-10105): it replays the ribbons it
    // built, as a streak with `playing` off does.
    let game_paused = time.is_some_and(|time| time.paused);
    // `Query::iter_mut` yields `Mut`, so the loop reborrows each streak in place.
    for mut streak in &mut streaks {
        append_streak(&view, focal, &mut trails, &mut streak, &mut scratch, game_paused);
    }
}

/// Focal length in pixels for a view height and vertical FOV: `height / (2 tan(fov_y / 2))`.
///
/// The tangent runs on the deterministic trig kernel ([`regia_core::math::det_sin_cos`]), so
/// the same view pins the same `focal_px` bits on every platform: a streak width one ulp from
/// an integer pixel floors the same everywhere. The clamp sits on the whole denominator (the
/// pre-rewrite behaviour): a degenerate FOV saturates at `height / 1e-6`, half of what
/// clamping the tangent alone would give (review fix-vfx2-determinism finding 5).
fn focal_px(height: u32, fov_y: f32) -> f32 {
    let (sin, cos) = det_sin_cos(0.5 * fov_y);
    height as f32 / (2.0 * (sin / cos)).max(1e-6)
}

/// Evaluates every streak at its own frame, appends the ribbons, then advances the counters.
///
/// `scratch` keeps the ion, train, piece and ribbon lists. After the first frame for a profile,
/// a later frame allocates nothing: the trail buffer is reserved to the vertex bound for that
/// profile and the lists stay inside `scratch`. A paused streak whose frame, view, profile and
/// track are unchanged replays the ribbons it already built.
pub fn step_entry_streaks<'a>(
    view: &VfxView,
    trails: &mut VfxTrails,
    streaks: impl IntoIterator<Item = &'a mut EntryStreak>,
    scratch: &mut EntryScratch,
) {
    let _span = tracing::trace_span!("entry_streaks").entered();
    let focal = focal_px(view.height, view.camera.fov_y);
    for streak in streaks {
        append_streak(view, focal, trails, streak, scratch, false);
    }
}

fn append_streak(
    view: &VfxView,
    focal: f32,
    trails: &mut VfxTrails,
    streak: &mut EntryStreak,
    scratch: &mut EntryScratch,
    game_paused: bool,
) {
    let advancing = streak.playing && !game_paused;
    let entry_view = EntryView {
        camera: view.camera.eye,
        focal_px: focal,
        min_px: streak.min_px,
        shutter_s: 1.0 / ENTRY_FRAME_RATE,
        far_m: view.camera.far,
    };
    let bound = entry_vertex_bound(&streak.profile, streak.tier);
    if trails.0.capacity() < bound {
        trails.0.reserve(bound - trails.0.len());
    }
    if replay_matches(streak, &entry_view, advancing) {
        trails.0.extend_from_slice(&streak.replay.vertices);
        return;
    }
    let start = trails.0.len();
    scratch.eval_and_append(
        &streak.track,
        &streak.profile,
        streak.tier,
        streak.time_s(),
        &entry_view,
        &mut trails.0,
    );
    if advancing {
        streak.replay.valid = false;
        streak.frame = streak.frame.saturating_add(1);
    } else {
        remember_paused(streak, &entry_view, &trails.0[start..], bound);
    }
}

fn replay_matches(streak: &EntryStreak, view: &EntryView, advancing: bool) -> bool {
    let replay = &streak.replay;
    !advancing
        && replay.valid
        && replay.frame == streak.frame
        && replay.tier == streak.tier
        && replay.view == *view
        && replay.profile == streak.profile
        && replay.track_stamp == track_stamp(&streak.track)
}

fn remember_paused(streak: &mut EntryStreak, view: &EntryView, vertices: &[TrailVertex], bound: usize) {
    if streak.replay.vertices.capacity() < bound {
        streak
            .replay
            .vertices
            .reserve(bound - streak.replay.vertices.len());
    }
    streak.replay.vertices.clear();
    streak.replay.vertices.extend_from_slice(vertices);
    streak.replay.frame = streak.frame;
    streak.replay.valid = true;
    streak.replay.tier = streak.tier;
    streak.replay.view = *view;
    streak.replay.profile = streak.profile.clone();
    streak.replay.track_stamp = track_stamp(&streak.track);
}

fn track_stamp(track: &EntryTrack) -> u64 {
    let mut z = track.points().len() as u64;
    for point in track.points() {
        z = z
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(u64::from(point.time.to_bits()));
        for channel in point.position.to_array() {
            z = z
                .wrapping_mul(0xbf58_476d_1ce4_e5b9)
                .wrapping_add(u64::from(channel.to_bits()));
        }
    }
    z
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streak() -> EntryStreak {
        let at = |time: f32, x: f32| crate::entry::TrackPoint {
            time,
            position: regia_core::math::Vec3::new(x, 5_000.0, 0.0),
        };
        let track = EntryTrack::new(vec![at(0.0, 0.0), at(10.0, 9_000.0)]).expect("two-point track");
        EntryStreak::new(track)
    }

    #[test]
    fn time_is_frame_over_sixty() {
        let mut s = streak();
        s.frame = 90;
        assert_eq!(s.time_s(), 1.5);
    }

    /// The focal length is bit-pinned across platforms (deterministic tan kernel).
    #[test]
    fn focal_px_bits_are_pinned() {
        let focal = focal_px(1080, 1.0);
        assert_eq!(focal.to_bits(), FOCAL_PINNED_BITS, "focal length bits changed");
        // The kernel tracks the platform tan closely: far inside one pixel at 1080p.
        let std_focal = 1080.0 / (2.0 * 0.5f32.tan());
        assert!((focal - std_focal).abs() < 1e-3, "{focal} vs {std_focal}");
        assert_eq!(
            focal_px(720, 0.0),
            720_000_000.0,
            "a zero FOV hits the 1e-6 clamp on the whole denominator"
        );
    }

    const FOCAL_PINNED_BITS: u32 = 1148657064; // pinned 2026-11-09 (deterministic tan kernel)

    #[test]
    fn without_the_gpu_handle_nothing_advances_or_grows() {
        let mut app = App::new();
        app.insert_resource(VfxView::default())
            .add_plugins(EntryStreakPlugin);
        let e = app.world_mut().spawn(streak()).id();
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(app.world().get::<EntryStreak>(e).map(|s| s.frame), Some(0));
        assert!(app.world().resource::<VfxTrails>().0.is_empty());
    }

    #[test]
    fn stepping_advances_playing_streaks_only_and_paused_ones_hold() {
        let mut app = App::new();
        app.insert_resource(VfxView::default())
            .init_resource::<VfxTrails>()
            .add_systems(
                Update,
                |view: Res<VfxView>,
                 mut trails: ResMut<VfxTrails>,
                 mut streaks: Query<&mut EntryStreak>,
                 mut scratch: Local<EntryScratch>| {
                    for mut streak in &mut streaks {
                        let streak: &mut EntryStreak = &mut streak;
                        step_entry_streaks(&view, &mut trails, std::iter::once(streak), &mut scratch);
                    }
                },
            );
        let playing = app.world_mut().spawn(streak()).id();
        let mut paused = streak();
        paused.playing = false;
        paused.frame = 7;
        let paused = app.world_mut().spawn(paused).id();
        for _ in 0..4 {
            app.update();
        }
        assert_eq!(app.world().get::<EntryStreak>(playing).map(|s| s.frame), Some(4));
        assert_eq!(app.world().get::<EntryStreak>(paused).map(|s| s.frame), Some(7));
        assert_eq!(
            app.world().get::<EntryStreak>(playing).map(|s| s.time_s()),
            Some(4.0 / ENTRY_FRAME_RATE)
        );
    }

    /// ADR-10105: the gameplay pause holds a playing streak on its frame, and the ribbons it shows
    /// while held are the ones of that frame, byte for byte; unpausing continues from that frame.
    /// (`push_entry_streaks` passes `Time::paused`; it needs the GPU handle, so the test drives
    /// the step it calls.)
    #[test]
    fn the_gameplay_pause_holds_a_playing_streak_and_its_ribbons() {
        let view = VfxView::default();
        let focal = focal_px(view.height, view.camera.fov_y);
        let mut trails = VfxTrails::default();
        let mut scratch = EntryScratch::default();
        let mut s = streak();
        s.frame = 120;
        let mut step = |s: &mut EntryStreak, trails: &mut VfxTrails, paused: bool| {
            trails.0.clear();
            append_streak(&view, focal, trails, s, &mut scratch, paused);
        };
        step(&mut s, &mut trails, false);
        assert_eq!(s.frame, 121);
        step(&mut s, &mut trails, true);
        let held = trail_bits(&trails);
        assert!(!held.is_empty(), "the streak draws ribbons on its frame");
        for _ in 0..300 {
            step(&mut s, &mut trails, true);
            assert_eq!(s.frame, 121, "held on its frame");
            assert_eq!(trail_bits(&trails), held, "the same ribbons");
        }
        assert!(s.playing, "the pause does not touch the streak's own switch");
        step(&mut s, &mut trails, false);
        assert_eq!(s.frame, 122, "resumes from the held frame");
    }

    fn trail_bits(trails: &VfxTrails) -> Vec<u8> {
        bytemuck::cast_slice::<TrailVertex, u8>(&trails.0).to_vec()
    }
}
